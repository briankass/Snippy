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
const BINS: usize = 128;
/// Mean per-bin brightness difference (0-255 luma) under which two rows (or strips of
/// rows) count as the same. Browsers don't re-render scrolled content bit-for-bit
/// identically (anti-aliasing, colour management, fractional display scaling).
const MATCH_TOL: u32 = 4;
/// Tolerance for "this didn't change at all" (sticky bands, end of page).
const SAME_TOL: u32 = 2;
/// Vertical strips a row is split into. Each strip works out the scroll on its own and
/// the strips vote, so a strip that doesn't scroll or keeps changing (a sticky side
/// column with ads, a floating video player) is simply outvoted.
const STRIPS: usize = 8;

/// Per-row fingerprint: the average brightness of up to `BINS` column bins, and which
/// strips have some detail (blank strips match anything, so they aren't evidence).
#[derive(Clone)]
pub struct Sig {
    bins: [u8; BINS],
    /// Bins in use (fewer than `BINS` only for very narrow areas).
    n: u8,
    /// Bit `g` set: strip `g` has detail.
    strips: u8,
}

/// Bin range of strip `g`.
fn strip(n: usize, g: usize) -> std::ops::Range<usize> {
    g * n / STRIPS..(g + 1) * n / STRIPS
}

/// Total absolute per-bin difference over `range`.
fn diff(a: &Sig, b: &Sig, range: std::ops::Range<usize>) -> u32 {
    a.bins[range.clone()].iter().zip(&b.bins[range]).map(|(&x, &y)| x.abs_diff(y) as u32).sum()
}

/// Whole rows within a mean per-bin tolerance.
fn within(a: &Sig, b: &Sig, tol: u32) -> bool {
    let n = a.n.min(b.n) as usize;
    diff(a, b, 0..n) <= tol * n as u32
}

/// Evidence that the strips of `a` and `b` are unchanged: (unchanged rows, detailed rows)
/// summed over the strips that are mostly unchanged, and the detailed rows of all strips.
/// A strip counts as unchanged if 95 % of its detailed rows are. If at least half of the
/// detailed strips are unchanged, everything counts as unchanged.
fn unchanged_evidence(a: &[Sig], b: &[Sig]) -> (usize, usize) {
    let mut same = [0usize; STRIPS];
    let mut info = [0usize; STRIPS];
    for (x, y) in a.iter().zip(b) {
        let n = x.n.min(y.n) as usize;
        let mask = x.strips | y.strips;
        for g in 0..STRIPS {
            if mask & (1 << g) != 0 {
                let r = strip(n, g);
                let len = r.len().max(1) as u32;
                info[g] += 1;
                same[g] += (diff(x, y, r) <= SAME_TOL * len) as usize;
            }
        }
    }
    let still: usize = (0..STRIPS).filter(|&g| info[g] > 0 && same[g] * 100 >= info[g] * 95).map(|g| info[g]).sum();
    let total: usize = info.iter().sum();
    // Also by strip count: at the end of a page with a busy sticky column (lots of detail,
    // always changing) next to a quiet footer, the unchanged strips may hold less detail.
    let detailed = (0..STRIPS).filter(|&g| info[g] >= 8).count();
    let still_strips = (0..STRIPS).filter(|&g| info[g] >= 8 && same[g] * 100 >= info[g] * 95).count();
    if detailed > 0 && still_strips * 2 >= detailed && still_strips >= 2 {
        return (total, total);
    }
    (still, total)
}

pub fn signatures(img: &RgbaImage) -> Vec<Sig> {
    let w = img.width() as usize;
    let core = if w > SCROLLBAR_PX as usize * 3 { w - SCROLLBAR_PX as usize } else { w };
    let raw = img.as_raw();
    let stride = w * 4;
    let nb = BINS.min(core.max(1)); // ≤ 128, fits in u8
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
            let detail = |r: std::ops::Range<usize>| {
                let (mn, mx) = bins[r].iter().fold((255u8, 0u8), |(a, b), &v| (a.min(v), b.max(v)));
                mx.saturating_sub(mn) >= 6
            };
            let mut strips = 0u8;
            for g in 0..STRIPS {
                let r = strip(nb, g);
                if !r.is_empty() && detail(r) {
                    strips |= 1 << g;
                }
            }
            Sig { bins, n: nb as u8, strips }
        })
        .collect()
}

/// Rows at the top and bottom that didn't change between two frames: sticky header /
/// footer. Each band is capped at 35 % of the height. Returns `None` if (almost) nothing
/// changed, i.e. the content didn't move.
pub fn fixed_bands(a: &[Sig], b: &[Sig]) -> Option<(usize, usize)> {
    let h = a.len().min(b.len());
    let cap = h * 35 / 100;
    let same = |i: usize| within(&a[i], &b[i], SAME_TOL);
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
    /// Average row difference at this shift, and at the best other shift.
    pub cost: f64,
    pub runner: f64,
    /// Strips that agreed on this shift, of those with detail.
    pub strips: (usize, usize),
}

impl std::fmt::Debug for Match {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(
            f,
            "shift {} matched {}% of {} rows, cost {:.1} vs {:.1}, strips {}/{}",
            self.shift, self.percent, self.rows, self.cost, self.runner, self.strips.0, self.strips.1
        )
    }
}

/// How far the content moved from `a` to `b`, ignoring the fixed bands.
pub fn find_shift(a: &[Sig], b: &[Sig], top: usize, bottom: usize) -> Shift {
    analyse(a, b, top, bottom, None).0
}

/// `find_shift`, plus the best alignment it found.
///
/// `expect` is roughly how far the content should have moved (from earlier steps). It
/// settles ambiguous cases, e.g. a smooth gradient that matches itself at many shifts.
#[allow(clippy::needless_range_loop)] // tables indexed [shift][strip]
pub fn analyse(a: &[Sig], b: &[Sig], top: usize, bottom: usize, expect: Option<usize>) -> (Shift, Option<Match>) {
    let h = a.len().min(b.len());
    let (lo, hi) = (top, h.saturating_sub(bottom));
    if hi <= lo + 16 {
        return (Shift::Lost, None);
    }
    // Unchanged? (A playing video or animated ad can keep a strip changing.)
    let (still, total) = unchanged_evidence(&a[lo..hi], &b[lo..hi]);
    if total == 0 || still * 100 >= total * 60 {
        return (Shift::None, None);
    }
    // Per shift and strip: (matching rows, detailed rows, capped cost).
    let core = hi - lo;
    let max_s = core.saturating_sub(8);
    let mut stats = vec![[(0u32, 0u32, 0u32); STRIPS]; max_s.max(1)];
    for (s, st) in stats.iter_mut().enumerate().skip(1) {
        for r in lo..hi - s {
            let (ra, rb) = (&a[r + s], &b[r]);
            let mask = ra.strips | rb.strips;
            if mask == 0 {
                continue;
            }
            let n = ra.n.min(rb.n) as usize;
            for (g, e) in st.iter_mut().enumerate() {
                if mask & (1 << g) != 0 {
                    let rg = strip(n, g);
                    let len = rg.len().max(1) as u32;
                    let d = diff(ra, rb, rg) / len; // mean per-bin difference
                    e.1 += 1;
                    e.0 += (d <= MATCH_TOL) as u32;
                    e.2 += d.min(40);
                }
            }
        }
    }
    // Scores (shift, matches, rows, average cost) over a set of strips; shifts compared on
    // too few rows are left out (a handful of rows can match by chance).
    let scores_for = |set: &[usize]| -> Vec<(usize, usize, usize, f64)> {
        let sums: Vec<(usize, usize, usize, u64)> = (1..max_s)
            .map(|s| {
                let (m, t, c) = set.iter().fold((0usize, 0usize, 0u64), |(m, t, c), &g| {
                    let e = stats[s][g];
                    (m + e.0 as usize, t + e.1 as usize, c + e.2 as u64)
                });
                (s, m, t, c)
            })
            .collect();
        // Evidence shrinks with the overlap; judge each shift against what its overlap
        // could offer, and never trust a sliver of overlap.
        let max_t = sums.iter().map(|x| x.2).max().unwrap_or(0);
        sums.into_iter()
            .filter(|x| core - x.0 >= core / 8 && x.2 >= 12.max(max_t * (core - x.0) / core / 3))
            .map(|(s, m, t, c)| (s, m, t, c as f64 / t as f64))
            .collect()
    };
    let lowest = |sc: &[(usize, usize, usize, f64)]| sc.iter().copied().min_by(|x, y| x.3.total_cmp(&y.3));
    // Each strip lists the shifts it accepts (it lines up well there, nearly as well as at
    // its best). A strip of text accepts one; a smooth gradient accepts many; a strip with
    // a sticky column or a video accepts none. The shift most of the detail accepts wins.
    let good_vs = |c0: f64, best: f64| c0 <= best * 1.6 + 1.5;
    let mut accepts = vec![[0u32; STRIPS]; max_s.max(1)]; // weight per shift and strip
    let mut detailed = 0;
    for g in 0..STRIPS {
        let sc = scores_for(&[g]);
        let Some((_, _, _, best)) = lowest(&sc) else { continue };
        detailed += 1;
        for &(s, m, t, c) in &sc {
            if good_vs(c, best) && m * 100 >= t * 70 {
                accepts[s][g] = t as u32;
            }
        }
    }
    // Weight of shift s: each strip accepting s (± 2 rows) counts with its evidence.
    let weight = |s: usize| -> (usize, Vec<usize>) {
        let (lo_s, hi_s) = (s.saturating_sub(2).max(1), (s + 2).min(max_s.saturating_sub(1)));
        let mut w = 0;
        let mut set = Vec::new();
        for g in 0..STRIPS {
            let best = (lo_s..=hi_s).map(|x| accepts[x][g]).max().unwrap_or(0);
            if best > 0 {
                w += best as usize;
                set.push(g);
            }
        }
        (w, set)
    };
    let weights: Vec<(usize, usize)> = (1..max_s).map(|s| (s, weight(s).0)).collect();
    let max_w = weights.iter().map(|x| x.1).max().unwrap_or(0);
    // Among the (near-)top shifts, prefer the expected one, else the first.
    let top: Vec<usize> = weights.iter().filter(|x| x.1 > 0 && x.1 * 10 >= max_w * 9).map(|x| x.0).collect();
    let chosen = match expect {
        Some(e) => top.iter().copied().min_by_key(|s| s.abs_diff(e)),
        None => top.first().copied(),
    };
    let agree: Vec<usize> = match chosen {
        // The agreeing strips must carry a fair share of the area's detail.
        // (compared with all the detail that overlaps at that shift)
        Some(s) if max_w * 100 >= stats[s].iter().map(|e| e.1 as usize).sum::<usize>() * 30 => weight(s).1,
        _ => {
            let all: Vec<usize> = (0..STRIPS).collect();
            let sc = scores_for(&all);
            let info = lowest(&sc).map(|(s, m, t, c)| Match { shift: s, percent: m * 100 / t.max(1), rows: t, cost: c, runner: f64::NAN, strips: (0, detailed) });
            return (Shift::Lost, info);
        }
    };
    let scores = scores_for(&agree);
    let near = chosen.unwrap_or(0);
    let Some((s, m, t, c)) = lowest(&scores.iter().copied().filter(|x| x.0.abs_diff(near) <= 2).collect::<Vec<_>>()) else {
        return (Shift::Lost, None);
    };
    // It must also stand out: lines of text look alike, so a wrong shift can match a fair
    // share of rows. The real one is far better than any other (not neighbouring) shift.
    let runner = scores.iter().filter(|x| x.0.abs_diff(s) > 4).map(|x| x.3).fold(f64::INFINITY, f64::min);
    let good = |c0: f64| c0 <= c * 1.6 + 1.5;
    let enough = |m: usize, t: usize| m * 100 >= t * 80 || (t >= 300 && m * 100 >= t * 70);
    let strips = (agree.len(), detailed);
    let info = Match { shift: s, percent: m * 100 / t, rows: t, cost: c, runner, strips };
    if !enough(m, t) {
        return (Shift::Lost, Some(info));
    }
    if !good(runner) {
        return (Shift::By(s), Some(info));
    }
    // Ambiguous: take the good candidate closest to the expected shift, if it's near it.
    if let Some(e) = expect {
        let pick = scores.iter().filter(|x| good(x.3) && enough(x.1, x.2)).min_by_key(|x| x.0.abs_diff(e));
        if let Some(&(s2, m2, t2, c2)) = pick {
            if s2.abs_diff(e) <= e / 4 + 8 {
                return (Shift::By(s2), Some(Match { shift: s2, percent: m2 * 100 / t2, rows: t2, cost: c2, runner, strips }));
            }
        }
    }
    (Shift::Lost, Some(info))
}

/// Percentage of detailed strip-rows that line up if the content moved by exactly `s`.
pub fn match_at(a: &[Sig], b: &[Sig], top: usize, bottom: usize, s: usize) -> usize {
    let h = a.len().min(b.len());
    let (lo, hi) = (top, h.saturating_sub(bottom));
    if s == 0 || hi <= lo + s {
        return 0;
    }
    let (mut m, mut t) = (0usize, 0usize);
    for r in lo..hi - s {
        let (ra, rb) = (&a[r + s], &b[r]);
        let mask = ra.strips | rb.strips;
        let n = ra.n.min(rb.n) as usize;
        for g in 0..STRIPS {
            if mask & (1 << g) != 0 {
                let rg = strip(n, g);
                let len = rg.len().max(1) as u32;
                t += 1;
                m += (diff(ra, rb, rg) / len <= MATCH_TOL) as usize;
            }
        }
    }
    (m * 100).checked_div(t).unwrap_or(0)
}

/// Like `match_at`, but ignoring brightness and contrast: the percentage of detailed
/// strip-rows whose *pattern* lines up (correlation ≥ 0.95). A page dimmed behind a pop-up,
/// or under a translucent banner, still lines up this way.
pub fn shape_match_at(a: &[Sig], b: &[Sig], top: usize, bottom: usize, s: usize) -> usize {
    let h = a.len().min(b.len());
    let (lo, hi) = (top, h.saturating_sub(bottom));
    if hi <= lo + s + 8 {
        return 0;
    }
    let (mut m, mut t) = (0usize, 0usize);
    for r in lo..hi - s {
        let (ra, rb) = (&a[r + s], &b[r]);
        let mask = ra.strips & rb.strips;
        let n = ra.n.min(rb.n) as usize;
        for g in 0..STRIPS {
            if mask & (1 << g) == 0 {
                continue;
            }
            let rg = strip(n, g);
            let len = rg.len().max(1) as f64;
            let (xa, xb) = (&ra.bins[rg.clone()], &rb.bins[rg]);
            let ma = xa.iter().map(|&v| v as f64).sum::<f64>() / len;
            let mb = xb.iter().map(|&v| v as f64).sum::<f64>() / len;
            let (mut sab, mut saa, mut sbb) = (0.0, 0.0, 0.0);
            for (&x, &y) in xa.iter().zip(xb) {
                let (dx, dy) = (x as f64 - ma, y as f64 - mb);
                sab += dx * dy;
                saa += dx * dx;
                sbb += dy * dy;
            }
            t += 1;
            if saa > 0.0 && sbb > 0.0 && sab / (saa * sbb).sqrt() >= 0.95 {
                m += 1;
            }
        }
    }
    (m * 100).checked_div(t).unwrap_or(0)
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
    // Where the pointer sits while scrolling: near the right edge of the area (usually the
    // scrollbar or a margin), not the middle, where it would hover links and make the
    // browser show their address in the corner of every frame.
    let inset = 6.min(w / 2);
    let center = (target.origin.0 + (x + w - 1 - inset) as i32, target.origin.1 + (y + h / 2) as i32);
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
    // Start small (how far a notch scrolls varies a lot between apps) and speed up.
    let mut notches = 1i32;
    // Pixels one notch scrolled, from clean steps (for settling ambiguous matches).
    let mut per_notch: Option<f64> = None;
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
                    let mut res = analyse(&prev_sig, &s1, b.0, b.1, None);
                    if !matches!(res.0, Shift::By(_)) {
                        // Try the bands of the first pair instead.
                        if let Some(b0) = fixed_bands(&prev_sig, &s1) {
                            let r0 = analyse(&prev_sig, &s1, b0.0, b0.1, None);
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
                            per_notch = Some(s as f64 / notches as f64);
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
        let expect = per_notch.map(|p| (p * notches as f64) as usize);
        let (mut shift, mut best) = analyse(&prev_sig, &sig, b.0, b.1, expect);
        let (mut frame, mut sig) = (frame, sig);
        // Recovery: the page may still have been settling, or it scrolled further than the
        // area's height (heavy pages can jump a long way), leaving no overlap. Look again,
        // then step back one notch at a time until the frames line up.
        let mut tries = 0;
        let mut covered = false;
        let mut net = 0i32; // notches scrolled while recovering
        // Back up at most about four area-heights.
        let core_h = h as usize - b.0 - b.1;
        let back_limit = per_notch.map_or(6, |p| ((4 * core_h) as f64 / p.max(1.0)).ceil() as i32).clamp(4, 12);
        while shift == Shift::Lost && tries < back_limit as usize + 4 && !stop.load(Ordering::Relaxed) {
            tries += 1;
            // Is the page where we expected, but looking different? Then something covered
            // or changed it (a pop-up, a cookie banner, an ad, the page dimming behind a
            // dialog); scrolling back can't help.
            // (Checked once, before backing up: afterwards "where expected" means nothing.)
            if tries == 3 {
                let e = expect.unwrap_or(0);
                let plain = match_at(&prev_sig, &sig, b.0, b.1, e);
                // (Pop-ups often stop the page scrolling, so also check "didn't move".)
                let shape = shape_match_at(&prev_sig, &sig, b.0, b.1, e).max(shape_match_at(&prev_sig, &sig, b.0, b.1, 0));
                if plain < 70 && (plain >= 20 || shape >= 60) {
                    crate::diag::log(&format!(
                        "scroll step {step}: page covered or changed (expected shift {e}: {plain}% match, {shape}% by pattern)"
                    ));
                    covered = true;
                    break;
                }
            }
            let dir = if tries <= 2 {
                0 // look again (pop-ups and lazy content animate in)
            } else if matches!(find_shift(&sig, &prev_sig, b.0, b.1), Shift::By(_)) {
                1 // we're above the previous frame: down a notch
            } else if net > -back_limit {
                -1 // past it: up a notch
            } else {
                break; // backed up far enough: give up
            };
            crate::diag::log(&format!("scroll step {step}: Lost (best {best:?}), recovering (try {tries}, scroll {dir})"));
            if dir == 0 {
                std::thread::sleep(Duration::from_millis(if tries == 1 { 400 } else { 900 }));
            } else {
                input::scroll(center, dir);
                net += dir;
            }
            frame = wait_until_still(&grab, &stop)?;
            sig = signatures(&frame);
            (shift, best) = analyse(&prev_sig, &sig, b.0, b.1, None);
            if shift == Shift::None && dir != 0 {
                // back exactly where we were: go down a notch
                input::scroll(center, 1);
                net += 1;
                frame = wait_until_still(&grab, &stop)?;
                sig = signatures(&frame);
                (shift, best) = analyse(&prev_sig, &sig, b.0, b.1, None);
            }
            if matches!(shift, Shift::By(_)) {
                notches = 1; // smaller steps from here on
            }
        }
        if shift == Shift::Lost && net < 0 {
            // Put the page back where it was.
            input::scroll(center, -net);
        }
        crate::diag::log(&format!(
            "scroll step {step}: {shift:?}, fixed bands top={} bottom={}, {notches} notches, best {best:?}",
            b.0, b.1
        ));
        match shift {
            Shift::By(s) => {
                still = 0;
                if tries == 0 {
                    per_notch = Some(s as f64 / notches as f64);
                }
                let core = h as usize - b.0 - b.1;
                // Keep plenty of overlap: scroll less if a step moved most of the area,
                // more if it barely moved.
                if s * 2 > core && notches > 1 {
                    notches -= 1;
                } else if s * 5 < core && notches < 6 {
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
                // (Pages sometimes ignore a scroll; the end is three in a row.)
                if still >= 3 {
                    break; // reached the end
                }
            }
            Shift::Lost => {
                notes.push(if covered {
                    "Stopped: something covered the page (a pop-up or ad?). Close it and try again".into()
                } else {
                    "Stopped: the page changed in a way that couldn't be lined up (a pop-up, or content loading?)".into()
                });
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

/// Captures until the content stops moving (smooth-scrolling animations have settled), or
/// ~2 s pass. "Stopped" tolerates a changing part of the area (a playing video, an
/// animated ad): at least 75 % of the detailed rows must be unchanged between two looks.
fn wait_until_still(grab: &dyn Fn() -> Result<RgbaImage, String>, stop: &AtomicBool) -> Result<RgbaImage, String> {
    std::thread::sleep(Duration::from_millis(140));
    let start = Instant::now();
    let mut last = grab()?;
    let mut last_sig = signatures(&last);
    let mut calm = 0;
    loop {
        std::thread::sleep(Duration::from_millis(90));
        let next = grab()?;
        #[cfg(test)]
        if let Ok(d) = std::env::var("SNIPPY_SCROLL_DUMP") {
            static N: AtomicU32 = AtomicU32::new(0);
            let _ = next.save(format!("{d}/g{:03}.png", N.fetch_add(1, Ordering::Relaxed)));
        }
        if next.as_raw() == last.as_raw() || start.elapsed() > Duration::from_millis(2000) || stop.load(Ordering::Relaxed) {
            return Ok(next);
        }
        let sig = signatures(&next);
        // Mostly unchanged twice in a row (one look can catch a half-finished repaint).
        if mostly_unchanged(&last_sig, &sig) {
            calm += 1;
            if calm >= 2 {
                return Ok(next);
            }
        } else {
            calm = 0;
        }
        (last, last_sig) = (next, sig);
    }
}

/// True if most of the area (by detail) is unchanged; a strip with a playing video or
/// animated ad may keep changing.
fn mostly_unchanged(a: &[Sig], b: &[Sig]) -> bool {
    let (still, total) = unchanged_evidence(a, b);
    total == 0 || still * 100 >= total * 60
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
        let button = if notches < 0 { "4" } else { "5" };
        let _ = std::process::Command::new("xdotool")
            .args(["mousemove", &x.to_string(), &y.to_string(), "click", "--repeat", &notches.abs().to_string(), "--delay", "15", button])
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
        let (shift, m) = analyse(&signatures(&a), &signatures(&b), 0, 0, None);
        assert_eq!(shift, Shift::By(211), "{m:?}");
        // and noise alone doesn't count as movement
        assert_eq!(find_shift(&signatures(&a), &signatures(&noisy(&a, 3)), 0, 0), Shift::None);
    }

    /// Paints a fixed right-hand column (sticky ads / floating video) that changes per frame.
    fn sticky_rail(img: &mut RgbaImage, frame: u32) {
        let w = img.width();
        for y in 0..img.height() {
            for x in w * 3 / 4..w {
                let v = ((x * 31 + y * 17 + frame * 97) % 256) as u8;
                img.put_pixel(x, y, Rgba([v, v / 2, 255 - v, 255]));
            }
        }
    }

    #[test]
    fn sticky_changing_side_column_does_not_spoil_matching() {
        let p = page(480, 3000);
        let mut a = view(&p, 0, 600, 0, 0);
        let mut b = noisy(&view(&p, 233, 600, 0, 0), 5);
        sticky_rail(&mut a, 0);
        sticky_rail(&mut b, 1);
        let (shift, m) = analyse(&signatures(&a), &signatures(&b), 0, 0, None);
        assert_eq!(shift, Shift::By(233), "{m:?}");
    }

    #[test]
    fn page_that_did_not_move_but_has_a_playing_video_is_still() {
        let p = page(480, 3000);
        let mut a = view(&p, 400, 600, 0, 0);
        let mut b = noisy(&a, 9);
        sticky_rail(&mut a, 0);
        sticky_rail(&mut b, 1);
        assert_eq!(find_shift(&signatures(&a), &signatures(&b), 0, 0), Shift::None);
        assert!(mostly_unchanged(&signatures(&a), &signatures(&b)));
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
            let (shift, m) = analyse(&sigs[i - 1], &sigs[i], b.0, b.1, None);
            println!("frame {i}: {shift:?} {m:?}");
            let Shift::By(s) = shift else { panic!("frame {i}: {shift:?}") };
            st.push(frames[i].clone(), s, b);
        }
        let out = st.finish();
        out.save(format!("{dir}/stitched.png")).unwrap();
        println!("stitched {}x{}", out.width(), out.height());
    }

    /// End-to-end on a live display: `SNIPPY_SCROLL_LIVE="x,y,w,h"` (first monitor) and
    /// `SNIPPY_SCROLL_OUT=file.png`. Run with `cargo test run_live -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn run_live() {
        let r: Vec<u32> = std::env::var("SNIPPY_SCROLL_LIVE").unwrap().split(',').map(|v| v.parse().unwrap()).collect();
        let target = ScrollTarget { monitor_id: None, region: [r[0], r[1], r[2], r[3]], origin: (0, 0) };
        let (img, notes) = run(target, Arc::new(AtomicBool::new(false)), Arc::new(AtomicU32::new(0))).unwrap();
        println!("result {}x{} notes {notes:?}", img.width(), img.height());
        img.save(std::env::var("SNIPPY_SCROLL_OUT").unwrap()).unwrap();
    }
}
