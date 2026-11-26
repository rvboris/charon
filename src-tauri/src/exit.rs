//! App-exit gating: the pending-request state machine and its window dialog.
//!
//! [`ExitGate`] keeps a single pending exit request identified by a monotonic
//! id. Stale resolutions are ignored; once approved no new dialog is created.

use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager};

/// Pending app-exit request awaiting the main window's decision.
///
/// The gate keeps a single pending request identified by a monotonic id.
/// Stale resolutions are ignored; once approved no new dialog is created.
#[derive(Default)]
pub(crate) struct ExitGate {
    state: Mutex<ExitState>,
}

impl ExitGate {
    /// Current pending exit request id, if any.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn pending_id(&self) -> Option<u64> {
        self.state
            .lock()
            .expect("charon exit gate lock poisoned")
            .pending_id
    }
}

/// Pure decision table of the exit gate, unit-tested below.
#[derive(Default)]
struct ExitState {
    next_id: u64,
    pending_id: Option<u64>,
    approved: bool,
}

enum ExitAction {
    /// Ask the main window about this request id.
    Ask(u64),
    /// Nothing to ask; exit now.
    Exit,
    /// Already handled; do nothing.
    None,
}

impl ExitState {
    fn request(&mut self, main_window_open: bool) -> ExitAction {
        if self.approved {
            return ExitAction::None;
        }
        if let Some(request_id) = self.pending_id {
            return ExitAction::Ask(request_id);
        }
        if !main_window_open {
            self.approved = true;
            return ExitAction::Exit;
        }
        self.next_id += 1;
        let request_id = self.next_id;
        self.pending_id = Some(request_id);
        ExitAction::Ask(request_id)
    }

    fn resolve(&mut self, request_id: u64, approve: bool) -> bool {
        match self.pending_id {
            Some(current) if current == request_id => {
                self.pending_id = None;
                if approve {
                    self.approved = true;
                }
                approve
            }
            _ => false,
        }
    }
}

/// Asks the main window for a decision; exits immediately when absent.
///
/// Idempotent: repeated requests re-notify the same pending id.
pub(crate) fn request_exit(app: &AppHandle) {
    let Some(gate) = app.try_state::<ExitGate>() else {
        app.exit(0);
        return;
    };
    let window = app.get_webview_window("main");
    let action = {
        let mut state = gate.state.lock().expect("charon exit gate lock poisoned");
        state.request(window.is_some())
    };
    match action {
        ExitAction::Ask(request_id) => {
            if let Some(window) = &window {
                let _ = window.unminimize();
                let _ = window.show();
                let _ = window.set_focus();
            }
            if window.is_some() {
                if let Err(error) = app.emit_to("main", "charon://exit-requested", request_id) {
                    eprintln!("[charon] cannot notify main window about exit: {error}");
                }
            }
        }
        ExitAction::Exit => app.exit(0),
        ExitAction::None => {}
    }
}

/// Whether the app already decided to exit (final approval).
pub(crate) fn exit_approved(app: &AppHandle) -> bool {
    app.try_state::<ExitGate>().is_none_or(|gate| {
        gate.state
            .lock()
            .expect("charon exit gate lock poisoned")
            .approved
    })
}

/// Applies the main window's decision for a pending exit request.
///
/// Only the main window may resolve, and only the current pending id acts;
/// stale or duplicated resolutions are safe no-ops.
fn exit_sender_authorized(sender_label: &str) -> bool {
    sender_label == "main"
}

pub(crate) fn resolve_exit(
    app: &AppHandle,
    sender_label: &str,
    request_id: u64,
    approve: bool,
) -> Result<(), String> {
    if !exit_sender_authorized(sender_label) {
        return Err("exit decisions are only accepted from the main window".into());
    }
    let gate = app
        .try_state::<ExitGate>()
        .ok_or_else(|| "exit gate is unavailable".to_string())?;
    let exit_now = {
        let mut state = gate.state.lock().expect("charon exit gate lock poisoned");
        state.resolve(request_id, approve)
    };
    if exit_now {
        app.exit(0);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{exit_sender_authorized, ExitAction, ExitState};

    #[test]
    fn exits_immediately_without_main_window() {
        let mut state = ExitState::default();
        assert!(matches!(state.request(false), ExitAction::Exit));
        assert!(
            matches!(state.request(false), ExitAction::None),
            "already approved"
        );
    }

    #[test]
    fn asks_main_window_and_repeats_same_id() {
        let mut state = ExitState::default();
        let ExitAction::Ask(first) = state.request(true) else {
            panic!("expected Ask");
        };
        let ExitAction::Ask(second) = state.request(true) else {
            panic!("expected Ask");
        };
        assert_eq!(first, second, "repeated requests are idempotent");
    }

    #[test]
    fn cancel_allows_a_new_request() {
        let mut state = ExitState::default();
        let ExitAction::Ask(first) = state.request(true) else {
            panic!()
        };
        assert!(!state.resolve(first, false), "cancel does not exit");
        let ExitAction::Ask(second) = state.request(true) else {
            panic!()
        };
        assert_ne!(first, second);
    }

    #[test]
    fn stale_resolutions_are_ignored() {
        let mut state = ExitState::default();
        let ExitAction::Ask(first) = state.request(true) else {
            panic!()
        };
        assert!(!state.resolve(first.wrapping_sub(1), true));
        assert!(!state.resolve(first + 1, false));
        assert!(state.resolve(first, true), "current id approves");
        assert!(!state.resolve(first, true), "duplicate approve is a no-op");
    }

    #[test]
    fn only_main_is_authorized_to_resolve_exit() {
        assert!(exit_sender_authorized("main"));
        assert!(!exit_sender_authorized("correction-settings"));
        assert!(!exit_sender_authorized("recording-overlay"));
    }

    #[test]
    fn main_exists_while_pending_is_resolved_once() {
        let mut state = ExitState::default();
        let ExitAction::Ask(id) = state.request(true) else {
            panic!()
        };
        assert!(!state.resolve(id, false));
        let ExitAction::Ask(next) = state.request(true) else {
            panic!("expected another exit request")
        };
        assert_ne!(next, id);
    }
}
