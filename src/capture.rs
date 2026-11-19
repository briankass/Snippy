use super::*;

// ───────────────────────────── Screenshot backend ─────────────────────────────

/// A window visible on the captured monitor, in image-pixel coordinates.
pub(crate) struct WindowHit {
    pub(crate) rect: Rect,
    pub(crate) title: String,
}

pub(crate) struct Capture {
    pub(crate) purpose: Purpose,
    pub(crate) mode: CaptureMode,
    pub(crate) monitor_id: Option<u32>,
    pub(crate) image: RgbaImage,
    /// Top-most first. Only filled in Window mode.
    pub(crate) windows: Vec<WindowHit>,
}

/// `probe` is a physical-pixel point on the monitor to capture (Snippy's own position).
pub(crate) fn capture(purpose: Purpose, mode: CaptureMode, probe: Option<Pos2>) -> Result<Capture, String> {
    let monitor = probe
        .and_then(|p| Monitor::from_point(p.x as i32, p.y as i32).ok())
        .or_else(|| {
            Monitor::all()
                .ok()?
                .into_iter()
                .find(|m| m.is_primary().unwrap_or(false))
        })
        .ok_or("no monitor found")?;

    let image = monitor.capture_image().map_err(|e| e.to_string())?;
    let windows = if mode == CaptureMode::Window {
        list_windows(&monitor, &image)
    } else {
        Vec::new()
    };
    Ok(Capture { purpose, mode, monitor_id: monitor.id().ok(), image, windows })
}

pub(crate) fn list_windows(monitor: &Monitor, image: &RgbaImage) -> Vec<WindowHit> {
    let (mx, my) = (monitor.x().unwrap_or(0) as f32, monitor.y().unwrap_or(0) as f32);
    let bounds = Rect::from_min_size(Pos2::ZERO, Vec2::new(image.width() as f32, image.height() as f32));
    let me = std::process::id();

    let Ok(mut windows) = Window::all() else { return Vec::new() };
    windows.sort_by_key(|w| std::cmp::Reverse(w.z().unwrap_or(0))); // top-most first

    windows
        .into_iter()
        .filter(|w| !w.is_minimized().unwrap_or(true) && w.pid().ok() != Some(me))
        .filter_map(|w| {
            let (x, y) = (w.x().ok()? as f32 - mx, w.y().ok()? as f32 - my);
            let (ww, wh) = (w.width().ok()? as f32, w.height().ok()? as f32);
            let rect = Rect::from_min_size(Pos2::new(x, y), Vec2::new(ww, wh)).intersect(bounds);
            if rect.width() < 8.0 || rect.height() < 8.0 {
                return None;
            }
            let title = w.title().unwrap_or_default();
            let title = if title.trim().is_empty() { w.app_name().unwrap_or_default() } else { title };
            Some(WindowHit { rect, title: title.trim().to_owned() })
        })
        .collect()
}
