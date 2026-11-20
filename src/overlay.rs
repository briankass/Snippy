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
        let verb = if self.purpose == Purpose::Record { "record" } else { "snip" };
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
                            let edge = if self.purpose == Purpose::Record { REC_RED } else { Color32::WHITE };
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
