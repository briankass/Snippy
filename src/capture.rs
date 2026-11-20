use super::*;

// ───────────────────────────── Screenshot backend ─────────────────────────────
//
// A capture freezes *every* monitor at once, so the selection overlay can span all screens.
// Coordinates: each `Screen` image is in that monitor's physical pixels; `origin` places it
// on the virtual desktop (also physical pixels). Window rects are virtual-desktop pixels.

/// A visible window, in virtual-desktop physical pixels.
pub(crate) struct WindowHit {
    pub(crate) rect: Rect,
    pub(crate) title: String,
}

/// One frozen monitor.
pub(crate) struct Screen {
    pub(crate) monitor_id: Option<u32>,
    /// Top-left on the virtual desktop, physical pixels.
    pub(crate) origin: Pos2,
    /// Display scaling (1.0 = 100 %, 1.5 = 150 % …).
    pub(crate) scale: f32,
    pub(crate) image: RgbaImage,
}

impl Screen {
    /// The monitor's area on the virtual desktop, physical pixels.
    pub(crate) fn bounds(&self) -> Rect {
        Rect::from_min_size(self.origin, Vec2::new(self.image.width() as f32, self.image.height() as f32))
    }
}

pub(crate) struct Capture {
    pub(crate) purpose: Purpose,
    pub(crate) mode: CaptureMode,
    pub(crate) screens: Vec<Screen>,
    /// The screen under the mouse pointer when the capture started.
    pub(crate) cursor_screen: usize,
    /// Top-most first. Only filled in Window mode.
    pub(crate) windows: Vec<WindowHit>,
}

/// Index of the screen containing `p` (physical pixels), else the first one.
pub(crate) fn screen_at(screens: &[Screen], p: Option<Pos2>) -> usize {
    p.and_then(|p| screens.iter().position(|s| s.bounds().contains(p))).unwrap_or(0)
}

/// Captures all monitors. `probe` (physical pixels, usually the mouse pointer) picks the
/// "current" screen.
pub(crate) fn capture(purpose: Purpose, mode: CaptureMode, probe: Option<Pos2>) -> Result<Capture, String> {
    let mut monitors = Monitor::all().map_err(|e| e.to_string())?;
    // Primary first, so index 0 is a sensible fallback.
    monitors.sort_by_key(|m| !m.is_primary().unwrap_or(false));

    let mut screens = Vec::new();
    let mut last_err = None;
    for m in &monitors {
        match m.capture_image() {
            Ok(image) => screens.push(Screen {
                monitor_id: m.id().ok(),
                origin: Pos2::new(m.x().unwrap_or(0) as f32, m.y().unwrap_or(0) as f32),
                scale: m.scale_factor().unwrap_or(1.0).max(0.5),
                image,
            }),
            Err(e) => last_err = Some(e.to_string()),
        }
    }
    if screens.is_empty() {
        return Err(last_err.unwrap_or_else(|| "no monitor found".into()));
    }
    #[cfg(debug_assertions)]
    split_for_testing(&mut screens);
    let windows = if mode == CaptureMode::Window { list_windows() } else { Vec::new() };
    let cursor_screen = screen_at(&screens, probe);
    Ok(Capture { purpose, mode, screens, cursor_screen, windows })
}

/// Debug builds only: `SNIPPY_SPLIT_SCREENS=1` pretends a single monitor is two side-by-side
/// monitors (the second at 150 % scaling), to exercise multi-monitor code on one screen.
#[cfg(debug_assertions)]
fn split_for_testing(screens: &mut Vec<Screen>) {
    if std::env::var_os("SNIPPY_SPLIT_SCREENS").is_none() || screens.len() != 1 {
        return;
    }
    let s = screens.remove(0);
    let half = s.image.width() / 2;
    let left = imageops::crop_imm(&s.image, 0, 0, half, s.image.height()).to_image();
    let right = imageops::crop_imm(&s.image, half, 0, s.image.width() - half, s.image.height()).to_image();
    screens.push(Screen { monitor_id: s.monitor_id, origin: s.origin, scale: s.scale, image: left });
    screens.push(Screen { monitor_id: s.monitor_id, origin: s.origin + Vec2::new(half as f32, 0.0), scale: s.scale, image: right });
}

/// All visible top-level windows (except Snippy's), top-most first.
pub(crate) fn list_windows() -> Vec<WindowHit> {
    let me = std::process::id();
    let Ok(mut windows) = Window::all() else { return Vec::new() };
    windows.sort_by_key(|w| std::cmp::Reverse(w.z().unwrap_or(0)));
    windows
        .into_iter()
        .filter(|w| !w.is_minimized().unwrap_or(true) && w.pid().ok() != Some(me))
        .filter_map(|w| {
            let (x, y) = (w.x().ok()? as f32, w.y().ok()? as f32);
            let (ww, wh) = (w.width().ok()? as f32, w.height().ok()? as f32);
            if ww < 8.0 || wh < 8.0 {
                return None;
            }
            let title = w.title().unwrap_or_default();
            let title = if title.trim().is_empty() { w.app_name().unwrap_or_default() } else { title };
            Some(WindowHit { rect: Rect::from_min_size(Pos2::new(x, y), Vec2::new(ww, wh)), title: title.trim().to_owned() })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(x: f32, y: f32, w: u32, h: u32) -> Screen {
        Screen { monitor_id: None, origin: Pos2::new(x, y), scale: 1.0, image: RgbaImage::new(w, h) }
    }

    #[test]
    fn finds_screen_under_point() {
        // Laptop on the left, a 4K monitor to its right, one above with a negative origin.
        let s = vec![screen(0.0, 0.0, 1920, 1080), screen(1920.0, -200.0, 3840, 2160), screen(0.0, -1440.0, 2560, 1440)];
        assert_eq!(screen_at(&s, Some(Pos2::new(100.0, 100.0))), 0);
        assert_eq!(screen_at(&s, Some(Pos2::new(3000.0, 1500.0))), 1);
        assert_eq!(screen_at(&s, Some(Pos2::new(500.0, -500.0))), 2);
        assert_eq!(screen_at(&s, Some(Pos2::new(-50.0, 0.0))), 0); // nowhere → first
        assert_eq!(screen_at(&s, None), 0);
    }
}
