//! Scrolling capture: scrolls the content under a selected area and stitches the frames
//! into one tall image.
//!
//! Each step: put the pointer over the area, send mouse-wheel input, wait until the
//! content stops moving (smooth-scrolling animations), capture the area, then find how far
//! the content moved by matching row "signatures" between the previous and the new frame.
//! Rows that never move (sticky headers / footers) are detected from the first scroll and
//! kept only once.

use image::RgbaImage;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Never let a single image grow past this many bytes (≈ 25 Mpx RGBA).
const MAX_BYTES: u64 = 100 * 1024 * 1024;
const MAX_STEPS: usize = 400;
/// Columns at the right edge ignored when matching (scrollbars move independently).
const SCROLLBAR_PX: u32 = 24;

/// Per-row fingerprint: (hash, informative). "Informative" rows have some detail; blank
/// rows match anything, so they don't count as evidence when aligning frames.
pub type Sig = (u64, bool);

pub fn signatures(img: &RgbaImage) -> Vec<Sig> {
    let w = img.width();
    let core_end = if w > SCROLLBAR_PX * 3 { w - SCROLLBAR_PX } else { w };
    let raw = img.as_raw();
    let stride = w as usize * 4;
    (0..img.height() as usize)
        .map(|y| {
            let row = &raw[y * stride..y * stride + core_end as usize * 4];
            let mut h: u64 = 0xcbf2_9ce4_8422_2325;
            let first = [row[0] >> 4, row[1] >> 4, row[2] >> 4];
            let mut varied = false;
            for px in row.chunks_exact(4).step_by(2) {
                // Ignore tiny differences (anti-aliasing noise): compare at 4 bits/channel.
                let q = [px[0] >> 4, px[1] >> 4, px[2] >> 4];
                varied |= q != first;
                for v in q {
                    h = (h ^ v as u64).wrapping_mul(0x0100_0000_01b3);
                }
            }
            (h, varied)
        })
        .collect()
}

/// Rows at the top and bottom that didn't change between two frames: sticky header /
/// footer. Each band is capped at 35 % of the height. Returns `None` if (almost) nothing
/// changed, i.e. the content didn't move.
pub fn fixed_bands(a: &[Sig], b: &[Sig]) -> Option<(usize, usize)> {
    let h = a.len().min(b.len());
    let cap = h * 35 / 100;
    let top = (0..h).take_while(|&i| a[i].0 == b[i].0).count();
    if top >= h.saturating_sub(4) {
        return None; // nothing moved
    }
    let bottom = (0..h).take_while(|&i| a[h - 1 - i].0 == b[h - 1 - i].0).count();
    Some((top.min(cap), bottom.min(cap)))
}

#[derive(Debug, PartialEq, Eq)]
pub enum Shift {
    /// The content moved up by this many rows.
    By(usize),
    /// Nothing moved (end reached, or the app ignored the scroll).
    None,
    /// Couldn't line the frames up.
    Lost,
}

/// How far the content moved from `a` to `b`, ignoring the fixed bands.
pub fn find_shift(a: &[Sig], b: &[Sig], top: usize, bottom: usize) -> Shift {
    let h = a.len().min(b.len());
    let (lo, hi) = (top, h.saturating_sub(bottom));
    if hi <= lo + 8 {
        return Shift::Lost;
    }
    // Unchanged?
    let (same, info) = (lo..hi).fold((0, 0), |(m, t), r| {
        if a[r].1 {
            (m + (a[r].0 == b[r].0) as usize, t + 1)
        } else {
            (m, t)
        }
    });
    if info == 0 || same * 100 >= info * 98 {
        return Shift::None;
    }
    let mut best: Option<(usize, usize, usize)> = None; // (shift, matches, total)
    for s in 1..(hi - lo - 8) {
        let (mut m, mut t) = (0usize, 0usize);
        for r in lo..hi - s {
            if a[r + s].1 {
                t += 1;
                if a[r + s].0 == b[r].0 {
                    m += 1;
                }
            }
        }
        if t < 8 {
            continue;
        }
        let better = match best {
            None => true,
            // higher match ratio wins; ties → more evidence
            Some((_, bm, bt)) => m * bt > bm * t || (m * bt == bm * t && m > bm),
        };
        if better {
            best = Some((s, m, t));
        }
    }
    match best {
        Some((s, m, t)) if m * 100 >= t * 85 => Shift::By(s),
        _ => Shift::Lost,
    }
}

/// Builds the tall image from successive frames.
pub struct Stitcher {
    width: u32,
    height: u32,
    rows: Vec<u8>,
    first: Option<RgbaImage>,
    last: Option<RgbaImage>,
    bands: Option<(usize, usize)>,
}

impl Stitcher {
    pub fn new(first: RgbaImage) -> Self {
        Self { width: first.width(), height: first.height(), rows: Vec::new(), first: Some(first), last: None, bands: None }
    }

    fn stride(&self) -> usize {
        self.width as usize * 4
    }

    /// Current output height in pixels (footer not yet included).
    pub fn height(&self) -> usize {
        self.rows.len() / self.stride().max(1)
    }

    /// Adds the next frame, whose content moved up by `shift` rows relative to the
    /// previous frame. `bands` (fixed top/bottom rows) must be the same for every call.
    pub fn push(&mut self, frame: RgbaImage, shift: usize, bands: (usize, usize)) {
        let stride = self.stride();
        let h = self.height as usize;
        let (_, bottom) = bands;
        if self.bands.is_none() {
            // First scroll: now that the footer is known, start the output with the
            // first frame minus its footer.
            let first = self.first.take().expect("first frame");
            self.rows.extend_from_slice(&first.as_raw()[..(h - bottom) * stride]);
            self.bands = Some(bands);
        }
        let raw = frame.as_raw();
        let from = h - bottom - shift.min(h - bottom);
        self.rows.extend_from_slice(&raw[from * stride..(h - bottom) * stride]);
        self.last = Some(frame);
    }

    /// Final image: everything so far plus the footer from the last frame.
    pub fn finish(mut self) -> RgbaImage {
        let stride = self.stride();
        let h = self.height as usize;
        match (self.bands, self.last.take(), self.first.take()) {
            (Some((_, bottom)), Some(last), _) => {
                self.rows.extend_from_slice(&last.as_raw()[(h - bottom) * stride..h * stride]);
                let total = (self.rows.len() / stride) as u32;
                RgbaImage::from_raw(self.width, total, self.rows).expect("consistent size")
            }
            // Never scrolled: just the first frame.
            (_, _, Some(first)) => first,
            _ => RgbaImage::new(self.width, self.height),
        }
    }
}

/// What to scroll-capture: an area of one monitor, in that monitor's physical pixels.
pub struct ScrollTarget {
    pub monitor_id: Option<u32>,
    /// `[x, y, w, h]` relative to the monitor.
    pub region: [u32; 4],
    /// Monitor's top-left on the virtual desktop (for positioning the pointer).
    pub origin: (i32, i32),
}

/// Runs the scroll-and-stitch loop. `progress` receives the current height in pixels.
/// Returns the stitched image and notes for the user.
pub fn run(target: ScrollTarget, stop: Arc<AtomicBool>, progress: Arc<AtomicU32>) -> Result<(RgbaImage, Vec<String>), String> {
    let monitors = xcap::Monitor::all().map_err(|e| e.to_string())?;
    let monitor = monitors
        .into_iter()
        .find(|m| target.monitor_id.is_none() || m.id().ok() == target.monitor_id)
        .ok_or("monitor not found")?;
    let [x, y, w, h] = target.region;
    let grab = || monitor.capture_region(x, y, w, h).map_err(|e| e.to_string());
    let center = (target.origin.0 + (x + w / 2) as i32, target.origin.1 + (y + h / 2) as i32);
    let restore = input::cursor_pos();
    let max_h = (MAX_BYTES / (w as u64 * 4).max(1)).min(60_000) as usize;

    let first = grab()?;
    let mut prev_sig = signatures(&first);
    let mut stitcher = Stitcher::new(first);
    progress.store(h, Ordering::Relaxed);
    let mut bands: Option<(usize, usize)> = None;
    let mut notches = 3i32;
    let mut still = 0;
    let mut notes = Vec::new();
    let mut step = 0usize;

    // The fixed (sticky) bands are learned from the 2nd and 3rd frames: on many pages
    // something changes on the *first* scroll (a header shrinks, a status line updates,
    // a "back to top" button appears), so the first pair isn't representative.
    let mut pending: Option<(RgbaImage, Vec<Sig>)> = None;

    for _ in 0..MAX_STEPS {
        if stop.load(Ordering::Relaxed) || input::escape_pressed() {
            notes.push("Stopped before the end".into());
            break;
        }
        input::scroll(center, notches);
        let frame = wait_until_still(&grab, &stop)?;
        let sig = signatures(&frame);
        step += 1;

        let b = match bands {
            Some(b) => b,
            None => match pending.take() {
                None => {
                    if fixed_bands(&prev_sig, &sig).is_none() {
                        // Didn't move yet (slow app, or nothing to scroll). Try a couple more.
                        crate::diag::log(&format!("scroll step {step}: nothing moved yet ({notches} notches)"));
                        still += 1;
                        if still >= 3 {
                            break;
                        }
                    } else {
                        still = 0;
                        pending = Some((frame, sig)); // wait for one more frame
                    }
                    continue;
                }
                Some((f1, s1)) => {
                    let Some(b) = fixed_bands(&s1, &sig).or_else(|| fixed_bands(&prev_sig, &s1)) else { break };
                    bands = Some(b);
                    match find_shift(&prev_sig, &s1, b.0, b.1) {
                        Shift::By(s) => {
                            crate::diag::log(&format!("scroll step 1: By({s}), fixed bands top={} bottom={}", b.0, b.1));
                            stitcher.push(f1, s, b);
                            prev_sig = s1;
                        }
                        other => {
                            crate::diag::log(&format!("scroll step 1: {other:?} — couldn't line up the first scroll"));
                            notes.push("Stopped: couldn't line up the first scroll".into());
                            break;
                        }
                    }
                    b
                }
            },
        };
        let shift = find_shift(&prev_sig, &sig, b.0, b.1);
        crate::diag::log(&format!("scroll step {step}: {shift:?}, fixed bands top={} bottom={}, {notches} notches", b.0, b.1));
        match shift {
            Shift::By(s) => {
                still = 0;
                let core = h as usize - b.0 - b.1;
                // Keep plenty of overlap: scroll less if a step moved most of the area,
                // more if it barely moved.
                if s * 10 > core * 7 && notches > 1 {
                    notches -= 1;
                } else if s * 4 < core && notches < 8 {
                    notches += 1;
                }
                stitcher.push(frame, s, b);
                prev_sig = sig;
                progress.store((stitcher.height() + b.1) as u32, Ordering::Relaxed);
                if stitcher.height() >= max_h {
                    notes.push("Reached the maximum image size".into());
                    break;
                }
            }
            Shift::None => {
                still += 1;
                if still >= 2 {
                    break; // reached the end
                }
            }
            Shift::Lost => {
                notes.push("Stopped: the content changed in a way that couldn't be lined up (animation or video?)".into());
                break;
            }
        }
    }
    // Only one scroll happened before the end: stitch it with the bands from that pair.
    if let Some((f1, s1)) = pending.take() {
        if let Some(b) = fixed_bands(&prev_sig, &s1) {
            if let Shift::By(s) = find_shift(&prev_sig, &s1, b.0, b.1) {
                stitcher.push(f1, s, b);
                bands = Some(b);
            }
        }
    }
    if let Some(p) = restore {
        input::set_cursor(p);
    }
    if bands.is_none() {
        notes.push("Nothing scrolled — select the scrollable part of the window (e.g. the page, not the whole window)".into());
    }
    Ok((stitcher.finish(), notes))
}

/// Captures until two consecutive frames are identical (smooth scrolling has settled), or
/// ~1.2 s passes.
fn wait_until_still(grab: &dyn Fn() -> Result<RgbaImage, String>, stop: &AtomicBool) -> Result<RgbaImage, String> {
    std::thread::sleep(Duration::from_millis(140));
    let start = Instant::now();
    let mut last = grab()?;
    loop {
        std::thread::sleep(Duration::from_millis(90));
        let next = grab()?;
        if next.as_raw() == last.as_raw() || start.elapsed() > Duration::from_millis(1200) || stop.load(Ordering::Relaxed) {
            return Ok(next);
        }
        last = next;
    }
}

/// Pointer and wheel input.
mod input {
    #[cfg(windows)]
    pub fn scroll((x, y): (i32, i32), notches: i32) {
        use windows::Win32::UI::Input::KeyboardAndMouse::{SendInput, INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_WHEEL, MOUSEINPUT};
        use windows::Win32::UI::WindowsAndMessaging::SetCursorPos;
        unsafe {
            let _ = SetCursorPos(x, y);
            let wheel = INPUT {
                r#type: INPUT_MOUSE,
                Anonymous: INPUT_0 {
                    mi: MOUSEINPUT { dx: 0, dy: 0, mouseData: (-120 * notches) as u32, dwFlags: MOUSEEVENTF_WHEEL, time: 0, dwExtraInfo: 0 },
                },
            };
            SendInput(&[wheel], std::mem::size_of::<INPUT>() as i32);
        }
    }

    #[cfg(windows)]
    pub fn escape_pressed() -> bool {
        use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_ESCAPE};
        unsafe { GetAsyncKeyState(VK_ESCAPE.0 as i32) as u16 & 0x8000 != 0 }
    }

    #[cfg(windows)]
    pub fn cursor_pos() -> Option<(i32, i32)> {
        crate::record::cursor_position()
    }

    #[cfg(windows)]
    pub fn set_cursor((x, y): (i32, i32)) {
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::SetCursorPos(x, y);
        }
    }

    // Elsewhere (used for testing on Linux): drive the pointer with xdotool if present.
    #[cfg(not(windows))]
    pub fn scroll((x, y): (i32, i32), notches: i32) {
        let _ = std::process::Command::new("xdotool")
            .args(["mousemove", &x.to_string(), &y.to_string(), "click", "--repeat", &notches.to_string(), "--delay", "15", "5"])
            .status();
    }

    #[cfg(not(windows))]
    pub fn escape_pressed() -> bool {
        false
    }

    #[cfg(not(windows))]
    pub fn cursor_pos() -> Option<(i32, i32)> {
        None
    }

    #[cfg(not(windows))]
    pub fn set_cursor(_p: (i32, i32)) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;

    /// A tall synthetic "page": every row has a unique pattern, with occasional blank rows.
    fn page(w: u32, h: u32) -> RgbaImage {
        RgbaImage::from_fn(w, h, |x, y| {
            if y % 37 == 0 {
                return Rgba([255, 255, 255, 255]); // blank line
            }
            let v = ((x * 7 + y * 13) ^ (y * y)) % 251;
            Rgba([v as u8, (v * 3 % 256) as u8, (255 - v) as u8, 255])
        })
    }

    /// The visible window of `page` scrolled to `offset`, with an optional sticky header and
    /// footer drawn on top (like a website's nav bar).
    fn view(page: &RgbaImage, offset: u32, h: u32, header: u32, footer: u32) -> RgbaImage {
        let w = page.width();
        RgbaImage::from_fn(w, h, |x, y| {
            if y < header {
                Rgba([30, 60, 200, 255])
            } else if y >= h - footer {
                Rgba([200, 60, 30, 255])
            } else {
                *page.get_pixel(x, offset + y)
            }
        })
    }

    #[test]
    fn finds_shift_between_frames() {
        let p = page(200, 3000);
        let a = signatures(&view(&p, 0, 400, 0, 0));
        let b = signatures(&view(&p, 137, 400, 0, 0));
        assert_eq!(find_shift(&a, &b, 0, 0), Shift::By(137));
        assert_eq!(find_shift(&a, &a, 0, 0), Shift::None);
    }

    #[test]
    fn stitches_a_page_exactly() {
        let p = page(160, 2000);
        let h = 300;
        let offsets = [0u32, 120, 260, 410, 555, 700, 840, 990, 1130, 1280, 1420, 1560, 1700];
        let frames: Vec<_> = offsets.iter().map(|&o| view(&p, o, h, 0, 0)).collect();
        let mut st = Stitcher::new(frames[0].clone());
        let mut prev = signatures(&frames[0]);
        for f in &frames[1..] {
            let s = signatures(f);
            let bands = fixed_bands(&prev, &s).unwrap();
            let Shift::By(shift) = find_shift(&prev, &s, bands.0, bands.1) else { panic!("lost") };
            st.push(f.clone(), shift, bands);
            prev = s;
        }
        let out = st.finish();
        let expected = image::imageops::crop_imm(&p, 0, 0, 160, 1700 + h).to_image();
        assert_eq!((out.width(), out.height()), (expected.width(), expected.height()));
        assert!(out.as_raw() == expected.as_raw(), "stitched image differs from the page");
    }

    #[test]
    fn sticky_header_and_footer_appear_once() {
        let p = page(160, 1500);
        let (h, header, footer) = (320, 40, 24);
        let offsets = [0u32, 150, 300, 450, 600];
        let frames: Vec<_> = offsets.iter().map(|&o| view(&p, o, h, header, footer)).collect();
        let mut st = Stitcher::new(frames[0].clone());
        let mut prev = signatures(&frames[0]);
        let mut bands = None;
        for f in &frames[1..] {
            let s = signatures(f);
            let b = *bands.get_or_insert_with(|| fixed_bands(&prev, &s).unwrap());
            assert_eq!(b, (header as usize, footer as usize));
            let Shift::By(shift) = find_shift(&prev, &s, b.0, b.1) else { panic!("lost") };
            assert_eq!(shift, 150);
            st.push(f.clone(), shift, b);
            prev = s;
        }
        let out = st.finish();
        // header + page rows [header .. 600 + h - footer) + footer
        assert_eq!(out.height(), header + (600 + h - footer - header) + footer);
        assert_eq!(*out.get_pixel(5, 5), Rgba([30, 60, 200, 255]), "header at the top");
        assert_eq!(*out.get_pixel(5, out.height() - 3), Rgba([200, 60, 30, 255]), "footer at the bottom");
        // the middle is the page, continuous
        for y in [header, 200, 500, 700, out.height() - footer - 1] {
            assert_eq!(out.get_pixel(17, y), p.get_pixel(17, y), "row {y}");
        }
    }
}
