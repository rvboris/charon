//! Recording overlay window management: creation, geometry, show and hide.
//!
//! Pure window mechanics, deliberately separate from [`crate::state`]: the
//! state module decides *when* the overlay must appear or disappear and calls
//! in with a single `show` decision; this module owns *how* the window is
//! built and placed.

use num_traits::ToPrimitive;
use tauri::{AppHandle, LogicalSize, Manager, PhysicalPosition, Position, Size, WebviewWindow,
    WebviewWindowBuilder};

/// Shows or hides the recording overlay for the current status presentation.
/// `show == false` hides an existing window (creating nothing).
pub(crate) fn manage_recording_overlay(app: &AppHandle, show: bool) {
    let result = if show {
        get_or_create_recording_overlay(app).and_then(|window| show_recording_overlay(&window))
    } else {
        app.get_webview_window("recording-overlay")
            .map_or(Ok(()), |window| hide_recording_overlay(&window))
    };
    if let Err(error) = result {
        eprintln!("[charon] recording overlay error: {error}");
    }
}

pub(crate) fn hide_recording_overlay(window: &WebviewWindow) -> Result<(), String> {
    window
        .hide()
        .map_err(|error| format!("cannot hide recording overlay: {error}"))
}

fn get_or_create_recording_overlay(app: &AppHandle) -> Result<WebviewWindow, String> {
    if let Some(window) = app.get_webview_window("recording-overlay") {
        return Ok(window);
    }

    let config = app
        .config()
        .app
        .windows
        .iter()
        .find(|config| config.label == "recording-overlay")
        .ok_or_else(|| "recording overlay config not found".to_owned())?;

    WebviewWindowBuilder::from_config(app, config)
        .map_err(|error| format!("cannot configure recording overlay: {error}"))?
        .visible(false)
        .focusable(false)
        .build()
        .map_err(|error| format!("cannot build recording overlay: {error}"))
}

fn show_recording_overlay(window: &WebviewWindow) -> Result<(), String> {
    const OVERLAY_W: f64 = 172.0;
    const OVERLAY_H: f64 = 48.0;

    // Do not trust a compositor-restored/default geometry: the card fills this
    // surface, so a stale square window turns the rounded card into a huge disk.
    window
        .set_size(Size::Logical(LogicalSize::new(OVERLAY_W, OVERLAY_H)))
        .map_err(|error| format!("cannot size recording overlay: {error}"))?;

    // Position computation is harmless on X11; on GNOME/Mutter (Wayland) the
    // compositor places the surface itself and `set_position` is a no-op.
    if let Some(monitor) = window
        .primary_monitor()
        .map_err(|error| format!("cannot get primary monitor: {error}"))?
    {
        let monitor_size = monitor.size();
        let monitor_position = monitor.position();
        // Use the requested logical size scaled to physical pixels instead of
        // querying outer_size(): on XWayland the previous set_size may not have
        // taken effect yet, so outer_size() returns the stale default geometry
        // and the first-show position ends up shifted.
        let scale = monitor.scale_factor();
        let window_w = (OVERLAY_W * scale)
            .round()
            .to_i32()
            .ok_or_else(|| "recording overlay width exceeds supported range".to_owned())?;
        let window_h = (OVERLAY_H * scale)
            .round()
            .to_i32()
            .ok_or_else(|| "recording overlay height exceeds supported range".to_owned())?;
        let monitor_w = i32::try_from(monitor_size.width)
            .map_err(|_| "primary monitor width exceeds supported range".to_owned())?;
        let monitor_h = i32::try_from(monitor_size.height)
            .map_err(|_| "primary monitor height exceeds supported range".to_owned())?;
        let bottom_margin = (28.0 * scale)
            .round()
            .to_i32()
            .ok_or_else(|| "recording overlay margin exceeds supported range".to_owned())?;
        let x = monitor_position.x + (monitor_w - window_w) / 2;
        let y = monitor_position.y + monitor_h - window_h - bottom_margin;
        window
            .set_position(Position::Physical(PhysicalPosition { x, y }))
            .map_err(|error| format!("cannot position recording overlay: {error}"))?;
    }
    // Re-assert these at show time: GNOME/Mutter often ignores the static
    // tauri.conf.json window flags and needs them set just before mapping.
    window
        .set_always_on_top(true)
        .map_err(|error| format!("cannot set recording overlay always on top: {error}"))?;
    window
        .set_skip_taskbar(true)
        .map_err(|error| format!("cannot skip taskbar for recording overlay: {error}"))?;
    window
        .show()
        .map_err(|error| format!("cannot show recording overlay: {error}"))?;
    Ok(())
}
