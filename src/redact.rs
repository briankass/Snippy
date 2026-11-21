//! Redaction: hide parts of a snip by pixelating, blurring or blacking them out, and find
//! personal details (email addresses, phone numbers, long numbers) to hide automatically.

use image::{Rgba, RgbaImage};
use serde::{Deserialize, Serialize};

#[derive(PartialEq, Eq, Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub(crate) enum RedactStyle {
    /// Coarse blocks of the average colour.
    #[default]
    Pixelate,
    /// A strong blur.
    Blur,
    /// Solid black: the only style that leaves nothing at all to recover.
    Black,
}

impl RedactStyle {
    pub(crate) const ALL: [(RedactStyle, &'static str, &'static str); 3] = [
        (RedactStyle::Pixelate, "Pixelate", "Coarse blocks"),
        (RedactStyle::Blur, "Blur", "Strong blur"),
        (RedactStyle::Black, "Black box", "Solid black: the safest, nothing can be recovered"),
    ];
}

/// Pixel rectangle `[x0, y0, x1, y1)`, clipped to the image. `None` if empty.
fn clip(img: &RgbaImage, r: [f32; 4]) -> Option<[u32; 4]> {
    let (w, h) = (img.width() as f32, img.height() as f32);
    let x0 = r[0].min(r[2]).floor().clamp(0.0, w) as u32;
    let x1 = r[0].max(r[2]).ceil().clamp(0.0, w) as u32;
    let y0 = r[1].min(r[3]).floor().clamp(0.0, h) as u32;
    let y1 = r[1].max(r[3]).ceil().clamp(0.0, h) as u32;
    (x1 > x0 && y1 > y0).then_some([x0, y0, x1, y1])
}

/// Applies `style` to the area `r` (image pixels, any corner order).
pub(crate) fn apply(img: &mut RgbaImage, r: [f32; 4], style: RedactStyle) {
    let Some([x0, y0, x1, y1]) = clip(img, r) else { return };
    let short = (x1 - x0).min(y1 - y0);
    match style {
        RedactStyle::Black => {
            for y in y0..y1 {
                for x in x0..x1 {
                    img.put_pixel(x, y, Rgba([0, 0, 0, 255]));
                }
            }
        }
        RedactStyle::Pixelate => {
            // Blocks big enough that text can't be read back (small blocks over known
            // fonts can be reversed): at least 12 px, or a third of the short side.
            let block = (short / 3).clamp(12, 48);
            pixelate(img, [x0, y0, x1, y1], block);
        }
        RedactStyle::Blur => {
            // Pixelate first so no fine detail survives, then blur the blocks smooth.
            let block = (short / 4).clamp(8, 32);
            pixelate(img, [x0, y0, x1, y1], block);
            let radius = (short / 3).clamp(6, 40);
            for _ in 0..3 {
                box_blur(img, [x0, y0, x1, y1], radius);
            }
        }
    }
}

fn pixelate(img: &mut RgbaImage, [x0, y0, x1, y1]: [u32; 4], block: u32) {
    let mut by = y0;
    while by < y1 {
        let ey = (by + block).min(y1);
        let mut bx = x0;
        while bx < x1 {
            let ex = (bx + block).min(x1);
            let mut sum = [0u64; 4];
            for y in by..ey {
                for x in bx..ex {
                    let p = img.get_pixel(x, y).0;
                    for c in 0..4 {
                        sum[c] += p[c] as u64;
                    }
                }
            }
            let n = ((ex - bx) * (ey - by)) as u64;
            let avg = Rgba(sum.map(|s| (s / n) as u8));
            for y in by..ey {
                for x in bx..ex {
                    img.put_pixel(x, y, avg);
                }
            }
            bx = ex;
        }
        by = ey;
    }
}

/// Separable box blur inside the rectangle (edges clamp to the rectangle, so nothing
/// from outside bleeds in and nothing inside leaks out).
fn box_blur(img: &mut RgbaImage, [x0, y0, x1, y1]: [u32; 4], radius: u32) {
    let (w, h) = ((x1 - x0) as usize, (y1 - y0) as usize);
    let r = radius as isize;
    let mut buf: Vec<[u32; 4]> = (0..w * h).map(|i| img.get_pixel(x0 + (i % w) as u32, y0 + (i / w) as u32).0.map(|v| v as u32)).collect();
    let mut tmp = buf.clone();
    let pass = |src: &[[u32; 4]], dst: &mut [[u32; 4]], len: usize, count: usize, idx: &dyn Fn(usize, usize) -> usize| {
        for line in 0..count {
            for i in 0..len {
                let mut s = [0u32; 4];
                let mut n = 0;
                for k in -r..=r {
                    let j = (i as isize + k).clamp(0, len as isize - 1) as usize;
                    let p = src[idx(line, j)];
                    for c in 0..4 {
                        s[c] += p[c];
                    }
                    n += 1;
                }
                dst[idx(line, i)] = s.map(|v| v / n);
            }
        }
    };
    pass(&buf, &mut tmp, w, h, &|line, i| line * w + i); // horizontal
    pass(&tmp, &mut buf, h, w, &|line, i| i * w + line); // vertical
    for (i, p) in buf.iter().enumerate() {
        img.put_pixel(x0 + (i % w) as u32, y0 + (i / w) as u32, Rgba(p.map(|v| v as u8)));
    }
}

/// A recognised word and its box (image pixels).
#[derive(Clone, Debug)]
pub(crate) struct Word {
    pub text: String,
    pub rect: [f32; 4],
}

/// Boxes to hide: email addresses, phone-like runs of digits (7+ digits across
/// neighbouring words, e.g. "(555) 123-4567" or "4111 1111 1111 1111"), and single
/// words with 6+ digits (account / order / ID numbers). Years and times aren't matched.
/// `lines` are the recognised lines, each a list of words in reading order.
pub(crate) fn find_sensitive(lines: &[Vec<Word>]) -> Vec<[f32; 4]> {
    let digits = |s: &str| s.chars().filter(|c| c.is_ascii_digit()).count();
    let numeric = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit() || "+-().#/ ".contains(c)) && digits(s) > 0;
    let is_email = |s: &str| {
        let s = s.trim_matches(|c: char| !c.is_alphanumeric());
        match s.split_once('@') {
            Some((user, host)) => !user.is_empty() && host.contains('.') && !host.starts_with('.') && !host.ends_with('.'),
            None => false,
        }
    };
    let mut out = Vec::new();
    for line in lines {
        let mut i = 0;
        while i < line.len() {
            let w = &line[i];
            if is_email(&w.text) || digits(&w.text) >= 6 && digits(&w.text) * 2 >= w.text.chars().count() {
                out.push(w.rect);
                i += 1;
                continue;
            }
            if numeric(&w.text) {
                // A run of numeric words: phone numbers, card numbers split into groups.
                let mut j = i;
                let mut total = 0;
                while j < line.len() && numeric(&line[j].text) {
                    total += digits(&line[j].text);
                    j += 1;
                }
                if total >= 7 {
                    out.extend(line[i..j].iter().map(|w| w.rect));
                }
                i = j.max(i + 1);
                continue;
            }
            i += 1;
        }
    }
    // A little padding so letter edges don't peek out.
    out.into_iter()
        .map(|[x0, y0, x1, y1]| {
            let pad = ((y1 - y0) * 0.15).max(2.0);
            [x0 - pad, y0 - pad, x1 + pad, y1 + pad]
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_like(w: u32, h: u32) -> RgbaImage {
        RgbaImage::from_fn(w, h, |x, y| if (x / 3 + y / 5) % 2 == 0 { Rgba([0, 0, 0, 255]) } else { Rgba([255, 255, 255, 255]) })
    }

    #[test]
    fn black_box_hides_everything_and_stays_inside() {
        let mut img = text_like(100, 60);
        let before = img.clone();
        apply(&mut img, [80.0, 50.0, 20.0, 10.0], RedactStyle::Black); // reversed corners
        for (x, y, p) in img.enumerate_pixels() {
            let inside = (20..80).contains(&x) && (10..50).contains(&y);
            if inside {
                assert_eq!(p.0, [0, 0, 0, 255]);
            } else {
                assert_eq!(p, before.get_pixel(x, y), "changed outside at {x},{y}");
            }
        }
    }

    #[test]
    fn pixelate_and_blur_remove_detail() {
        for style in [RedactStyle::Pixelate, RedactStyle::Blur] {
            let mut img = text_like(120, 40);
            apply(&mut img, [0.0, 0.0, 120.0, 40.0], style);
            // Neighbouring pixels are (nearly) equal everywhere: the stripes are gone.
            let mut max_step = 0;
            for y in 0..40 {
                for x in 1..120 {
                    let (a, b) = (img.get_pixel(x - 1, y)[0] as i32, img.get_pixel(x, y)[0] as i32);
                    max_step = max_step.max((a - b).abs());
                }
            }
            match style {
                RedactStyle::Blur => assert!(max_step <= 12, "blur left detail: step {max_step}"),
                _ => {
                    // blocks: few distinct columns of change, each block uniform
                    let p = img.get_pixel(0, 0);
                    assert!((0..12).all(|x| img.get_pixel(x, 5) == p), "first block not uniform");
                }
            }
        }
    }

    #[test]
    fn redacting_outside_the_image_is_harmless() {
        let mut img = text_like(10, 10);
        apply(&mut img, [50.0, 50.0, 90.0, 90.0], RedactStyle::Blur);
        apply(&mut img, [-20.0, -5.0, 3.0, 4.0], RedactStyle::Pixelate);
    }

    fn line(words: &[&str]) -> Vec<Word> {
        words.iter().enumerate().map(|(i, t)| Word { text: t.to_string(), rect: [i as f32 * 50.0, 0.0, i as f32 * 50.0 + 40.0, 20.0] }).collect()
    }

    #[test]
    fn finds_emails_phones_and_long_numbers_only() {
        let lines = vec![
            line(&["Contact", "brian.k@example.com", "today"]),
            line(&["Call", "(555)", "123-4567", "now"]),
            line(&["Card", "4111", "1111", "1111", "1111"]),
            line(&["Order", "#A1234567"]),
            line(&["Meeting", "at", "10:30", "in", "2026"]),
            line(&["Room", "12", "floor", "3"]),
        ];
        let hits = find_sensitive(&lines);
        // email 1 + phone 2 + card 4 + order 1 = 8; the meeting/room line has none
        assert_eq!(hits.len(), 8, "{hits:?}");
        assert!(hits.iter().all(|r| r[1] < 0.0), "padded");
    }

    /// Writes the three styles over the sample to `SNIPPY_REDACT_DEMO` (a PNG path).
    #[test]
    #[ignore]
    fn demo() {
        let base = image::load_from_memory(include_bytes!("../assets/test/redact-sample.png")).unwrap().to_rgba8();
        let mut img = base.clone();
        apply(&mut img, [105.0, 18.0, 420.0, 58.0], RedactStyle::Pixelate);
        apply(&mut img, [115.0, 83.0, 330.0, 123.0], RedactStyle::Blur);
        apply(&mut img, [180.0, 148.0, 260.0, 188.0], RedactStyle::Black);
        img.save(std::env::var("SNIPPY_REDACT_DEMO").unwrap()).unwrap();
    }
}
