use super::*;

// ───────────────────────────── Selection overlay ─────────────────────────────
//
// The main window is stretched over the whole virtual desktop (every monitor) and draws
// each monitor's frozen screenshot exactly where that monitor is. One window means one
// GPU surface, no window creation per capture (no white flashes) and one redraw per frame.
//
// With per-monitor DPI awareness Windows maps the window's pixels 1:1 onto every monitor,
// so we place everything in physical pixels and convert with the window's own scale.

pub(crate) struct OverlayScreen {
    pub(crate) screen: Screen,
    texture: egui::TextureHandle,
}

pub(crate) struct Overlay {
    pub(crate) purpose: Purpose,
    pub(crate) mode: CaptureMode,
    pub(crate) screens: Vec<OverlayScreen>,
    windows: Vec<WindowHit>,
    /// Region drag in progress: (screen index, start, end) in window points.
    pub(crate) drag: Option<(usize, Pos2, Pos2)>,
}

pub(crate) enum OverlayOutcome {
    Cancel,
    /// `px`: selection in the chosen screen's image pixels.
    Selected { screen: usize, px: Rect },
    /// Colour picked (colour-picker mode).
    Color { screen: usize, rgb: [u8; 3] },
}

impl Overlay {
    pub(crate) fn new(ctx: &egui::Context, cap: Capture) -> Self {
        let screens = cap
            .screens
            .into_iter()
            .enumerate()
            .map(|(i, screen)| OverlayScreen { texture: image_to_texture(ctx, &format!("overlay_{i}"), &screen.image), screen })
            .collect();
        Self { purpose: cap.purpose, mode: cap.mode, screens, windows: cap.windows, drag: None }
    }

    /// Bounding box of all monitors (virtual-desktop physical pixels).
    pub(crate) fn desktop(&self) -> Rect {
        self.screens.iter().map(|s| s.screen.bounds()).reduce(|a, b| a.union(b)).unwrap_or(Rect::NOTHING)
    }

    pub(crate) fn ui(&mut self, ui: &mut egui::Ui) -> Option<OverlayOutcome> {
        let ctx = ui.ctx().clone();
        if ui.input(|i| i.key_pressed(egui::Key::Escape) || i.pointer.secondary_clicked()) {
            return Some(OverlayOutcome::Cancel);
        }
        // Where the window actually is, in physical pixels (robust even if placement was
        // adjusted by the OS), and its scale.
        let (origin, ppp) = ctx.input(|i| {
            let ppp = i.viewport().native_pixels_per_point.unwrap_or(1.0);
            let rect = i.viewport().inner_rect.or(i.viewport().outer_rect);
            (rect.map_or(self.desktop().min, |r| (r.min.to_vec2() * ppp).to_pos2()), ppp)
        });
        let to_points = |r: Rect| Rect::from_min_max(((r.min - origin) / ppp).to_pos2(), ((r.max - origin) / ppp).to_pos2());
        let rects: Vec<Rect> = self.screens.iter().map(|s| to_points(s.screen.bounds())).collect();
        let verb = match self.purpose {
            Purpose::Record => "record",
            Purpose::Ocr => "copy text from",
            _ => "snip",
        };
        let multi = self.screens.len() > 1;
        let mut outcome = None;

        egui::CentralPanel::default().frame(egui::Frame::NONE).show(ui, |ui| {
            let full = ui.max_rect();
            let resp = ui.allocate_rect(full, egui::Sense::click_and_drag());
            let painter = ui.painter();
            ctx.set_cursor_icon(egui::CursorIcon::Crosshair);
            let screen_at = |p: Pos2| rects.iter().position(|r| r.contains(p));
            let hover = resp.hover_pos().and_then(screen_at);
            let dim = Color32::from_black_alpha(130);

            for (i, (s, &rect)) in self.screens.iter().zip(&rects).enumerate() {
                painter.image(s.texture.id(), rect, unit_uv(), Color32::WHITE);
                let (iw, ih) = (s.screen.image.width() as f32, s.screen.image.height() as f32);
                let (sx, sy) = (iw / rect.width(), ih / rect.height());
                let to_px = |p: Pos2| Pos2::new((p.x - rect.min.x) * sx, (p.y - rect.min.y) * sy);
                let to_screen = |r: Rect| {
                    Rect::from_min_max(rect.min + r.min.to_vec2() / Vec2::new(sx, sy), rect.min + r.max.to_vec2() / Vec2::new(sx, sy))
                };
                let img_bounds = Rect::from_min_size(Pos2::ZERO, Vec2::new(iw, ih));
                let hint = |text: &str| {
                    let pos = Pos2::new(rect.center().x, rect.min.y + 40.0);
                    let g = painter.layout_no_wrap(text.to_owned(), egui::FontId::proportional(18.0), Color32::WHITE);
                    let bg = Rect::from_center_size(pos, g.size() + Vec2::new(28.0, 16.0));
                    painter.rect_filled(bg, 8.0, Color32::from_black_alpha(190));
                    painter.galley(bg.center() - g.size() / 2.0, g, Color32::WHITE);
                };
                let hovered_here = hover == Some(i);

                if self.purpose == Purpose::ColorPick {
                    if hovered_here {
                        if let Some(pt) = resp.hover_pos() {
                            let px = to_px(pt);
                            let (cx, cy) = (px.x.floor() as i64, px.y.floor() as i64);
                            let get = |x: i64, y: i64| -> Option<[u8; 3]> {
                                (x >= 0 && y >= 0 && (x as u32) < s.screen.image.width() && (y as u32) < s.screen.image.height())
                                    .then(|| {
                                        let p = s.screen.image.get_pixel(x as u32, y as u32);
                                        [p[0], p[1], p[2]]
                                    })
                            };
                            if let Some(rgb) = get(cx, cy) {
                                draw_loupe(painter, rect, pt, rgb, &get, cx, cy);
                                if resp.clicked() {
                                    outcome = Some(OverlayOutcome::Color { screen: i, rgb });
                                }
                            }
                        }
                        hint("Click to copy a colour  ·  Esc to cancel");
                    }
                    continue;
                }

                match self.mode {
                    CaptureMode::FullScreen => {
                        if hovered_here {
                            painter.rect_stroke(rect.shrink(2.0), 0.0, egui::Stroke::new(5.0, ACCENT), egui::StrokeKind::Inside);
                            hint(&format!("Click to {verb} this screen  ·  Esc to cancel"));
                            if resp.clicked() {
                                outcome = Some(OverlayOutcome::Selected { screen: i, px: img_bounds });
                            }
                        } else {
                            painter.rect_filled(rect, 0.0, dim);
                        }
                    }
                    CaptureMode::Window => {
                        let origin_px = s.screen.origin.to_vec2();
                        let found = resp.hover_pos().filter(|_| hovered_here).and_then(|p| {
                            let global = to_px(p) + origin_px;
                            let w = self.windows.iter().find(|w| w.rect.contains(global))?;
                            let r = w.rect.translate(-origin_px).intersect(img_bounds);
                            (r.width() >= 4.0 && r.height() >= 4.0).then_some((w, r))
                        });
                        match found {
                            Some((w, r)) => {
                                let sel = to_screen(r);
                                dim_outside(painter, rect, sel, dim);
                                painter.rect_stroke(sel, 0.0, egui::Stroke::new(3.0, ACCENT), egui::StrokeKind::Inside);
                                hint(&truncate_chars(&w.title, 60));
                                if resp.clicked() {
                                    outcome = Some(OverlayOutcome::Selected { screen: i, px: r });
                                }
                            }
                            None => {
                                painter.rect_filled(rect, 0.0, dim);
                                hint(&format!("Click a window to {verb} it  ·  Esc to cancel"));
                            }
                        }
                    }
                    CaptureMode::Region => match self.drag {
                        Some((d, a, b)) if d == i => {
                            let sel = Rect::from_two_pos(a, b).intersect(rect);
                            dim_outside(painter, rect, sel, dim);
                            let edge = match self.purpose {
                                Purpose::Record => REC_RED,
                                Purpose::Ocr => ACCENT,
                                _ => Color32::WHITE,
                            };
                            painter.rect_stroke(sel, 0.0, egui::Stroke::new(2.0, edge), egui::StrokeKind::Outside);
                            let px = Rect::from_two_pos(to_px(sel.min), to_px(sel.max));
                            let label = format!("{} × {}", px.width().round(), px.height().round());
                            let pos = Pos2::new(sel.min.x, (sel.min.y - 24.0).max(rect.min.y + 2.0));
                            let g = painter.layout_no_wrap(label, egui::FontId::monospace(13.0), Color32::WHITE);
                            painter.rect_filled(Rect::from_min_size(pos, g.size() + Vec2::new(10.0, 6.0)), 4.0, Color32::from_black_alpha(190));
                            painter.galley(pos + Vec2::new(5.0, 3.0), g, Color32::WHITE);
                            if resp.drag_stopped() && sel.width() >= 3.0 && sel.height() >= 3.0 {
                                outcome = Some(OverlayOutcome::Selected { screen: i, px });
                            }
                        }
                        Some(_) => {
                            painter.rect_filled(rect, 0.0, dim); // dragging on another screen
                        }
                        None => {
                            painter.rect_filled(rect, 0.0, dim);
                            let tail = if multi { " on any screen" } else { "" };
                            hint(&format!("Drag to select an area to {verb}{tail}  ·  Esc or right-click to cancel"));
                        }
                    },
                }
            }

            // Region drag bookkeeping (a drag stays on the screen where it started).
            if self.mode == CaptureMode::Region {
                if resp.drag_started() {
                    if let Some(p) = ui.input(|i| i.pointer.press_origin()) {
                        self.drag = screen_at(p).map(|s| (s, p, p));
                    }
                }
                if let (true, Some(p), Some((_, _, end))) = (resp.dragged(), resp.interact_pointer_pos(), self.drag.as_mut()) {
                    *end = p;
                }
                if resp.drag_stopped() {
                    self.drag = None;
                }
            }
        });
        outcome
    }
}

/// Magnifier for the colour picker: an 11×11-pixel neighbourhood at 11× zoom with the
/// centre pixel outlined, plus the colour as hex and RGB.
fn draw_loupe(
    painter: &egui::Painter,
    screen: Rect,
    pointer: Pos2,
    rgb: [u8; 3],
    get: &dyn Fn(i64, i64) -> Option<[u8; 3]>,
    cx: i64,
    cy: i64,
) {
    const N: i64 = 11;
    const CELL: f32 = 11.0;
    let size = Vec2::splat(N as f32 * CELL);
    let label_h = 44.0;
    // Below-right of the pointer, flipped to stay on this screen.
    let mut min = pointer + Vec2::new(24.0, 24.0);
    if min.x + size.x > screen.max.x {
        min.x = pointer.x - 24.0 - size.x;
    }
    if min.y + size.y + label_h > screen.max.y {
        min.y = pointer.y - 24.0 - size.y - label_h;
    }
    let boxr = Rect::from_min_size(min, size);
    painter.rect_filled(boxr.expand(4.0), 8.0, Color32::from_black_alpha(220));
    for dy in 0..N {
        for dx in 0..N {
            let c = get(cx + dx - N / 2, cy + dy - N / 2).map_or(Color32::BLACK, |[r, g, b]| Color32::from_rgb(r, g, b));
            let cell = Rect::from_min_size(boxr.min + Vec2::new(dx as f32 * CELL, dy as f32 * CELL), Vec2::splat(CELL));
            painter.rect_filled(cell, 0.0, c);
        }
    }
    let centre = Rect::from_min_size(boxr.min + Vec2::splat((N / 2) as f32 * CELL), Vec2::splat(CELL));
    painter.rect_stroke(centre.expand(1.0), 0.0, egui::Stroke::new(1.0, Color32::BLACK), egui::StrokeKind::Outside);
    painter.rect_stroke(centre, 0.0, egui::Stroke::new(1.5, Color32::WHITE), egui::StrokeKind::Outside);
    let [r, g, b] = rgb;
    let label = Rect::from_min_size(Pos2::new(boxr.min.x - 4.0, boxr.max.y + 6.0), Vec2::new(size.x + 8.0, label_h - 8.0));
    painter.rect_filled(label, 8.0, Color32::from_black_alpha(220));
    let sw = Rect::from_min_size(label.min + Vec2::new(8.0, 8.0), Vec2::splat(20.0));
    painter.rect_filled(sw, 4.0, Color32::from_rgb(r, g, b));
    painter.rect_stroke(sw, 4.0, egui::Stroke::new(1.0, Color32::WHITE), egui::StrokeKind::Outside);
    painter.text(Pos2::new(sw.max.x + 8.0, label.min.y + 6.0), egui::Align2::LEFT_TOP, format!("#{r:02X}{g:02X}{b:02X}"),
        egui::FontId::monospace(13.0), Color32::WHITE);
    painter.text(Pos2::new(sw.max.x + 8.0, label.min.y + 21.0), egui::Align2::LEFT_TOP, format!("{r}, {g}, {b}"),
        egui::FontId::monospace(11.0), Color32::from_gray(190));
}

pub(crate) fn dim_outside(painter: &egui::Painter, screen: Rect, sel: Rect, dim: Color32) {
    let (l, r, t, b) = (sel.min.x, sel.max.x, sel.min.y, sel.max.y);
    for rect in [
        Rect::from_min_max(screen.min, Pos2::new(screen.max.x, t)),
        Rect::from_min_max(Pos2::new(screen.min.x, b), screen.max),
        Rect::from_min_max(Pos2::new(screen.min.x, t), Pos2::new(l, b)),
        Rect::from_min_max(Pos2::new(r, t), Pos2::new(screen.max.x, b)),
    ] {
        if rect.is_positive() {
            painter.rect_filled(rect, 0.0, dim);
        }
    }
}

pub(crate) fn crop_px(img: &RgbaImage, r: Rect) -> Option<RgbaImage> {
    let (iw, ih) = (img.width() as f32, img.height() as f32);
    let x0 = r.min.x.round().clamp(0.0, iw) as u32;
    let y0 = r.min.y.round().clamp(0.0, ih) as u32;
    let x1 = r.max.x.round().clamp(0.0, iw) as u32;
    let y1 = r.max.y.round().clamp(0.0, ih) as u32;
    (x1 > x0 && y1 > y0).then(|| imageops::crop_imm(img, x0, y0, x1 - x0, y1 - y0).to_image())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: u32, h: u32) -> RgbaImage {
        RgbaImage::from_pixel(w, h, Rgba([255, 255, 255, 255]))
    }

    #[test]
    fn crop_clamps_to_image() {
        let img = solid(50, 50);
        let c = crop_px(&img, Rect::from_min_max(Pos2::new(-10.0, 40.0), Pos2::new(100.0, 100.0))).unwrap();
        assert_eq!((c.width(), c.height()), (50, 10));
        assert!(crop_px(&img, Rect::from_min_max(Pos2::new(60.0, 0.0), Pos2::new(70.0, 10.0))).is_none());
    }
}
