//! Enumerate capturable on-screen windows for the "record this window" picker.
//!
//! Uses ScreenCaptureKit's `SCShareableContent` (macOS, `sck` feature). The
//! chosen window id is later captured occlusion-independently via
//! `screencapture -l<id>` (see `frames.rs`) — so the call window need not stay
//! visible/foreground.

use serde::Serialize;

#[derive(Serialize, Clone, Debug)]
pub struct WindowInfo {
    pub id: u32,
    pub app: String,
    pub title: String,
}

#[cfg(all(target_os = "macos", feature = "sck"))]
pub fn list() -> Vec<WindowInfo> {
    use screencapturekit::shareable_content::SCShareableContent;

    let content = match SCShareableContent::get() {
        Ok(c) => c,
        Err(e) => {
            log::warn!("[windows] shareable content failed: {e:?}");
            return Vec::new();
        }
    };
    // Apps that are never the meeting window — skip to keep the list clean.
    const SKIP_APPS: &[&str] = &[
        "granola", "Vexa", "Window Server", "Dock", "Control Center",
        "Notification Center", "WindowManager", "Spotlight",
    ];
    let mut out: Vec<WindowInfo> = Vec::new();
    for w in content.windows() {
        if w.window_layer() != 0 {
            continue; // only normal app windows (not menus/overlays)
        }
        let title = w.title();
        if title.trim().is_empty() {
            continue;
        }
        let app = w.owning_application().application_name();
        if SKIP_APPS.iter().any(|s| s.eq_ignore_ascii_case(&app)) {
            continue;
        }
        let frame = w.get_frame();
        if frame.size.width < 200.0 || frame.size.height < 150.0 {
            continue; // tiny utility windows
        }
        out.push(WindowInfo {
            id: w.window_id(),
            app,
            title,
        });
    }
    out.sort_by(|a, b| {
        (a.app.to_lowercase(), a.title.to_lowercase())
            .cmp(&(b.app.to_lowercase(), b.title.to_lowercase()))
    });
    out
}

#[cfg(not(all(target_os = "macos", feature = "sck")))]
pub fn list() -> Vec<WindowInfo> {
    Vec::new()
}
