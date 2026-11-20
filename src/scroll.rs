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

/// Number of column bins in a row signature.
const BINS: usize = 64;
/// Mean per-bin difference (0-255 luma) under which two rows count as the same.
/// Browsers don't re-render scrolled content bit-for-bit identically (anti-aliasing,
/// colour-managed images, fractional display scaling), so matching must be tolerant.
const MATCH_TOL: u32 = 4;
/// Tolerance for "this row didn't change at all" (sticky bands, end of page).
const SAME_TOL: u32 = 2;

/// Per-row fingerprint: the average brightness of `BINS` column bins, and whether the row
/// is "informative" (has some detail; blank rows match anything, so they aren't evidence).
#[derive(Clone)]
pub struct Sig {
    bins: [u8; BINS],
    info: bool,
}

/// Mean absolute per-bin difference between two rows.
fn dist(a: &Sig, b: &Sig) -> u32 {
    a.bins.iter().zip(&b.bins).map(|(&x, &y)| x.abs_diff(y) as u32).sum::<u32>() / BINS as u32
}

pub fn signatures(img: &RgbaImage) -> Vec<Sig> {
    let w = img.width() as usize;
    let core = if w > SCROLLBAR_PX as usize * 3 { w - SCROLLBAR_PX as usize } else { w };
    let raw = img.as_raw();
    let stride = w * 4;
    let nb = BINS.min(core.max(1));
    (0..img.height() as usize)
        .map(|y| {
            let row = &raw[y * stride..y * stride + core * 4];
            let mut bins = [0u8; BINS];
            for (j, bin) in bins.iter_mut().enumerate().take(nb) {
                let (c0, c1) = (j * core / nb, ((j + 1) * core / nb).max(j * core / nb + 1));
                let sum: u32 = row[c0 * 4..c1 * 4]
                    .chunks_exact(4)
                    .map(|p| (p[0] as u32 * 77 + p[1] as u32 * 150 + p[2] as u32 * 29) >> 8)
                    .sum();
                *bin = (sum / (c1 - c0) as u32) as u8;
            }
            let (mn, mx) = bins[..nb].iter().fold((255u8, 0u8), |(a, b), &v| (a.min(v), b.max(v)));
            Sig { bins, info: mx - mn >= 6 }
        })
        .collect()
}

/// Rows at the top and bottom that didn't change between two frames: sticky header /
/// footer. Each band is capped at 35 % of the height. Returns `None` if (almost) nothing
/// changed, i.e. the content didn't move.
pub fn fixed_bands(a: &[Sig], b: &[Sig]) -> Option<(usize, usize)> {
    let h = a.len().min(b.len());
    let cap = h * 35 / 100;
    let same = |i: usize| dist(&a[i], &b[i]) <= SAME_TOL;
    let top = (0..h).take_while(|&i| same(i)).count();
    if top >= h.saturating_sub(4) {
        return None; // nothing moved
    }
    let bottom = (0..h).take_while(|&i| same(h - 1 - i)).count();
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

/// Best candidate alignment, for the diagnostics log.
#[derive(Clone, Copy)]
pub struct Match {
    pub shift: usize,
    /// Percentage of informative overlapping rows that matched.
    pub percent: usize,
    /// Informative rows compared.
    pub rows: usize,
}

impl std::fmt::Debug for Match {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "shift {} matched {}% of {} rows", self.shift, self.percent, self.rows)
    }
}

/// How far the content moved from `a` to `b`, ignoring the fixed bands.
pub fn find_shift(a: &[Sig], b: &[Sig], top: usize, bottom: usize) -> Shift {
    analyse(a, b, top, bottom).0
}

/// `find_shift`, plus the best alignment it found.
pub fn analyse(a: &[Sig], b: &[Sig], top: usize, bottom: usize) -> (Shift, Option<Match>) {
    let h = a.len().min(b.len());
    let (lo, hi) = (top, h.saturating_sub(bottom));
    if hi <= lo + 16 {
        return (Shift::Lost, None);
    }
    // Unchanged?
    let (same, info) = (lo..hi).fold((0, 0), |(m, t), r| {
        if a[r].info {
            (m + (dist(&a[r], &b[r]) <= SAME_TOL) as usize, t + 1)
        } else {
            (m, t)
        }
    });
    if info == 0 || same * 100 >= info * 98 {
        return (Shift::None, None);
    }
    // Score every shift by its average (capped) row difference; capping keeps a changing
    // region (an animated ad, a video) from swamping an otherwise perfect match.
    let core = hi - lo;
    let mut best: Option<(usize, usize, usize, u64)> = None; // (shift, matches, rows, cost)
    for s in 1..core.saturating_sub(8) {
        let (mut m, mut t, mut cost) = (0usize, 0usize, 0u64);
        for r in lo..hi - s {
            let (ra, rb) = (&a[r + s], &b[r]);
            if ra.info || rb.info {
                let d = dist(ra, rb);
                t += 1;
                m += (d <= MATCH_TOL) as usize;
                cost += d.min(40) as u64;
            }
        }
        if t < 12 {
            continue;
        }
        let better = match best {
            None => true,
            // lower average cost wins; ties → more evidence
            Some((_, _, bt, bc)) => cost * (bt as u64) < bc * (t as u64) || (cost * bt as u64 == bc * t as u64 && t > bt),
        };
        if better {
            best = Some((s, m, t, cost));
        }
    }
    let Some((s, m, t, _)) = best else { return (Shift::Lost, None) };
    let info = Match { shift: s, percent: m * 100 / t, rows: t };
    if m * 100 >= t * 70 {
        (Shift::By(s), Some(info))
    } else {
        (Shift::Lost, Some(info))
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

    // Put the pointer over the area first (hover effects, overlay fade-out), then take
    // the first frame once the screen is still.
    input::set_cursor(center);
    let first = wait_until_still(&grab, &stop)?;
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
                    let Some(mut b) = fixed_bands(&s1, &sig).or_else(|| fixed_bands(&prev_sig, &s1)) else { break };
                    let mut res = analyse(&prev_sig, &s1, b.0, b.1);
                    if !matches!(res.0, Shift::By(_)) {
                        // Try the bands of the first pair instead.
                        if let Some(b0) = fixed_bands(&prev_sig, &s1) {
                            let r0 = analyse(&prev_sig, &s1, b0.0, b0.1);
                            if matches!(r0.0, Shift::By(_)) {
                                (b, res) = (b0, r0);
                            }
                        }
                    }
                    bands = Some(b);
                    match res.0 {
                        Shift::By(s) => {
                            crate::diag::log(&format!("scroll step 1: By({s}), fixed bands top={} bottom={}, {:?}", b.0, b.1, res.1));
                            stitcher.push(f1, s, b);
                            prev_sig = s1;
                        }
                        other => {
                            crate::diag::log(&format!(
                                "scroll step 1: {other:?} — couldn't line up the first scroll; bands top={} bottom={}, best {:?}",
                                b.0, b.1, res.1
                            ));
                            notes.push("Stopped: couldn't line up the first scroll".into());
                            break;
                        }
                    }
                    b
                }
            },
        };
        let (mut shift, mut best) = analyse(&prev_sig, &sig, b.0, b.1);
        let (mut frame, mut sig) = (frame, sig);
        if shift == Shift::Lost {
            // Something may still have been settling (lazy-loaded images, animations): look again.
            crate::diag::log(&format!("scroll step {step}: Lost (best {best:?}), retrying"));
            std::thread::sleep(Duration::from_millis(500));
            frame = wait_until_still(&grab, &stop)?;
            sig = signatures(&frame);
            (shift, best) = analyse(&prev_sig, &sig, b.0, b.1);
        }
        crate::diag::log(&format!(
            "scroll step {step}: {shift:?}, fixed bands top={} bottom={}, {notches} notches, best {best:?}",
            b.0, b.1
        ));
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

    /// Re-rendering noise: every pixel nudged by a small pseudo-random amount, different
    /// per frame (what browsers do with anti-aliasing / colour management).
    fn noisy(img: &RgbaImage, seed: u32) -> RgbaImage {
        let mut out = img.clone();
        for (i, px) in out.pixels_mut().enumerate() {
            let n = ((i as u32).wrapping_mul(2_654_435_761).wrapping_add(seed.wrapping_mul(40_503)) >> 27) as i32 - 16; // -16..15
            let d = n / 3; // about ±5
            for c in 0..3 {
                px.0[c] = (px.0[c] as i32 + d).clamp(0, 255) as u8;
            }
        }
        out
    }

    #[test]
    fn tolerates_rendering_noise_and_a_changing_region() {
        let p = page(400, 3000);
        let a = view(&p, 0, 600, 0, 0);
        let mut b = noisy(&view(&p, 211, 600, 0, 0), 7);
        // an "animated ad": a block in the middle that differs completely
        for y in 250..330 {
            for x in 0..400 {
                b.put_pixel(x, y, Rgba([(x * 3 % 256) as u8, 0, (y % 256) as u8, 255]));
            }
        }
        let (shift, m) = analyse(&signatures(&a), &signatures(&b), 0, 0);
        assert_eq!(shift, Shift::By(211), "{m:?}");
        // and noise alone doesn't count as movement
        assert_eq!(find_shift(&signatures(&a), &signatures(&noisy(&a, 3)), 0, 0), Shift::None);
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

    /// Real browser frames: `SNIPPY_SCROLL_FRAMES=dir` with f0.png, f1.png, … taken while
    /// scrolling. Run with `cargo test real_frames -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn real_frames() {
        let dir = std::env::var("SNIPPY_SCROLL_FRAMES").expect("SNIPPY_SCROLL_FRAMES");
        let frames: Vec<RgbaImage> = (0..)
            .map_while(|i| image::open(format!("{dir}/f{i}.png")).ok().map(|im| im.to_rgba8()))
            .collect();
        assert!(frames.len() >= 3);
        let sigs: Vec<_> = frames.iter().map(signatures).collect();
        let b = fixed_bands(&sigs[1], &sigs[2]).unwrap();
        println!("bands {b:?}");
        let mut st = Stitcher::new(frames[0].clone());
        for i in 1..frames.len() {
            let (shift, m) = analyse(&sigs[i - 1], &sigs[i], b.0, b.1);
            println!("frame {i}: {shift:?} {m:?}");
            let Shift::By(s) = shift else { panic!("frame {i}: {shift:?}") };
            st.push(frames[i].clone(), s, b);
        }
        let out = st.finish();
        out.save(format!("{dir}/stitched.png")).unwrap();
        println!("stitched {}x{}", out.width(), out.height());
    }
}
