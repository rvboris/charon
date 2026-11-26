//! Aggregate application state and the helpers that mutate it safely.
//!
//! The app keeps its mutable state behind a small number of `Mutex`es. Two of
//! those mutexes ([`AppState::settings`] and [`AppState::shortcut`]) wrap
//! *consolidated* runtime structs so that values always mutated together
//! cannot drift apart and so the lock-ordering surface is small and explicit.

use crate::audio::AudioController;
use crate::overlay::{hide_recording_overlay, manage_recording_overlay};
use crate::settings::{self, Settings, TriggerType};
use crate::shortcut::{
    BackendKind, ShortcutBackendStatus, ShortcutChord, ShortcutManager,
};
use crate::trigger::{Action, TriggerState};
use serde::Serialize;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;
use tauri::{AppHandle, Emitter, Manager};

/// Consolidated settings + registered hotkey string.
///
/// These two values are always mutated together in `update_settings`, so they
/// share a single mutex to make that atomic and to remove a lock-ordering
/// hazard.
pub(crate) struct SettingsRuntime {
    pub(crate) settings: Settings,
    pub(crate) registered_hotkey: String,
}

/// Consolidated shortcut backend runtime.
///
/// `manager` and `status` describe the same subsystem. `manager` is absent
/// while asynchronous Wayland initialization has not succeeded, and is also
/// temporarily `take`n during a serialized blocking operation. `status` remains
/// available in both cases and reflects the latest authoritative publication.
pub(crate) struct ShortcutRuntime {
    pub(crate) manager: Option<ShortcutManager>,
    pub(crate) status: ShortcutBackendStatus,
    /// Increments whenever a backend publishes status through the shared state.
    pub(crate) status_revision: u64,
}

/// Aggregate application state.
///
/// # Lock order
///
/// When multiple locks must be held simultaneously, acquire them in this order:
///   1. `SETTINGS_SAVE` (commands module)
///   2. `processing`
///   3. short `settings` / `recording` snapshots
///   4. `shortcut`, `trigger`, `status` only when their operation permits it
///
/// Never hold `processing` across `ShortcutManager::replace`: the Wayland
/// helper may synchronously dispatch a release while replacement waits for ACK.
/// Never hold `settings` while waiting for `processing`.
///
/// `shortcut_operations` is a plain `Mutex<()>` used purely as a serialization
/// primitive. Initialization, retry, replace, and shutdown manager mutations
/// hold it for their full duration. It must be acquired BEFORE `shortcut`
/// whenever both are needed; blocking helper IPC never holds `shortcut`.
/// `shortcut_stopping` is lock-free lifecycle state and may be checked at any
/// point, including while an operation owns `shortcut_operations`.
pub struct AppState {
    settings: Mutex<SettingsRuntime>,
    trigger: Mutex<TriggerState>,
    audio: AudioController,
    recording: Mutex<Option<Instant>>,
    // Serializes start/stop transitions; true blocks new jobs until output completes.
    processing: Mutex<bool>,
    /// Odd while a server check/start is pending; even otherwise.
    pub(crate) recording_start_token: AtomicU64,
    shortcut: Mutex<ShortcutRuntime>,
    shortcut_operations: Mutex<()>,
    shortcut_stopping: AtomicBool,
    /// Monotonic capture token packed as `(token << 1) | active`.
    /// A token prevents a delayed end command from disabling a newer capture.
    hotkey_capture: std::sync::atomic::AtomicU64,
    status: Mutex<StatusEvent>,
    status_revision: AtomicU64,
}

pub(crate) struct ProcessingReservation<'a> {
    processing: &'a Mutex<bool>,
}

impl Drop for ProcessingReservation<'_> {
    fn drop(&mut self) {
        if let Ok(mut processing) = self.processing.lock() {
            *processing = false;
        }
    }
}

impl AppState {
    pub(crate) fn reserve_idle_processing(&self) -> Result<ProcessingReservation<'_>, String> {
        let mut processing = self
            .processing
            .lock()
            .map_err(|_| "processing lock poisoned")?;
        let recording = self
            .recording
            .lock()
            .map_err(|_| "recording lock poisoned")?;
        if *processing
            || recording.is_some()
            || self.recording_start_token.load(Ordering::Acquire) & 1 == 1
        {
            return Err(
                "Cannot change the hotkey while recording or processing is active.".to_owned(),
            );
        }
        *processing = true;
        drop(recording);
        drop(processing);
        Ok(ProcessingReservation {
            processing: &self.processing,
        })
    }

    pub(crate) fn set_hotkey_capture(&self, active: bool, token: u64) -> Result<(), String> {
        if !active {
            self.update_hotkey_capture(active, token);
            return Ok(());
        }

        let processing = self
            .processing
            .lock()
            .map_err(|_| "processing lock poisoned")?;
        let mut current = self.hotkey_capture.load(Ordering::Acquire);
        let result = loop {
            if token < (current >> 1) {
                break Ok(());
            }
            if *processing {
                break Err("Cannot capture a hotkey while processing is active.".to_owned());
            }
            if self
                .recording
                .lock()
                .map_err(|_| "recording lock poisoned")?
                .is_some()
            {
                break Err("Cannot capture a hotkey while recording is active.".to_owned());
            }
            if self.recording_start_token.load(Ordering::Acquire) & 1 == 1 {
                break Err("Cannot capture a hotkey while recording is starting.".to_owned());
            }
            let next = (token << 1) | 1;
            match self.hotkey_capture.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break Ok(()),
                Err(observed) => current = observed,
            }
        };
        drop(processing);
        result
    }

    fn update_hotkey_capture(&self, active: bool, token: u64) {
        let next = (token << 1) | u64::from(active);
        let mut current = self.hotkey_capture.load(Ordering::Acquire);
        while token >= (current >> 1) {
            match self.hotkey_capture.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(observed) => current = observed,
            }
        }
    }

    pub(crate) fn is_hotkey_capture_active(&self) -> bool {
        self.hotkey_capture.load(Ordering::Acquire) & 1 == 1
    }

    /// Snapshots the authoritative settings.
    pub(crate) fn settings_snapshot(&self) -> Result<Settings, String> {
        self.settings
            .lock()
            .map(|runtime| runtime.settings.clone())
            .map_err(|_| "settings lock poisoned".into())
    }

    /// Snapshots the registered hotkey string.
    pub(crate) fn registered_hotkey(&self) -> Result<String, String> {
        self.settings
            .lock()
            .map(|runtime| runtime.registered_hotkey.clone())
            .map_err(|_| "settings lock poisoned".to_owned())
    }

    /// Latest published shortcut backend status.
    pub(crate) fn shortcut_status(&self) -> Result<ShortcutBackendStatus, String> {
        self.shortcut
            .lock()
            .map(|runtime| runtime.status.clone())
            .map_err(|_| "shortcut lock poisoned".into())
    }

    /// Backend kind of the latest published shortcut status.
    pub(crate) fn shortcut_backend_kind(&self) -> Result<crate::shortcut::BackendKind, String> {
        Ok(self
            .shortcut
            .lock()
            .map_err(|_| "shortcut lock poisoned".to_string())?
            .status
            .backend())
    }

    /// Latest status event snapshot.
    pub(crate) fn current_status(&self) -> Result<StatusEvent, String> {
        self.status
            .lock()
            .map(|status| status.clone())
            .map_err(|_| "status lock poisoned".into())
    }

    /// Applies a correction-settings update as one lock-held transaction.
    pub(crate) fn update_correction_settings(
        &self,
        correction: settings::CorrectionSettings,
        save: impl FnOnce(&Settings) -> Result<(), String>,
    ) -> Result<Settings, String> {
        let mut runtime = self
            .settings
            .lock()
            .map_err(|_| "settings lock poisoned")?;
        update_correction_settings_with(&mut runtime, correction, save)
    }

    /// Applies a general settings update as one serialized transaction.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn update_general_settings(
        &self,
        next: Settings,
        old_hotkey: &str,
        manager_kind: BackendKind,
        replace: impl FnMut(
            ShortcutChord,
        ) -> Result<(Result<(), String>, ShortcutBackendStatus), String>,
        publish_status: impl FnMut(ShortcutBackendStatus),
        save: impl FnOnce(&Settings) -> Result<(), String>,
        manager_status: impl FnOnce() -> Result<ShortcutBackendStatus, String>,
    ) -> Result<Settings, String> {
        update_general_settings_with(
            &self.settings,
            next,
            old_hotkey,
            manager_kind,
            replace,
            publish_status,
            save,
            manager_status,
        )
    }

    /// Marks the shortcut subsystem as shutting down.
    pub(crate) fn begin_shortcut_shutdown(&self) {
        self.shortcut_stopping.store(true, Ordering::Release);
    }

    /// Whether shortcut shutdown has started.
    pub(crate) fn is_shortcut_stopping(&self) -> bool {
        self.shortcut_stopping.load(Ordering::Acquire)
    }

    /// Locks the settings runtime; `Err` means the lock is poisoned.
    pub(crate) fn settings_runtime_lock(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, SettingsRuntime>, String> {
        self.settings.lock().map_err(|_| "settings lock poisoned".to_owned())
    }

    /// Locks the processing flag; `None` means the lock is poisoned.
    pub(crate) fn processing_guard(&self) -> Option<std::sync::MutexGuard<'_, bool>> {
        self.processing.lock().ok()
    }

    /// Releases an exclusive processing job.
    pub(crate) fn finish_processing(&self) {
        if let Ok(mut processing) = self.processing.lock() {
            *processing = false;
        }
    }

    pub(crate) fn begin_recording_start(&self) -> Option<u64> {
        let current = self.recording_start_token.load(Ordering::Acquire);
        if current & 1 == 1 {
            return None;
        }
        let token = current.wrapping_add(1);
        self.recording_start_token
            .compare_exchange(current, token, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| token)
    }

    pub(crate) fn recording_start_is_current(&self, token: u64) -> bool {
        self.recording_start_token.load(Ordering::Acquire) == token
    }

    pub(crate) fn finish_recording_start(&self, token: u64) -> bool {
        self.recording_start_token
            .compare_exchange(
                token,
                token.wrapping_add(1),
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    pub(crate) fn cancel_recording_start(&self) {
        let mut current = self.recording_start_token.load(Ordering::Acquire);
        while current & 1 == 1 {
            match self.recording_start_token.compare_exchange_weak(
                current,
                current.wrapping_add(1),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(observed) => current = observed,
            }
        }
    }

    /// Applies a hotkey press or release to the trigger state machine and
    /// returns the action it requests. `None` means the trigger lock is
    /// poisoned, so the caller must drop the event.
    pub(crate) fn trigger_event(&self, trigger_type: TriggerType, pressed: bool) -> Option<Action> {
        let mut trigger = self.trigger.lock().ok()?;
        Some(if pressed {
            trigger.press(trigger_type)
        } else {
            trigger.release(trigger_type)
        })
    }

    /// Forces the trigger state machine back to idle after a failed start or
    /// a stop. A poisoned lock is ignored: the trigger is self-healing on the
    /// next press cycle.
    pub(crate) fn force_trigger_idle(&self) {
        if let Ok(mut trigger) = self.trigger.lock() {
            trigger.force_idle();
        }
    }

    /// Whether no recording is in flight. A poisoned lock counts as busy, so
    /// a broken state never lets a second recording start.
    pub(crate) fn recording_is_idle(&self) -> bool {
        self.recording
            .lock()
            .is_ok_and(|recording| recording.is_none())
    }

    /// Completes the start transition: marks the recording live at `started`
    /// and publishes the initial `Recording` status so the overlay appears.
    /// A poisoned `recording` lock still publishes; stop paths treat the
    /// missing mark as "already cleared".
    pub(crate) fn recording_started(&self, app: &AppHandle, started: Instant) {
        if let Ok(mut recording) = self.recording.lock() {
            *recording = Some(started);
        }
        self.emit_status_event(
            app,
            StatusEvent {
                kind: StatusKind::Recording,
                revision: 0,
                message: None,
                correction_warning: None,
                elapsed_seconds: Some(0),
            },
        );
    }

    /// The start time of the current recording, if any (poisoned lock: none).
    pub(crate) fn current_recording_start(&self) -> Option<Instant> {
        self.recording.lock().ok().and_then(|recording| *recording)
    }

    /// Whether `started` is still the live recording. A poisoned lock or a
    /// replaced recording means it is not.
    pub(crate) fn recording_is_current(&self, started: Instant) -> bool {
        self.recording
            .lock()
            .is_ok_and(|recording| *recording == Some(started))
    }

    /// Removes and returns the live recording's start time, atomically
    /// claiming the stop transition. `None` means someone else already
    /// stopped it (or the lock is poisoned).
    pub(crate) fn take_recording_start(&self) -> Option<Instant> {
        self.recording
            .lock()
            .ok()
            .and_then(|mut recording| recording.take())
    }

    /// Starts audio capture; `on_auto_stop` runs when VAD ends the recording
    /// on its own. Returns the cpal name of the device actually opened.
    pub(crate) fn start_audio(
        &self,
        auto_vad: bool,
        device_name: Option<String>,
        on_auto_stop: Box<dyn Fn() + Send + Sync + 'static>,
    ) -> Result<String, String> {
        self.audio.start(auto_vad, device_name, on_auto_stop)
    }

    /// Stops audio capture and returns the recorded WAV bytes.
    pub(crate) fn stop_audio(&self) -> Result<Vec<u8>, String> {
        self.audio.stop()
    }

    /// Normalized live microphone level, `0.0` when not recording.
    pub(crate) fn audio_level(&self) -> f32 {
        self.audio.level()
    }

    pub(crate) fn new(
        settings: Settings,
        registered_hotkey: String,
        shortcut_manager: Option<ShortcutManager>,
        shortcut_status: ShortcutBackendStatus,
    ) -> Self {
        Self {
            settings: Mutex::new(SettingsRuntime {
                settings,
                registered_hotkey,
            }),
            trigger: Mutex::new(TriggerState::default()),
            audio: AudioController::new(),
            recording: Mutex::new(None),
            processing: Mutex::new(false),
            recording_start_token: AtomicU64::new(0),
            shortcut: Mutex::new(ShortcutRuntime {
                manager: shortcut_manager,
                status: shortcut_status,
                status_revision: 0,
            }),
            shortcut_operations: Mutex::new(()),
            shortcut_stopping: AtomicBool::new(false),
            hotkey_capture: std::sync::atomic::AtomicU64::new(0),
            status_revision: AtomicU64::new(0),
            status: Mutex::new(StatusEvent {
                kind: StatusKind::Ready,
                revision: 0,
                message: None,
                correction_warning: None,
                elapsed_seconds: None,
            }),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum StatusKind {
    Ready,
    Recording,
    Transcribing,
    Correcting,
    Error,
    Copied,
    Inserted,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct StatusEvent {
    pub(crate) kind: StatusKind,
    pub(crate) revision: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) message: Option<String>,
    #[serde(rename = "correctionWarning", skip_serializing_if = "Option::is_none")]
    pub(crate) correction_warning: Option<String>,
    #[serde(rename = "elapsedSeconds", skip_serializing_if = "Option::is_none")]
    pub(crate) elapsed_seconds: Option<u64>,
}

impl AppState {
    pub(crate) fn emit_status(&self, app: &AppHandle, kind: StatusKind, message: Option<String>) {
        self.emit_status_event(
            app,
            StatusEvent {
                kind,
                revision: 0,
                message,
                correction_warning: None,
                elapsed_seconds: None,
            },
        );
    }

    pub(crate) fn emit_status_event(&self, app: &AppHandle, event: StatusEvent) {
        let published = {
            let Ok(mut current) = self.status.lock() else {
                return;
            };
            publish_status_locked(&mut current, &self.status_revision, event, |published| {
                if let Err(error) = app.emit("charon://status", published.clone()) {
                    eprintln!("[charon] cannot emit status event: {error}");
                }
            })
        };
        Self::queue_status_window_action(app, published.clone(), published.revision);
    }

    fn queue_status_window_action(app: &AppHandle, event: StatusEvent, revision: u64) {
        let queued_app = app.clone();
        if app
            .run_on_main_thread(move || {
                let app = queued_app;
                let state = app.state::<Self>();
                state.run_status_window_action(&app, &event, revision);
            })
            .is_err()
        {
            eprintln!("[charon] could not queue status presentation");
        }
    }

    fn run_status_window_action(&self, app: &AppHandle, event: &StatusEvent, revision: u64) {
        let presentation = status_presentation(&event.kind);
        let current =
            with_current_status_revision(&self.status, revision, || {
                manage_recording_overlay(app, presentation.show);
            });
        if current.is_some() && presentation.delay_hide {
            Self::queue_overlay_hide(app.clone(), revision);
        }
    }

    fn queue_overlay_hide(app: AppHandle, revision: u64) {
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(4)).await;
            let queued_app = app.clone();
            if app
                .run_on_main_thread(move || {
                    let app = queued_app;
                    let state = app.state::<Self>();
                    state.hide_overlay_if_current(&app, revision);
                })
                .is_err()
            {
                eprintln!("[charon] could not queue overlay hide");
            }
        });
    }

    fn hide_overlay_if_current(&self, app: &AppHandle, revision: u64) {
        with_current_status_revision(&self.status, revision, || {
            if let Some(window) = app.get_webview_window("recording-overlay") {
                if let Err(error) = hide_recording_overlay(&window) {
                    eprintln!("[charon] recording overlay error: {error}");
                }
            }
        });
    }
}

fn publish_status_locked(
    snapshot: &mut StatusEvent,
    revision: &AtomicU64,
    mut event: StatusEvent,
    mut publish: impl FnMut(&StatusEvent),
) -> StatusEvent {
    event.revision = revision.fetch_add(1, Ordering::AcqRel) + 1;
    *snapshot = event.clone();
    publish(&event);
    event
}

fn with_current_status_revision<T>(
    status: &Mutex<StatusEvent>,
    expected: u64,
    action: impl FnOnce() -> T,
) -> Option<T> {
    let current = status.lock().ok()?;
    (current.revision == expected).then(action)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct StatusPresentation {
    show: bool,
    delay_hide: bool,
}

fn status_presentation(kind: &StatusKind) -> StatusPresentation {
    match kind {
        StatusKind::Recording | StatusKind::Transcribing | StatusKind::Correcting => {
            StatusPresentation {
                show: true,
                delay_hide: false,
            }
        }
        StatusKind::Inserted | StatusKind::Copied | StatusKind::Error => StatusPresentation {
            show: true,
            delay_hide: true,
        },
        StatusKind::Ready => StatusPresentation {
            show: false,
            delay_hide: false,
        },
    }
}

fn update_correction_settings_with(
    runtime: &mut SettingsRuntime,
    correction: settings::CorrectionSettings,
    save: impl FnOnce(&Settings) -> Result<(), String>,
) -> Result<Settings, String> {
    let mut next = runtime.settings.clone();
    correction.apply(&mut next)?;
    save(&next)?;
    runtime.settings = next.clone();
    Ok(next)
}

#[allow(clippy::too_many_arguments)]
fn update_general_settings_with(
    settings: &std::sync::Mutex<SettingsRuntime>,
    mut next: Settings,
    old_hotkey: &str,
    manager_kind: BackendKind,
    replace: impl FnMut(ShortcutChord) -> Result<(Result<(), String>, ShortcutBackendStatus), String>,
    publish_status: impl FnMut(ShortcutBackendStatus),
    save: impl FnOnce(&Settings) -> Result<(), String>,
    manager_status: impl FnOnce() -> Result<ShortcutBackendStatus, String>,
) -> Result<Settings, String> {
    next.hotkey
        .parse::<ShortcutChord>()
        .map_err(|error| error.to_string())?;
    update_general_settings_transaction(
        settings,
        &mut next,
        old_hotkey,
        manager_kind,
        replace,
        publish_status,
        save,
        manager_status,
    )
}

#[allow(clippy::too_many_arguments)]
fn update_general_settings_transaction(
    settings: &std::sync::Mutex<SettingsRuntime>,
    next: &mut Settings,
    old_hotkey: &str,
    manager_kind: BackendKind,
    mut replace: impl FnMut(
        ShortcutChord,
    ) -> Result<(Result<(), String>, ShortcutBackendStatus), String>,
    mut publish_status: impl FnMut(ShortcutBackendStatus),
    save: impl FnOnce(&Settings) -> Result<(), String>,
    manager_status: impl FnOnce() -> Result<ShortcutBackendStatus, String>,
) -> Result<Settings, String> {
    if next.hotkey != old_hotkey {
        let (result, status) = replace(
            next.hotkey
                .parse()
                .map_err(|error: crate::shortcut::ShortcutError| error.to_string())?,
        )?;
        publish_status(status);
        result?;
    }
    let authoritative = settings
        .lock()
        .map_err(|_| "settings lock poisoned")?
        .settings
        .clone();
    settings::preserve_correction(next, &authoritative);
    if let Err(error) = save(next) {
        if next.hotkey != old_hotkey {
            let old_chord = old_hotkey
                .parse::<ShortcutChord>()
                .map_err(|parse_error| parse_error.to_string())?;
            let (rollback, _) = replace(old_chord)?;
            if let Err(rollback) = rollback {
                publish_status(ShortcutBackendStatus::Failed {
                    backend: manager_kind,
                    detail: "Не удалось восстановить прежнюю горячую клавишу.".to_owned(),
                });
                return Err(format!("{error}; shortcut rollback failed: {rollback}"));
            }
            publish_status(manager_status()?);
        }
        return Err(error);
    }
    let mut runtime = settings.lock().map_err(|_| "settings lock poisoned")?;
    runtime.settings = next.clone();
    runtime.registered_hotkey.clone_from(&next.hotkey);
    drop(runtime);
    Ok(next.clone())
}

/// RAII guard that restores the [`ShortcutManager`] into [`ShortcutRuntime`]
/// when dropped.  This prevents the manager from being permanently lost if the
/// operation panics or returns early.
struct ManagerGuard<'a> {
    state: &'a AppState,
    manager: Option<ShortcutManager>,
}

impl ManagerGuard<'_> {
    /// Returns a mutable reference to the extracted manager.
    fn manager_mut(&mut self) -> &mut ShortcutManager {
        self.manager
            .as_mut()
            .expect("ManagerGuard manager consumed before drop")
    }
}

impl Drop for ManagerGuard<'_> {
    fn drop(&mut self) {
        if let Some(manager) = self.manager.take() {
            self.state.restore_shortcut_manager(manager);
        }
    }
}

impl AppState {
    /// Best-effort restore; if the mutex is poisoned the manager is
    /// abandoned, but that state already implies an unrecoverable panic.
    fn restore_shortcut_manager(&self, manager: ShortcutManager) {
        if let Ok(mut runtime) = self.shortcut.lock() {
            runtime.status = manager.status();
            runtime.manager = Some(manager);
        }
    }

    /// Runs `operation` against the live [`ShortcutManager`] while guaranteeing
    /// that neither `set_shared_status` (called by Wayland helper methods) nor the
    /// protocol reader thread can deadlock on the shortcut lock.
    ///
    /// # Design
    ///
    /// The manager is **taken** out of [`ShortcutRuntime`] under `shortcut` lock,
    /// after which the lock is dropped.  The operation then runs free of the
    /// `shortcut` mutex — so any internal status publication
    /// (`set_shared_status`) or channel wait (`wait_for`) cannot cause a
    /// self-deadlock or circular wait with the reader thread.
    ///
    /// A [`ManagerGuard`] restores the manager and refreshes the cached status on
    /// drop (including on panic/early-return), so readers always see a consistent
    /// runtime.
    ///
    /// `shortcut_operations` is still acquired first as a serialization primitive,
    /// ensuring a second caller cannot observe the temporarily empty manager slot.
    pub(crate) fn with_shortcut_manager<T>(
        &self,
        operation: impl FnOnce(&mut ShortcutManager) -> T,
    ) -> Result<T, String> {
        self.with_extracted_shortcut_manager(operation)
    }

    /// Constructs, configures, and installs a missing manager as one serialized
    /// operation. Construction and configuration run without the `shortcut` lock,
    /// because the Wayland helper publishes status through that same mutex.
    pub(crate) fn initialize_shortcut_manager(
        &self,
        backend: crate::shortcut::BackendKind,
        construct: impl FnOnce() -> Result<ShortcutManager, String>,
        configure: impl FnOnce(&mut ShortcutManager) -> Result<(), String>,
    ) -> Result<ShortcutBackendStatus, String> {
        let _operation = self
            .shortcut_operations
            .lock()
            .map_err(|_| "shortcut operation lock poisoned")?;
        self.initialize_shortcut_manager_locked(backend, construct, configure)
    }

    fn initialize_shortcut_manager_locked(
        &self,
        backend: crate::shortcut::BackendKind,
        construct: impl FnOnce() -> Result<ShortcutManager, String>,
        configure: impl FnOnce(&mut ShortcutManager) -> Result<(), String>,
    ) -> Result<ShortcutBackendStatus, String> {
        let (status_revision_before_construct, status_before_construct) = {
            let runtime = self
                .shortcut
                .lock()
                .map_err(|_| "shortcut lock poisoned")?;
            if runtime.manager.is_some() {
                return Ok(runtime.status.clone());
            }
            if self.shortcut_stopping.load(Ordering::Acquire) {
                return Err("shortcut backend is shutting down".to_owned());
            }
            (runtime.status_revision, runtime.status.clone())
        };

        let mut manager = match construct() {
            Ok(manager) => manager,
            Err(error) => {
                let mut runtime = self
                    .shortcut
                    .lock()
                    .map_err(|_| "shortcut lock poisoned")?;
                if self.shortcut_stopping.load(Ordering::Acquire) {
                    return Err("shortcut backend is shutting down".to_owned());
                }
                let actionable_status_was_published = runtime.status_revision
                    != status_revision_before_construct
                    && runtime.status != status_before_construct
                    && matches!(
                        &runtime.status,
                        ShortcutBackendStatus::PermissionDenied { .. }
                            | ShortcutBackendStatus::DevicesUnavailable { .. }
                    );
                if !actionable_status_was_published {
                    runtime.status = ShortcutBackendStatus::Failed {
                        backend,
                        detail: error.clone(),
                    };
                }
                drop(runtime);
                return Err(error);
            }
        };

        if let Err(error) = configure(&mut manager) {
            let manager_status = manager.status();
            let mut runtime = self
                .shortcut
                .lock()
                .map_err(|_| "shortcut lock poisoned")?;
            if self.shortcut_stopping.load(Ordering::Acquire) {
                manager.invalidate();
                return Err("shortcut backend is shutting down".to_owned());
            }
            runtime.status = match manager_status {
                status @ (ShortcutBackendStatus::PermissionDenied { .. }
                | ShortcutBackendStatus::DevicesUnavailable { .. }) => status,
                _ => ShortcutBackendStatus::Failed {
                    backend,
                    detail: error.clone(),
                },
            };
            drop(runtime);
            return Err(error);
        }

        let manager_status = manager.status();
        let mut runtime = self
            .shortcut
            .lock()
            .map_err(|_| "shortcut lock poisoned")?;
        if self.shortcut_stopping.load(Ordering::Acquire) {
            manager.invalidate();
            runtime.status = ShortcutBackendStatus::ShuttingDown;
            drop(runtime);
            return Err("shortcut backend is shutting down".to_owned());
        }
        let status_was_newly_published =
            runtime.status_revision != status_revision_before_construct
                && runtime.status != status_before_construct;
        let status = if status_was_newly_published
            && matches!(
                &runtime.status,
                ShortcutBackendStatus::PermissionDenied { .. }
                    | ShortcutBackendStatus::DevicesUnavailable { .. }
                    | ShortcutBackendStatus::Failed { .. }
            ) {
            runtime.status.clone()
        } else {
            manager_status
        };
        // The operations lock makes this assignment atomic with the absence check.
        runtime.manager = Some(manager);
        runtime.status = status.clone();
        drop(runtime);
        Ok(status)
    }

    /// Under one operation lock, retries an installed manager or initializes the
    /// missing Wayland manager. Callers never infer absence while another operation
    /// has temporarily extracted the manager.
    pub(crate) fn retry_or_initialize_shortcut_manager(
        &self,
        backend: crate::shortcut::BackendKind,
        retry: impl FnOnce(&mut ShortcutManager) -> Result<(), String>,
        construct: impl FnOnce() -> Result<ShortcutManager, String>,
        configure: impl FnOnce(&mut ShortcutManager) -> Result<(), String>,
    ) -> Result<ShortcutBackendStatus, String> {
        let _operation = self
            .shortcut_operations
            .lock()
            .map_err(|_| "shortcut operation lock poisoned")?;
        if self.shortcut_stopping.load(Ordering::Acquire) {
            return Err("shortcut backend is shutting down".to_owned());
        }

        let manager = self
            .shortcut
            .lock()
            .map_err(|_| "shortcut lock poisoned")?
            .manager
            .take();
        if let Some(manager) = manager {
            let mut guard = ManagerGuard {
                state: self,
                manager: Some(manager),
            };
            let result = retry(guard.manager_mut());
            if self.shortcut_stopping.load(Ordering::Acquire) {
                // Prevent ManagerGuard::drop from reinstalling this manager.
                if let Some(manager) = guard.manager.take() {
                    manager.invalidate();
                }
                return Err("shortcut backend is shutting down".to_owned());
            }
            result?;
            return Ok(guard.manager_mut().status());
        }

        self.initialize_shortcut_manager_locked(backend, construct, configure)
    }

    /// Marks shutdown immediately, then serializes extraction and shutdown of an
    /// installed manager. It is intended to run on a worker so an in-flight helper
    /// handshake never blocks the event loop.
    pub(crate) fn shutdown_shortcut_manager(
        &self,
        shutdown: impl FnOnce(&mut ShortcutManager) -> Result<(), String>,
    ) -> Result<(), String> {
        self.begin_shortcut_shutdown();
        let _operation = self
            .shortcut_operations
            .lock()
            .map_err(|_| "shortcut operation lock poisoned")?;
        let manager = self
            .shortcut
            .lock()
            .map_err(|_| "shortcut lock poisoned")?
            .manager
            .take();
        if let Some(mut manager) = manager {
            manager.invalidate();
            shutdown(&mut manager)?;
        }
        Ok(())
    }

    fn with_extracted_shortcut_manager<T>(
        &self,
        operation: impl FnOnce(&mut ShortcutManager) -> T,
    ) -> Result<T, String> {
        let _operation = self
            .shortcut_operations
            .lock()
            .map_err(|_| "shortcut operation lock poisoned")?;
        let mut runtime = self.shortcut.lock().map_err(|_| "shortcut lock poisoned")?;
        let manager = runtime
            .manager
            .take()
            .ok_or("shortcut manager is not initialized")?;
        drop(runtime);
        // The operation runs without holding `shortcut`.
        let mut guard = ManagerGuard {
            state: self,
            manager: Some(manager),
        };
        let result = operation(guard.manager_mut());
        // ManagerGuard::drop restores the manager and updates the cached status.
        Ok(result)
    }
}

impl AppState {
    pub(crate) fn set_shortcut_status(&self, app: &AppHandle, status: ShortcutBackendStatus) {
        let stopping = self.shortcut_stopping.load(Ordering::Acquire);
        if stopping && !matches!(status, ShortcutBackendStatus::ShuttingDown) {
            return;
        }
        if let Ok(mut shortcut) = self.shortcut.lock() {
            // Redundant check under lock to avoid racing the transition to ShuttingDown
            if self.shortcut_stopping.load(Ordering::Acquire)
                && !matches!(status, ShortcutBackendStatus::ShuttingDown)
            {
                return;
            }
            shortcut.status = status.clone();
            shortcut.status_revision = shortcut.status_revision.wrapping_add(1);
        }
        let _ = app.emit("charon://shortcut-status", status);
    }

    /// Publishes a shortcut status into the shared state and emits it verbatim,
    /// without the shutdown suppression of [`AppState::set_shortcut_status`].
    pub(crate) fn publish_shortcut_status_raw(
        &self,
        app: &AppHandle,
        status: ShortcutBackendStatus,
    ) {
        if let Ok(mut shortcut) = self.shortcut.lock() {
            shortcut.status = status.clone();
            shortcut.status_revision = shortcut.status_revision.wrapping_add(1);
        }
        let _ = app.emit("charon://shortcut-status", status);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status_event(kind: StatusKind) -> StatusEvent {
        StatusEvent {
            kind,
            revision: 0,
            message: None,
            correction_warning: None,
            elapsed_seconds: None,
        }
    }

    #[test]
    fn status_publications_are_monotonic_and_update_snapshot() {
        let mut snapshot = status_event(StatusKind::Ready);
        let revision = AtomicU64::new(0);
        let mut delivered = Vec::new();
        for kind in [StatusKind::Error, StatusKind::Recording] {
            let event =
                publish_status_locked(&mut snapshot, &revision, status_event(kind), |event| {
                    delivered.push(event.clone());
                });
            assert_eq!(event.revision, delivered.len() as u64);
            assert_eq!(snapshot.revision, event.revision);
            assert_eq!(
                std::mem::discriminant(&snapshot.kind),
                std::mem::discriminant(&event.kind)
            );
        }
        assert_eq!(delivered.len(), 2);
        assert!(matches!(delivered[0].kind, StatusKind::Error));
        assert!(matches!(delivered[1].kind, StatusKind::Recording));
    }

    #[test]
    fn status_presentation_classifies_all_kinds() {
        for (kind, show, delay_hide) in [
            (StatusKind::Recording, true, false),
            (StatusKind::Transcribing, true, false),
            (StatusKind::Correcting, true, false),
            (StatusKind::Inserted, true, true),
            (StatusKind::Copied, true, true),
            (StatusKind::Error, true, true),
            (StatusKind::Ready, false, false),
        ] {
            assert_eq!(
                status_presentation(&kind),
                StatusPresentation { show, delay_hide }
            );
        }
    }

    #[test]
    fn delayed_hide_requires_matching_revision_and_does_not_mutate_snapshot() {
        let mut snapshot = status_event(StatusKind::Ready);
        let revision = AtomicU64::new(0);
        let terminal = publish_status_locked(
            &mut snapshot,
            &revision,
            StatusEvent {
                message: Some("done".into()),
                correction_warning: Some("warning".into()),
                ..status_event(StatusKind::Error)
            },
            |_| {},
        );
        let before = serde_json::to_value(&snapshot).unwrap();
        let hidden = std::cell::Cell::new(0);
        let status = Mutex::new(snapshot);
        let pending_hide = || {
            with_current_status_revision(&status, terminal.revision, || {
                hidden.set(hidden.get() + 1);
            });
        };
        publish_status_locked(
            &mut status.lock().unwrap(),
            &revision,
            status_event(StatusKind::Recording),
            |_| {},
        );
        pending_hide();
        assert_eq!(hidden.get(), 0);
        let mut matching_snapshot = status_event(StatusKind::Ready);
        let matching_terminal = publish_status_locked(
            &mut matching_snapshot,
            &AtomicU64::new(terminal.revision - 1),
            terminal.clone(),
            |_| {},
        );
        let matching_status = Mutex::new(matching_snapshot);
        let matching_before = serde_json::to_value(&*matching_status.lock().unwrap()).unwrap();
        with_current_status_revision(&matching_status, matching_terminal.revision, || {
            hidden.set(hidden.get() + 1);
        });
        assert_eq!(hidden.get(), 1);
        assert_eq!(
            serde_json::to_value(&*matching_status.lock().unwrap()).unwrap(),
            matching_before
        );
        assert_eq!(before, matching_before);
    }

    #[test]
    fn status_serialization_uses_camel_case_and_omits_empty_fields() {
        let value = serde_json::to_value(StatusEvent {
            correction_warning: Some("warning".into()),
            ..status_event(StatusKind::Correcting)
        })
        .unwrap();
        assert_eq!(value["correctionWarning"], "warning");
        assert!(value.get("elapsedSeconds").is_none());
        assert!(value.get("message").is_none());
    }

    #[test]
    fn stale_queued_revision_does_not_run_action() {
        let status = Mutex::new(status_event(StatusKind::Error));
        let revision = status.lock().unwrap().revision;
        let ran = std::cell::Cell::new(false);
        *status.lock().unwrap() = StatusEvent {
            revision: revision + 1,
            ..status_event(StatusKind::Recording)
        };
        assert_eq!(
            with_current_status_revision(&status, revision, || ran.set(true)),
            None
        );
        assert!(!ran.get());
        assert_eq!(
            with_current_status_revision(&status, revision + 1, || ran.set(true)),
            Some(())
        );
        assert!(ran.get());
        assert_eq!(status.lock().unwrap().revision, revision + 1);
    }

    fn installed_manager_state() -> AppState {
        let state = test_state();
        state
            .initialize_shortcut_manager(
                crate::shortcut::BackendKind::Native,
                || Ok(ShortcutManager::native()),
                |_| Ok(()),
            )
            .expect("initialization should succeed");
        state
    }

    #[test]
    fn manager_operation_releases_shortcut_lock_and_restores_manager_on_ok() {
        let state = installed_manager_state();

        let result = state
            .with_shortcut_manager(|_| {
                // Models WaylandSupervisor::set_status publishing into AppState
                // while replace/retry owns the extracted manager.
                let mut runtime = state
                    .shortcut
                    .try_lock()
                    .expect("operation must not hold shortcut lock");
                runtime.status = ShortcutBackendStatus::Restarting {
                    backend: crate::shortcut::BackendKind::Native,
                };
                drop(runtime);
                Ok::<_, &'static str>(())
            })
            .expect("state operation should run");

        assert_eq!(result, Ok(()));
        assert!(state.shortcut.lock().unwrap().manager.is_some());
    }

    #[test]
    fn manager_operation_releases_shortcut_lock_and_restores_manager_on_err() {
        let state = installed_manager_state();

        let result = state
            .with_shortcut_manager(|_| {
                let _runtime = state
                    .shortcut
                    .try_lock()
                    .expect("operation must not hold shortcut lock");
                Err::<(), _>("operation failed")
            })
            .expect("state operation should run");

        assert_eq!(result, Err("operation failed"));
        assert!(state.shortcut.lock().unwrap().manager.is_some());
    }

    fn test_state() -> AppState {
        AppState::new(
            crate::settings::Settings::default(),
            "Control+Shift+Space".to_owned(),
            None,
            ShortcutBackendStatus::Starting {
                backend: crate::shortcut::BackendKind::WaylandHelper,
            },
        )
    }

    #[test]
    fn recording_start_allows_one_pending_check_and_can_cancel_it() {
        let state = test_state();
        let token = state.begin_recording_start().expect("first check starts");

        assert!(state.begin_recording_start().is_none());
        assert!(state.recording_start_is_current(token));

        state.cancel_recording_start();
        assert!(!state.recording_start_is_current(token));
        assert!(!state.finish_recording_start(token));
        assert!(state.begin_recording_start().is_some());
    }

    #[test]
    fn recording_start_token_can_be_finished_once() {
        let state = test_state();
        let token = state.begin_recording_start().expect("check starts");

        assert!(state.finish_recording_start(token));
        assert!(!state.finish_recording_start(token));
        assert!(state.begin_recording_start().is_some());
    }

    #[test]
    fn hotkey_capture_ignores_stale_commands() {
        let state = test_state();

        state.set_hotkey_capture(true, 1).unwrap();
        assert!(state.is_hotkey_capture_active());

        state.set_hotkey_capture(false, 2).unwrap();
        assert!(!state.is_hotkey_capture_active());

        state.set_hotkey_capture(true, 1).unwrap();
        assert!(!state.is_hotkey_capture_active());
    }

    #[test]
    fn capture_rejects_processing_and_pending_start_but_release_is_allowed() {
        let state = test_state();
        *state.processing.lock().unwrap() = true;
        assert!(state.set_hotkey_capture(true, 1).is_err());
        assert!(
            !state.is_hotkey_capture_active(),
            "rejected capture must not activate"
        );
        assert!(state.set_hotkey_capture(false, 1).is_ok());
        *state.processing.lock().unwrap() = false;

        let start = state.begin_recording_start().expect("start reservation");
        assert!(state.set_hotkey_capture(true, 2).is_err());
        assert!(state.set_hotkey_capture(false, 2).is_ok());
        assert!(state.finish_recording_start(start));
    }

    #[test]
    fn reservation_excludes_capture_and_rolls_back_on_drop() {
        let state = test_state();
        {
            let reservation = state.reserve_idle_processing().expect("idle reservation");
            assert!(state.set_hotkey_capture(true, 1).is_err());
            drop(reservation);
        }
        assert!(state.set_hotkey_capture(true, 1).is_ok());
    }

    #[test]
    fn stale_activation_while_busy_is_a_noop() {
        let state = test_state();
        state.set_hotkey_capture(true, 2).unwrap();
        state.set_hotkey_capture(false, 2).unwrap();
        *state.processing.lock().unwrap() = true;

        assert!(state.set_hotkey_capture(true, 1).is_ok());
        assert!(!state.is_hotkey_capture_active());
    }

    #[test]
    fn newer_capture_is_not_cleared_by_older_end() {
        let state = test_state();

        state.set_hotkey_capture(true, 3).unwrap();
        state.set_hotkey_capture(false, 2).unwrap();

        assert!(state.is_hotkey_capture_active());
    }

    #[test]
    fn initialize_succeeds_and_installs_manager() {
        let state = test_state();
        let status = state.initialize_shortcut_manager(
            crate::shortcut::BackendKind::Native,
            || Ok(ShortcutManager::native()),
            |_| Ok(()),
        )
        .expect("initialization should succeed");
        assert_eq!(
            status,
            ShortcutBackendStatus::Starting {
                backend: crate::shortcut::BackendKind::Native
            }
        );
        assert!(state.shortcut.lock().unwrap().manager.is_some());
    }

    #[test]
    fn initialize_construction_failure_leaves_manager_absent() {
        let state = test_state();
        let result = state.initialize_shortcut_manager(
            crate::shortcut::BackendKind::WaylandHelper,
            || Err("spawn failed".to_owned()),
            |_| unreachable!("configure should not run"),
        );
        assert_eq!(result, Err("spawn failed".to_owned()));
        let runtime = state.shortcut.lock().unwrap();
        assert!(runtime.manager.is_none());
        assert_eq!(
            runtime.status,
            ShortcutBackendStatus::Failed {
                backend: crate::shortcut::BackendKind::WaylandHelper,
                detail: "spawn failed".to_owned(),
            }
        );
        drop(runtime);
    }

    #[test]
    fn initialize_is_idempotent_when_manager_is_installed() {
        let state = test_state();
        state.initialize_shortcut_manager(
            crate::shortcut::BackendKind::Native,
            || Ok(ShortcutManager::native()),
            |_| Ok(()),
        )
        .expect("initialization should succeed");
        let result = state.initialize_shortcut_manager(
            crate::shortcut::BackendKind::WaylandHelper,
            || panic!("second construction must not run"),
            |_| panic!("second configuration must not run"),
        );
        assert_eq!(
            result,
            Ok(ShortcutBackendStatus::Starting {
                backend: crate::shortcut::BackendKind::Native
            })
        );
    }

    #[test]
    fn initialize_aborts_before_construction_after_shutdown() {
        let state = test_state();
        state.shortcut_stopping.store(true, Ordering::Release);
        let result = state.initialize_shortcut_manager(
            crate::shortcut::BackendKind::WaylandHelper,
            || panic!("construction must not run after shutdown"),
            |_| unreachable!(),
        );
        assert_eq!(result, Err("shortcut backend is shutting down".to_owned()));
        assert!(state.shortcut.lock().unwrap().manager.is_none());
    }

    #[test]
    fn initialize_does_not_install_when_shutdown_starts_during_construction() {
        let state = test_state();
        let result = state.initialize_shortcut_manager(
            crate::shortcut::BackendKind::Native,
            || {
                state.shortcut_stopping.store(true, Ordering::Release);
                Ok(ShortcutManager::native())
            },
            |_| Ok(()),
        );
        assert_eq!(result, Err("shortcut backend is shutting down".to_owned()));
        let runtime = state.shortcut.lock().unwrap();
        assert!(runtime.manager.is_none());
        assert_eq!(runtime.status, ShortcutBackendStatus::ShuttingDown);
        drop(runtime);
    }

    #[test]
    fn initialization_does_not_hold_shortcut_lock() {
        let state = test_state();
        state.initialize_shortcut_manager(
            crate::shortcut::BackendKind::Native,
            || {
                let runtime = state
                    .shortcut
                    .try_lock()
                    .expect("construction must not hold shortcut lock");
                assert!(runtime.manager.is_none());
                drop(runtime);
                Ok(ShortcutManager::native())
            },
            |manager| {
                let runtime = state
                    .shortcut
                    .try_lock()
                    .expect("configuration must not hold shortcut lock");
                assert!(runtime.manager.is_none());
                drop(runtime);
                manager.invalidate();
                Ok(())
            },
        )
        .expect("initialization should succeed");
        assert!(state.shortcut.lock().unwrap().manager.is_some());
    }

    #[test]
    fn retry_or_initialize_retries_installed_manager_without_construction() {
        let state = test_state();
        state.initialize_shortcut_manager(
            crate::shortcut::BackendKind::Native,
            || Ok(ShortcutManager::native()),
            |_| Ok(()),
        )
        .expect("initialization should succeed");
        let status = state.retry_or_initialize_shortcut_manager(
            crate::shortcut::BackendKind::Native,
            |_| Ok(()),
            || panic!("retry with installed manager must not construct"),
            |_| unreachable!(),
        )
        .expect("retry should succeed");
        assert_eq!(
            status,
            ShortcutBackendStatus::Starting {
                backend: crate::shortcut::BackendKind::Native
            }
        );
        assert!(state.shortcut.lock().unwrap().manager.is_some());
    }

    #[test]
    fn retry_or_initialize_constructs_missing_manager() {
        let state = test_state();
        let status = state.retry_or_initialize_shortcut_manager(
            crate::shortcut::BackendKind::Native,
            |_| unreachable!("missing manager must not use existing retry"),
            || Ok(ShortcutManager::native()),
            |_| Ok(()),
        )
        .expect("initialization should succeed");
        assert_eq!(
            status,
            ShortcutBackendStatus::Starting {
                backend: crate::shortcut::BackendKind::Native
            }
        );
        assert!(state.shortcut.lock().unwrap().manager.is_some());
    }

    #[test]
    fn shutdown_without_manager_is_benign() {
        let state = test_state();
        let result = state.shutdown_shortcut_manager( |_| panic!("no manager to shut down"));
        assert_eq!(result, Ok(()));
        assert!(state.shortcut_stopping.load(Ordering::Acquire));
        assert!(state.shortcut.lock().unwrap().manager.is_none());
    }

    #[test]
    fn initialize_after_prior_actionable_failure_uses_new_manager_status() {
        let state = test_state();
        {
            let mut runtime = state.shortcut.lock().unwrap();
            runtime.status = ShortcutBackendStatus::PermissionDenied {
                detail: "no access".to_owned(),
                setup_available: true,
            };
        }
        let status = state.initialize_shortcut_manager(
            crate::shortcut::BackendKind::Native,
            || Ok(ShortcutManager::native()),
            |_| Ok(()),
        )
        .expect("initialization should succeed");
        // The new manager's own status (Starting) must win, not the stale
        // PermissionDenied from the previous failed attempt.
        assert_eq!(
            status,
            ShortcutBackendStatus::Starting {
                backend: crate::shortcut::BackendKind::Native
            }
        );
        let runtime = state.shortcut.lock().unwrap();
        assert_eq!(
            runtime.status,
            ShortcutBackendStatus::Starting {
                backend: crate::shortcut::BackendKind::Native
            }
        );
        assert!(runtime.manager.is_some());
        drop(runtime);
    }

    #[test]
    fn retry_does_not_reinstall_manager_after_shutdown_begins() {
        let state = test_state();
        state.initialize_shortcut_manager(
            crate::shortcut::BackendKind::Native,
            || Ok(ShortcutManager::native()),
            |_| Ok(()),
        )
        .expect("initialization should succeed");
        let result = state.retry_or_initialize_shortcut_manager(
            crate::shortcut::BackendKind::Native,
            |_| {
                state.shortcut_stopping.store(true, Ordering::Release);
                Ok(())
            },
            || panic!("shutdown must not construct a new manager"),
            |_| unreachable!(),
        );
        assert_eq!(result, Err("shortcut backend is shutting down".to_owned()));
        assert!(state.shortcut.lock().unwrap().manager.is_none());
    }
}

#[cfg(test)]
mod settings_transaction_tests {
    use super::*;

    fn correction(url: Option<&str>) -> settings::CorrectionSettings {
        settings::CorrectionSettings {
            llm_server_url: url.map(str::to_owned),
            llm_model: Some("model".into()),
            llm_api_key: Some("secret".into()),
            llm_prompt: Some("prompt".into()),
        }
    }

    fn runtime(settings: Settings) -> SettingsRuntime {
        SettingsRuntime {
            registered_hotkey: settings.hotkey.clone(),
            settings,
        }
    }

    fn general_fixture() -> (SettingsRuntime, Settings) {
        let original = Settings {
            hotkey: "Control+Shift+Space".into(),
            llm_prompt: Some("authoritative correction".into()),
            llm_api_key: Some("authoritative key".into()),
            ..Settings::default()
        };
        (runtime(original.clone()), original)
    }

    fn general_candidate(old: &Settings, hotkey: &str) -> Settings {
        Settings {
            hotkey: hotkey.into(),
            llm_prompt: Some("stale correction snapshot".into()),
            llm_api_key: Some("stale key".into()),
            input_device: Some("mic".into()),
            ..old.clone()
        }
    }

    fn active_status(chord: &ShortcutChord) -> ShortcutBackendStatus {
        ShortcutBackendStatus::Active {
            backend: BackendKind::Native,
            shortcut: chord.to_string(),
            device_count: None,
        }
    }

    #[test]
    fn external_transaction_callbacks_run_without_state_mutexes() {
        let (initial, original) = general_fixture();
        let settings = std::sync::Mutex::new(initial);
        let shortcut = std::sync::Mutex::new(active_status(&original.hotkey.parse().unwrap()));
        let assert_unlocked = || {
            assert!(settings.try_lock().is_ok(), "settings mutex held");
            assert!(shortcut.try_lock().is_ok(), "shortcut mutex held");
        };
        let mut published = Vec::new();
        let error = update_general_settings_with(
            &settings,
            general_candidate(&original, "Control+Shift+KeyQ"),
            &original.hotkey,
            BackendKind::Native,
            |chord| {
                assert_unlocked();
                Ok((Ok(()), active_status(&chord)))
            },
            |status| {
                assert_unlocked();
                published.push(status);
            },
            |_| {
                assert_unlocked();
                Err("disk failed".into())
            },
            || {
                assert_unlocked();
                Ok(ShortcutBackendStatus::Failed {
                    backend: BackendKind::Native,
                    detail: "latest status".into(),
                })
            },
        );
        assert_eq!(error.unwrap_err(), "disk failed");
        assert_eq!(published.len(), 2);
        assert!(
            matches!(published[1], ShortcutBackendStatus::Failed { ref detail, .. } if detail == "latest status")
        );
        assert_eq!(settings.lock().unwrap().settings, original);
    }

    #[test]
    fn general_invalid_does_not_invoke_external_operations() {
        let (state, original) = general_fixture();
        let state = std::sync::Mutex::new(state);
        let calls = std::cell::Cell::new(0);
        assert!(update_general_settings_with(
            &state,
            general_candidate(&original, "not a chord"),
            &original.hotkey,
            BackendKind::Native,
            |_| {
                calls.set(calls.get() + 1);
                unreachable!()
            },
            |_| {
                calls.set(calls.get() + 1);
            },
            |_| {
                calls.set(calls.get() + 1);
                Ok(())
            },
            || {
                calls.set(calls.get() + 1);
                unreachable!()
            },
        )
        .is_err());
        assert_eq!(calls.get(), 0);
        assert_eq!(state.lock().unwrap().settings, original);
    }

    #[test]
    fn unchanged_settings_preserve_existing_processing_and_skip_hotkey_replacement() {
        let (initial, original) = general_fixture();
        let state = std::sync::Mutex::new(initial);
        let replaces = std::cell::Cell::new(0);
        update_general_settings_with(
            &state,
            general_candidate(&original, &original.hotkey),
            &original.hotkey,
            BackendKind::Native,
            |_| {
                replaces.set(replaces.get() + 1);
                unreachable!()
            },
            |_| {},
            |_| Ok(()),
            || unreachable!(),
        )
        .unwrap();
        assert_eq!(replaces.get(), 0);
        assert_eq!(state.lock().unwrap().settings.hotkey, original.hotkey);
    }

    #[test]
    fn general_unchanged_skips_shortcut_replacement() {
        let (initial, original) = general_fixture();
        let state = std::sync::Mutex::new(initial);
        let saves = std::cell::Cell::new(0);
        let replaces = std::cell::Cell::new(0);
        update_general_settings_with(
            &state,
            general_candidate(&original, &original.hotkey),
            &original.hotkey,
            BackendKind::Native,
            |_| {
                replaces.set(replaces.get() + 1);
                unreachable!()
            },
            |_| {},
            |next| {
                saves.set(saves.get() + 1);
                assert_eq!(next.llm_prompt, original.llm_prompt);
                Ok(())
            },
            || unreachable!(),
        )
        .unwrap();
        assert_eq!(replaces.get(), 0);
        assert_eq!(saves.get(), 1);
    }

    #[test]
    fn general_success_replaces_publishes_saves_and_commits() {
        let (state, original) = general_fixture();
        let state = std::sync::Mutex::new(state);
        let calls = std::cell::RefCell::new(Vec::new());
        let next = general_candidate(&original, "Control+Shift+KeyA");
        let expected = next.hotkey.parse::<ShortcutChord>().unwrap();
        let returned = update_general_settings_with(
            &state,
            next,
            &original.hotkey,
            BackendKind::Native,
            |chord| {
                calls.borrow_mut().push("replace");
                assert_eq!(chord, expected.clone());
                Ok((Ok(()), active_status(&chord)))
            },
            |_| calls.borrow_mut().push("publish"),
            |saved| {
                calls.borrow_mut().push("save");
                assert_eq!(saved.llm_prompt, original.llm_prompt);
                assert_eq!(saved.llm_api_key, original.llm_api_key);
                Ok(())
            },
            || {
                calls.borrow_mut().push("status");
                Ok(active_status(&expected))
            },
        )
        .unwrap();
        assert_eq!(*calls.borrow(), ["replace", "publish", "save"]);
        let committed = state.lock().unwrap();
        assert_eq!(committed.settings, returned);
        assert_eq!(committed.registered_hotkey, returned.hotkey);
        drop(committed);
        assert_eq!(returned.input_device.as_deref(), Some("mic"));
    }

    #[test]
    fn general_registration_failure_does_not_save_or_commit() {
        let (state, original) = general_fixture();
        let state = std::sync::Mutex::new(state);
        let saves = std::cell::Cell::new(0);
        let publishes = std::cell::Cell::new(0);
        let error = update_general_settings_with(
            &state,
            general_candidate(&original, "Control+Shift+KeyA"),
            &original.hotkey,
            BackendKind::Native,
            |chord| Ok((Err("register".into()), active_status(&chord))),
            |_| publishes.set(publishes.get() + 1),
            |_| {
                saves.set(saves.get() + 1);
                Ok(())
            },
            || unreachable!(),
        )
        .unwrap_err();
        assert_eq!(error, "register");
        assert_eq!(publishes.get(), 1);
        assert_eq!(saves.get(), 0);
        assert_eq!(state.lock().unwrap().settings, original);
        assert_eq!(state.lock().unwrap().registered_hotkey, original.hotkey);
    }

    #[test]
    fn general_save_failure_rolls_back_and_preserves_runtime() {
        let (state, original) = general_fixture();
        let state = std::sync::Mutex::new(state);
        let calls = std::cell::RefCell::new(Vec::new());
        let next = general_candidate(&original, "Control+Shift+KeyA");
        let mut published = Vec::new();
        let returned = update_general_settings_with(
            &state,
            next,
            &original.hotkey,
            BackendKind::Native,
            |chord| {
                calls.borrow_mut().push("replace");
                Ok((Ok(()), active_status(&chord)))
            },
            |status| {
                calls.borrow_mut().push("publish");
                published.push(status);
            },
            |_| {
                calls.borrow_mut().push("save");
                Err("disk".into())
            },
            || {
                calls.borrow_mut().push("status");
                Ok(ShortcutBackendStatus::Failed {
                    backend: BackendKind::Native,
                    detail: "latest status".into(),
                })
            },
        )
        .unwrap_err();
        assert_eq!(returned, "disk");
        assert_eq!(
            *calls.borrow(),
            ["replace", "publish", "save", "replace", "status", "publish"]
        );
        assert!(
            matches!(published.last(), Some(ShortcutBackendStatus::Failed { detail, .. }) if detail == "latest status")
        );
        assert_eq!(state.lock().unwrap().settings, original);
        assert_eq!(state.lock().unwrap().registered_hotkey, original.hotkey);
    }

    #[test]
    fn general_rollback_failure_reports_combined_error_and_failed_status() {
        let (state, original) = general_fixture();
        let state = std::sync::Mutex::new(state);
        let calls = std::cell::RefCell::new(Vec::new());
        let error = update_general_settings_with(
            &state,
            general_candidate(&original, "Control+Shift+KeyA"),
            &original.hotkey,
            BackendKind::Native,
            |_| {
                let mut calls = calls.borrow_mut();
                calls.push("replace");
                if calls.len() == 1 {
                    Ok((
                        Ok(()),
                        active_status(&"Control+Shift+KeyA".parse().unwrap()),
                    ))
                } else {
                    Ok((
                        Err("rollback".into()),
                        active_status(&"Control+Shift+KeyA".parse().unwrap()),
                    ))
                }
            },
            |status| {
                calls.borrow_mut().push(
                    if matches!(status, ShortcutBackendStatus::Failed { .. }) {
                        "failed"
                    } else {
                        "publish"
                    },
                );
            },
            |_| {
                calls.borrow_mut().push("save");
                Err("disk".into())
            },
            || unreachable!(),
        )
        .unwrap_err();
        assert_eq!(error, "disk; shortcut rollback failed: rollback");
        assert_eq!(
            *calls.borrow(),
            ["replace", "publish", "save", "replace", "failed"]
        );
        assert_eq!(state.lock().unwrap().settings, original);
    }

    #[test]
    fn general_manager_acquisition_failure_propagates_before_status_publish() {
        let (state, original) = general_fixture();
        let state = std::sync::Mutex::new(state);
        let publishes = std::cell::Cell::new(0);
        let result = update_general_settings_with(
            &state,
            general_candidate(&original, "Control+Shift+KeyA"),
            &original.hotkey,
            BackendKind::Native,
            |_| Err("manager lock poisoned".into()),
            |_| publishes.set(publishes.get() + 1),
            |_| panic!("must not save"),
            || unreachable!(),
        );
        assert_eq!(result.unwrap_err(), "manager lock poisoned");
        assert_eq!(publishes.get(), 0);
        assert_eq!(state.lock().unwrap().settings, original);
    }

    #[test]
    fn correction_invalid_does_not_save_or_commit() {
        let original = Settings::default();
        let mut state = runtime(original.clone());
        let saves = std::cell::Cell::new(0);
        assert!(update_correction_settings_with(
            &mut state,
            correction(Some("file:///bad")),
            |_| {
                saves.set(saves.get() + 1);
                Ok(())
            }
        )
        .is_err());
        assert_eq!(saves.get(), 0);
        assert_eq!(state.settings, original);
        assert_eq!(state.registered_hotkey, original.hotkey);
    }

    #[test]
    fn correction_save_failure_preserves_runtime_and_registered_hotkey() {
        let original = Settings {
            llm_prompt: Some("existing".into()),
            ..Settings::default()
        };
        let mut state = runtime(original.clone());
        let error = update_correction_settings_with(
            &mut state,
            correction(Some("https://api.example.test")),
            |_| Err("disk".into()),
        );
        assert_eq!(error.unwrap_err(), "disk");
        assert_eq!(state.settings, original);
        assert_eq!(state.registered_hotkey, original.hotkey);
    }

    #[test]
    fn correction_success_persists_returns_and_commits_preserving_general_fields() {
        let original = Settings {
            input_device: Some("mic".into()),
            llm_prompt: Some("old".into()),
            ..Settings::default()
        };
        let mut state = runtime(original.clone());
        let mut persisted = None;
        let returned = update_correction_settings_with(
            &mut state,
            correction(Some("https://api.example.test")),
            |next| {
                persisted = Some(next.clone());
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(persisted, Some(returned.clone()));
        assert_eq!(state.settings, returned);
        assert_eq!(state.registered_hotkey, original.hotkey);
        assert_eq!(returned.hotkey, original.hotkey);
        assert_eq!(returned.server_url, original.server_url);
        assert_eq!(returned.trigger_type, original.trigger_type);
        assert_eq!(returned.input_device, original.input_device);
    }
}

