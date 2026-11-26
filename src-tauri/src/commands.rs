//! Tauri command handlers exposed to the frontend via `invoke_handler`.
//!
//! Each `#[tauri::command]` function here is registered in `app.rs` via
//! `tauri::generate_handler!`. Helper functions that are only used by these
//! commands live alongside them or in their dedicated module
//! (`permissions.rs`).

use crate::audio;
use crate::hotkey::{canonicalize_hotkey, parse_hotkey};
#[cfg(target_os = "linux")]
use crate::permissions::{permission_setup_in_directory, resolve_permission_setup_dir};
use crate::settings::{self, Settings};
use crate::shortcut::{BackendKind, ShortcutBackendStatus, ShortcutChord, ShortcutManager};
use crate::state::{AppState, StatusEvent};
use std::sync::mpsc;
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager, State};

// Serialize disk writes and hotkey replacement across settings updates.
static SETTINGS_SAVE: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // Tauri command parameters are framework-injected.
pub fn update_correction_settings(
    app: AppHandle,
    correction: settings::CorrectionSettings,
) -> Result<Settings, String> {
    let _save = SETTINGS_SAVE
        .lock()
        .map_err(|_| "settings save lock poisoned")?;
    let state = app.state::<AppState>();
    let next = state.update_correction_settings(correction, |next| settings::save(&app, next))?;
    let _ = app.emit("charon://settings-changed", ());
    Ok(next)
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // Tauri command parameters are framework-injected.
pub fn get_settings(state: State<'_, AppState>) -> Result<Settings, String> {
    state.settings_snapshot()
}

#[tauri::command]
pub async fn list_input_devices() -> Result<Vec<audio::InputDevice>, String> {
    tauri::async_runtime::spawn_blocking(audio::list_input_devices)
        .await
        .map_err(|error| format!("internal error while listing input devices: {error}"))?
}

#[tauri::command]
pub async fn check_server_url(server_url: String) -> Result<(), String> {
    crate::transcription::check_server(&server_url).await
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // Tauri command parameters are framework-injected.
pub fn get_shortcut_permission_setup(
    app: AppHandle,
) -> Result<crate::permissions::ShortcutPermissionSetup, String> {
    #[cfg(target_os = "linux")]
    {
        // The bundled resource is intentionally resolved for diagnostics, while
        // include_str remains the canonical exact content in dev and bundles.
        let _bundled_rule = app.path().resource_dir().ok().map(|directory| {
            directory
                .join("resources")
                .join(crate::permissions::SHORTCUT_RULE_NAME)
        });
        let directory = resolve_permission_setup_dir(&app);
        crate::permissions::log_permission_setup_dir(&directory);
        let directory = directory.map_err(|error| {
            let detail = format!("cannot resolve shortcut permission directory: {error}");
            crate::permissions::log_permission_setup_message(&detail);
            detail
        })?;
        let setup = permission_setup_in_directory(&directory).inspect_err(|detail| {
            crate::permissions::log_permission_setup_message(detail);
        })?;
        if let Some(path) = setup.prepared_rule_path.as_deref() {
            crate::permissions::log_permission_setup_message(&format!("prepared rule at {path}"));
        }
        Ok(setup)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = app;
        Ok(crate::permissions::permission_setup_for_path(
            None, false, None,
        ))
    }
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // Tauri command parameters are framework-injected.
pub fn get_shortcut_backend_status(
    state: State<'_, AppState>,
) -> Result<ShortcutBackendStatus, String> {
    state.shortcut_status()
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // Tauri command parameters are framework-injected.
pub fn retry_shortcut_backend(app: AppHandle) -> Result<ShortcutBackendStatus, String> {
    let app_for_retry = app.clone();
    let (sender, receiver) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("charon-shortcut-retry".into())
        .spawn(move || {
            let state = app_for_retry.state::<AppState>();
            let hotkey = state.registered_hotkey();
            let (dispatch, status_sink) = crate::hotkey::wayland_hooks(&app_for_retry);
            let result = hotkey.and_then(|hotkey| {
                state.retry_or_initialize_shortcut_manager(
                    BackendKind::WaylandHelper,
                    |manager| manager.retry(&app_for_retry).map_err(|e| e.to_string()),
                    || {
                        ShortcutManager::wayland(app_for_retry.clone(), dispatch, status_sink)
                            .map_err(|e| e.to_string())
                    },
                    |manager| {
                        let chord = hotkey
                            .parse::<ShortcutChord>()
                            .map_err(|error| error.to_string())?;
                        manager
                            .replace(&app_for_retry, chord)
                            .map_err(|e| e.to_string())
                    },
                )
            });

            let status =
                state
                    .shortcut_status()
                    .unwrap_or_else(|_| ShortcutBackendStatus::Failed {
                        backend: BackendKind::WaylandHelper,
                        detail: "shortcut lock poisoned".to_owned(),
                    });
            state.set_shortcut_status(&app_for_retry, status);
            let _ = sender.send(result);
        })
        .map_err(|error| format!("cannot start shortcut retry: {error}"))?;
    let status =
        receiver
            .recv_timeout(Duration::from_secs(10))
            .map_err(|error| match error {
                mpsc::RecvTimeoutError::Timeout => {
                    "Повторный запуск службы горячих клавиш не завершился вовремя.".to_owned()
                }
                mpsc::RecvTimeoutError::Disconnected => {
                    "Служба повторного запуска горячих клавиш остановлена.".to_owned()
                }
            })??;
    app.state::<AppState>()
        .set_shortcut_status(&app, status.clone());
    Ok(status)
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // Tauri command parameters are framework-injected.
pub fn get_status(state: State<'_, AppState>) -> Result<StatusEvent, String> {
    state.current_status()
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // Tauri command parameters are framework-injected.
pub fn set_hotkey_capture_active(
    state: State<'_, AppState>,
    active: bool,
    token: u64,
) -> Result<(), String> {
    state.set_hotkey_capture(active, token)
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // Tauri command parameters are framework-injected.
pub fn update_settings(app: AppHandle, settings: Settings) -> Result<Settings, String> {
    let _save = SETTINGS_SAVE
        .lock()
        .map_err(|_| "settings save lock poisoned")?;
    let mut next = settings;
    next.server_url = crate::settings::normalize_server_url(&next.server_url)?;
    next.hotkey = canonicalize_hotkey(&next.hotkey)?;
    next.input_device = next
        .input_device
        .take()
        .map(|device| device.trim().to_owned())
        .filter(|device| !device.is_empty());
    parse_hotkey(&next.hotkey)?;
    next.hotkey
        .parse::<ShortcutChord>()
        .map_err(|error| error.to_string())?;
    let state = app.state::<AppState>();
    // Acquire `settings` then `shortcut` (canonical lock order). Hold both
    // only long enough to snapshot the values we need; we deliberately drop
    // the guards before any blocking helper IPC.
    let (old_hotkey, current_settings) = {
        let runtime = state.settings_runtime_lock()?;
        (runtime.registered_hotkey.clone(), runtime.settings.clone())
    };
    let changed_hotkey = next.hotkey != old_hotkey;
    let reservation = if changed_hotkey {
        Some(state.reserve_idle_processing()?)
    } else {
        None
    };
    let manager_kind = state.shortcut_backend_kind()?;
    settings::preserve_correction(&mut next, &current_settings);
    let result = state.update_general_settings(
        next,
        &old_hotkey,
        manager_kind,
        |chord| {
            state.with_shortcut_manager(|manager| {
                let result = manager.replace(&app, chord);
                let status = manager.status();
                (result.map_err(|error| error.to_string()), status)
            })
        },
        |status| state.set_shortcut_status(&app, status),
        |next| settings::save(&app, next),
        || state.with_shortcut_manager(|manager| manager.status()),
    )?;
    drop(reservation);
    let _ = app.emit("charon://settings-changed", ());
    Ok(result)
}

/// Returns the pending app-exit request id, if any.
///
/// Called by the main window after registering its exit listener so a
/// request raised before (or during) subscription is not lost.
#[allow(clippy::needless_pass_by_value)] // Tauri command parameters are framework-injected.
#[tauri::command]
pub fn correction_exit_ready(app: AppHandle) -> Option<u64> {
    app.try_state::<crate::exit::ExitGate>()
        .and_then(|gate| gate.pending_id())
}

/// Applies the main window's decision on a pending app-exit request.
#[allow(clippy::needless_pass_by_value)] // Tauri command parameters are framework-injected.
#[tauri::command]
pub async fn resolve_exit_request(
    window: tauri::WebviewWindow,
    app: AppHandle,
    request_id: u64,
    decision: String,
) -> Result<(), String> {
    let approve = match decision.as_str() {
        "approve" => true,
        "cancel" => false,
        other => return Err(format!("unknown exit decision: {other}")),
    };
    let sender_label = window.label().to_owned();
    let handle = app.clone();
    let (send, receive) = tokio::sync::oneshot::channel();
    app.run_on_main_thread(move || {
        let result = crate::exit::resolve_exit(&handle, &sender_label, request_id, approve);
        let _ = send.send(result);
    })
    .map_err(|error| error.to_string())?;
    receive.await.map_err(|error| error.to_string())?
}
