//! Text and step-number annotations: font loading, layout and rasterising into images.
//!
//! The same font is registered with egui (family "annot") so the editor's live preview
//! while typing matches what gets saved.

use ab_glyph::{point, Font, FontArc, Glyph, PxScale, ScaleFont};
use eframe::egui::{self, Color32, Pos2};
use image::{Rgba, RgbaImage};
use std::sync::OnceLock;

/// egui font family name of the annotation font.
pub(crate) const FAMILY: &str = "annot";

/// Annotation font bytes: Segoe UI Bold on Windows (what Windows itself uses), else the
/// font bundled with egui.
fn font_bytes() -> &'static [u8] {
    static BYTES: OnceLock<&'static [u8]> = OnceLock::new();
    BYTES.get_or_init(|| {
        #[cfg(windows)]
        {
            let dir = std::env::var_os("WINDIR").map(std::path::PathBuf::from).unwrap_or_else(|| "C:\\Windows".into()).join("Fonts");
            for name in ["segoeuib.ttf", "seguisb.ttf", "arialbd.ttf", "segoeui.ttf"] {
                if let Ok(b) = std::fs::read(dir.join(name)) {
                    if FontArc::try_from_vec(b.clone()).is_ok() {
                        return Box::leak(b.into_boxed_slice());
                    }
                }
            }
        }
        epaint_default_fonts::UBUNTU_LIGHT
    })
}

fn font() -> &'static FontArc {
    static FONT: OnceLock<FontArc> = OnceLock::new();
    FONT.get_or_init(|| {
        FontArc::try_from_slice(font_bytes()).unwrap_or_else(|_| FontArc::try_from_slice(epaint_default_fonts::UBUNTU_LIGHT).expect("bundled font"))
    })
}

/// Registers the annotation font with egui as family `FAMILY`.
pub(crate) fn register(ctx: &egui::Context) {
    let mut defs = egui::FontDefinitions::default();
    defs.font_data.insert(FAMILY.into(), std::sync::Arc::new(egui::FontData::from_static(font_bytes())));
    defs.families.insert(egui::FontFamily::Name(FAMILY.into()), vec![FAMILY.into()]);
    ctx.set_fonts(defs);
}

/// egui font size that draws text the same size as `px` here. (egui sizes fonts by the em
/// square; here `px` is the full line height from ascent to descent.)
pub(crate) fn egui_size(px: f32) -> f32 {
    let f = font();
    let upem = f.units_per_em().unwrap_or(1000.0);
    px * upem / f.height_unscaled().max(1.0)
}

/// Glyphs of `text` at `px` pixels, laid out from (0, 0) (top-left of the first line), and
/// the overall size.
fn layout(text: &str, px: f32) -> (Vec<Glyph>, f32, f32) {
    let f = font().as_scaled(PxScale::from(px));
    let line_h = f.ascent() - f.descent() + f.line_gap();
    let mut glyphs = Vec::new();
    let mut width: f32 = 0.0;
    let mut lines = 0;
    for (i, line) in text.split('\n').enumerate() {
        lines += 1;
        let y = i as f32 * line_h + f.ascent();
        let mut x = 0.0;
        let mut prev = None;
        for c in line.chars() {
            let id = f.glyph_id(c);
            if let Some(p) = prev {
                x += f.kern(p, id);
            }
            glyphs.push(id.with_scale_and_position(px, point(x, y)));
            x += f.h_advance(id);
            prev = Some(id);
        }
        width = width.max(x);
    }
    (glyphs, width, lines as f32 * line_h - f.line_gap())
}

/// Size of `text` at `px` pixels.
pub(crate) fn measure(text: &str, px: f32) -> (f32, f32) {
    let (_, w, h) = layout(text, px);
    (w, h)
}

/// Blends `color` with coverage `c` (0..1) onto pixel (x, y), if inside the image.
fn blend(img: &mut RgbaImage, x: i32, y: i32, color: Color32, c: f32) {
    if x < 0 || y < 0 || x >= img.width() as i32 || y >= img.height() as i32 || c <= 0.0 {
        return;
    }
    let [sr, sg, sb, sa] = color.to_srgba_unmultiplied();
    let a = (sa as f32 / 255.0) * c.min(1.0);
    let d = img.get_pixel_mut(x as u32, y as u32);
    let mix = |s: u8, d: u8| (s as f32 * a + d as f32 * (1.0 - a)).round() as u8;
    *d = Rgba([mix(sr, d[0]), mix(sg, d[1]), mix(sb, d[2]), (a * 255.0 + d[3] as f32 * (1.0 - a)).round() as u8]);
}

/// Coverage mask of `text` drawn with its top-left at `origin`: (x0, y0, width, mask).
fn coverage(text: &str, px: f32, origin: Pos2, pad: i32) -> (i32, i32, usize, Vec<f32>) {
    let (glyphs, w, h) = layout(text, px);
    let (x0, y0) = (origin.x.floor() as i32 - pad, origin.y.floor() as i32 - pad);
    let (mw, mh) = ((w.ceil() as i32 + 2 * pad + 2) as usize, (h.ceil() as i32 + 2 * pad + 2) as usize);
    let mut mask = vec![0f32; mw * mh];
    let (fx, fy) = (origin.x - x0 as f32, origin.y - y0 as f32);
    for g in glyphs {
        let mut g = g;
        g.position = point(g.position.x + fx, g.position.y + fy);
        if let Some(o) = font().outline_glyph(g) {
            let b = o.px_bounds();
            o.draw(|gx, gy, c| {
                let (x, y) = (b.min.x as i32 + gx as i32, b.min.y as i32 + gy as i32);
                if x >= 0 && y >= 0 && (x as usize) < mw && (y as usize) < mh {
                    let i = y as usize * mw + x as usize;
                    mask[i] = (mask[i] + c).min(1.0);
                }
            });
        }
    }
    (x0, y0, mw, mask)
}

/// Draws `text` with its top-left at `origin`, `px` pixels tall, in `color`, with a white
/// (or black, for light colours) outline so it reads on any background.
pub(crate) fn draw_text(img: &mut RgbaImage, origin: Pos2, text: &str, px: f32, color: Color32) {
    if text.trim().is_empty() {
        return;
    }
    let halo_r = (px / 14.0).clamp(1.0, 6.0).round() as i32;
    let (x0, y0, mw, mask) = coverage(text, px, origin, halo_r + 1);
    let mh = mask.len() / mw;
    // Outline: the text mask grown by `halo_r` (round brush).
    let mut halo = vec![0f32; mask.len()];
    let offsets: Vec<(i32, i32)> = (-halo_r..=halo_r)
        .flat_map(|dy| (-halo_r..=halo_r).map(move |dx| (dx, dy)))
        .filter(|(dx, dy)| dx * dx + dy * dy <= halo_r * halo_r + halo_r)
        .collect();
    for y in 0..mh as i32 {
        for x in 0..mw as i32 {
            let c = mask[y as usize * mw + x as usize];
            if c <= 0.0 {
                continue;
            }
            for (dx, dy) in &offsets {
                let (nx, ny) = (x + dx, y + dy);
                if nx >= 0 && ny >= 0 && (nx as usize) < mw && (ny as usize) < mh {
                    let i = ny as usize * mw + nx as usize;
                    halo[i] = halo[i].max(c);
                }
            }
        }
    }
    let halo_color = outline_for(color);
    for (i, (&h, &c)) in halo.iter().zip(&mask).enumerate() {
        let (x, y) = (x0 + (i % mw) as i32, y0 + (i / mw) as i32);
        blend(img, x, y, halo_color, h * 0.9);
        blend(img, x, y, color, c);
    }
}

/// White outline for dark colours, black for light ones.
pub(crate) fn outline_for(color: Color32) -> Color32 {
    let [r, g, b, _] = color.to_srgba_unmultiplied();
    let lum = 0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32;
    if lum > 170.0 {
        Color32::from_black_alpha(220)
    } else {
        Color32::WHITE
    }
}

/// Draws a numbered step marker: a filled circle of `diameter` px in `color` with a thin
/// white ring and the number in white (or black on light colours).
pub(crate) fn draw_step(img: &mut RgbaImage, center: Pos2, n: u32, diameter: f32, color: Color32) {
    let r = diameter / 2.0;
    let ring = (diameter / 16.0).max(1.5);
    let (x0, x1) = ((center.x - r - 2.0).floor() as i32, (center.x + r + 2.0).ceil() as i32);
    let (y0, y1) = ((center.y - r - 2.0).floor() as i32, (center.y + r + 2.0).ceil() as i32);
    for y in y0..y1 {
        for x in x0..x1 {
            let d = Pos2::new(x as f32 + 0.5, y as f32 + 0.5).distance(center);
            blend(img, x, y, Color32::WHITE, (r + 0.5 - d).clamp(0.0, 1.0));
            blend(img, x, y, color, (r - ring + 0.5 - d).clamp(0.0, 1.0));
        }
    }
    let label = n.to_string();
    let px = diameter * if label.len() > 2 { 0.42 } else if label.len() == 2 { 0.5 } else { 0.6 };
    let (w, h) = measure(&label, px);
    let origin = Pos2::new(center.x - w / 2.0, center.y - h / 2.0);
    let (x0, y0, mw, mask) = coverage(&label, px, origin, 0);
    let ink = if outline_for(color) == Color32::WHITE { Color32::WHITE } else { Color32::BLACK };
    for (i, &c) in mask.iter().enumerate() {
        blend(img, x0 + (i % mw) as i32, y0 + (i / mw) as i32, ink, c);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn white(w: u32, h: u32) -> RgbaImage {
        RgbaImage::from_pixel(w, h, Rgba([255, 255, 255, 255]))
    }

    #[test]
    fn text_is_drawn_where_measured() {
        let mut img = white(400, 120);
        let (w, h) = measure("Hello", 40.0);
        assert!(w > 60.0 && h > 30.0, "{w}x{h}");
        draw_text(&mut img, Pos2::new(20.0, 30.0), "Hello", 40.0, Color32::from_rgb(220, 30, 30));
        // red ink inside the measured box, nothing far outside it
        let red = img.pixels().filter(|p| p[0] > 180 && p[1] < 90).count();
        assert!(red > 100, "red pixels: {red}");
        for (x, y, p) in img.enumerate_pixels() {
            let far = x < 10 || y < 20 || x as f32 > 20.0 + w + 10.0 || y as f32 > 30.0 + h + 10.0;
            if far {
                assert_eq!(p.0, [255, 255, 255, 255], "ink at {x},{y}");
            }
        }
    }

    #[test]
    fn multiline_text_is_taller() {
        let (_, h1) = measure("One", 30.0);
        let (w2, h2) = measure("One\nTwo lines", 30.0);
        assert!(h2 > h1 * 1.8 && w2 > 0.0);
    }

    #[test]
    fn step_marker_has_number_inside_circle() {
        let mut img = white(100, 100);
        draw_step(&mut img, Pos2::new(50.0, 50.0), 7, 60.0, Color32::from_rgb(30, 90, 220));
        let blue = img.pixels().filter(|p| p[2] > 180 && p[0] < 80).count();
        let inner_white = (35..65).flat_map(|y| (35..65).map(move |x| (x, y))).filter(|&(x, y)| img.get_pixel(x, y)[0] > 200).count();
        assert!(blue > 1500, "circle: {blue}");
        assert!(inner_white > 30, "number: {inner_white}");
        assert_eq!(img.get_pixel(5, 5).0, [255, 255, 255, 255], "outside untouched");
    }
}
