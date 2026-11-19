use super::*;

// ───────────────────────────── Selection overlay ─────────────────────────────

pub(crate) struct Overlay {
    pub(crate) purpose: Purpose,
    pub(crate) mode: CaptureMode,
    pub(crate) monitor_id: Option<u32>,
    pub(crate) image: RgbaImage,
    pub(crate) texture: egui::TextureHandle,
    pub(crate) windows: Vec<WindowHit>,
    pub(crate) drag: Option<(Pos2, Pos2)>,
}

pub(crate) enum OverlayOutcome {
    Cancel,
    /// `px`: selection in image pixels. `global`: the same area in global egui points
    /// (used to place the red "recording" border).
    Selected { px: Rect, global: Rect },
}

// ───────────────────────────── Selection overlay ─────────────────────────────

pub(crate) fn overlay_ui(ui: &mut egui::Ui, ov: &mut Overlay) -> Option<OverlayOutcome> {
    let ctx = ui.ctx().clone();
    if ctx.input(|i| i.key_pressed(egui::Key::Escape) || i.pointer.secondary_clicked()) {
        return Some(OverlayOutcome::Cancel);
    }
    let origin = ctx.input(|i| i.viewport().outer_rect.map_or(Pos2::ZERO, |r| r.min)).to_vec2();
    let verb = if ov.purpose == Purpose::Record { "record" } else { "snip" };

    let mut outcome = None;
    egui::CentralPanel::default().frame(egui::Frame::NONE).show(ui, |ui| {
        let screen = ui.max_rect();
        let resp = ui.allocate_rect(screen, egui::Sense::click_and_drag());
        let painter = ui.painter();
        painter.image(ov.texture.id(), screen, unit_uv(), Color32::WHITE);

        // points → image pixels, and back
        let sx = ov.image.width() as f32 / screen.width();
        let sy = ov.image.height() as f32 / screen.height();
        let to_px = |p: Pos2| Pos2::new((p.x - screen.min.x) * sx, (p.y - screen.min.y) * sy);
        let to_screen =
            |r: Rect| Rect::from_min_max(screen.min + (r.min.to_vec2() / Vec2::new(sx, sy)), screen.min + (r.max.to_vec2() / Vec2::new(sx, sy)));

        let dim = Color32::from_black_alpha(130);
        let hint = |text: &str| {
            let galley_pos = Pos2::new(screen.center().x, screen.min.y + 40.0);
            let font = egui::FontId::proportional(18.0);
            let g = painter.layout_no_wrap(text.to_owned(), font, Color32::WHITE);
            let bg = Rect::from_center_size(galley_pos, g.size() + Vec2::new(28.0, 16.0));
            painter.rect_filled(bg, 8.0, Color32::from_black_alpha(190));
            painter.galley(bg.center() - g.size() / 2.0, g, Color32::WHITE);
        };

        ctx.set_cursor_icon(egui::CursorIcon::Crosshair);

        match ov.mode {
            CaptureMode::Window => {
                let hovered = resp.hover_pos().and_then(|p| {
                    let px = to_px(p);
                    ov.windows.iter().find(|w| w.rect.contains(px))
                });
                match hovered {
                    Some(w) => {
                        let sel = to_screen(w.rect).intersect(screen);
                        dim_outside(painter, screen, sel, dim);
                        painter.rect_stroke(sel, 0.0, egui::Stroke::new(3.0, ACCENT), egui::StrokeKind::Inside);
                        hint(&truncate_chars(&w.title, 60));
                        if resp.clicked() {
                            outcome = Some(OverlayOutcome::Selected { px: w.rect, global: sel.translate(origin) });
                        }
                    }
                    None => {
                        painter.rect_filled(screen, 0.0, dim);
                        hint(&format!("Click a window to {verb} it  ·  Esc to cancel"));
                    }
                }
            }
            _ => {
                if resp.drag_started() {
                    if let Some(p) = ctx.input(|i| i.pointer.press_origin()) {
                        ov.drag = Some((p, p));
                    }
                }
                if let (true, Some(p), Some((_, end))) = (resp.dragged(), resp.interact_pointer_pos(), ov.drag.as_mut()) {
                    *end = p;
                }

                match ov.drag {
                    Some((a, b)) => {
                        let sel = Rect::from_two_pos(a, b).intersect(screen);
                        dim_outside(painter, screen, sel, dim);
                        let edge = if ov.purpose == Purpose::Record { REC_RED } else { Color32::WHITE };
                        painter.rect_stroke(sel, 0.0, egui::Stroke::new(2.0, edge), egui::StrokeKind::Outside);

                        let px = Rect::from_two_pos(to_px(sel.min), to_px(sel.max));
                        let label = format!("{} × {}", px.width().round(), px.height().round());
                        let pos = Pos2::new(sel.min.x, (sel.min.y - 24.0).max(screen.min.y + 2.0));
                        let g = painter.layout_no_wrap(label, egui::FontId::monospace(13.0), Color32::WHITE);
                        painter.rect_filled(Rect::from_min_size(pos, g.size() + Vec2::new(10.0, 6.0)), 4.0, Color32::from_black_alpha(190));
                        painter.galley(pos + Vec2::new(5.0, 3.0), g, Color32::WHITE);

                        if resp.drag_stopped() {
                            if sel.width() >= 3.0 && sel.height() >= 3.0 {
                                outcome = Some(OverlayOutcome::Selected { px, global: sel.translate(origin) });
                            }
                            ov.drag = None;
                        }
                    }
                    None => {
                        painter.rect_filled(screen, 0.0, dim);
                        hint(&format!("Drag to select an area to {verb}  ·  Esc or right-click to cancel"));
                    }
                }
            }
        }
    });
    outcome
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

    #[test]
    fn crop_clamps_to_image() {
        let img = solid(50, 50);
        let c = crop_px(&img, Rect::from_min_max(Pos2::new(-10.0, 40.0), Pos2::new(100.0, 100.0))).unwrap();
        assert_eq!((c.width(), c.height()), (50, 10));
        assert!(crop_px(&img, Rect::from_min_max(Pos2::new(60.0, 0.0), Pos2::new(70.0, 10.0))).is_none());
    }
}
