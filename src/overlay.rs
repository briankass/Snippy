use super::*;

// ───────────────────────────── Selection overlay ─────────────────────────────
//
// One borderless, always-on-top window per monitor, each showing that monitor's frozen
// screenshot, so you can pick a region / window / screen anywhere on the desktop.
// The windows are placed in physical pixels (see `platform::place_window`), so monitors
// with different display scaling line up exactly.

pub(crate) struct OverlayScreen {
    pub(crate) screen: Screen,
    texture: egui::TextureHandle,
}

pub(crate) struct Overlay {
    pub(crate) purpose: Purpose,
    pub(crate) mode: CaptureMode,
    pub(crate) screens: Vec<OverlayScreen>,
    windows: Vec<WindowHit>,
    /// Region drag in progress: (screen index, start, end) in that window's local points.
    pub(crate) drag: Option<(usize, Pos2, Pos2)>,
    cursor_screen: usize,
    frames: u32,
}

pub(crate) enum OverlayOutcome {
    Cancel,
    /// `px`: selection in the chosen screen's image pixels.
    Selected { screen: usize, px: Rect },
}

fn overlay_title(i: usize) -> String {
    format!("Snippy overlay {i}")
}

impl Overlay {
    pub(crate) fn new(ctx: &egui::Context, cap: Capture) -> Self {
        let screens = cap
            .screens
            .into_iter()
            .enumerate()
            .map(|(i, screen)| OverlayScreen { texture: image_to_texture(ctx, &format!("overlay_{i}"), &screen.image), screen })
            .collect();
        Self {
            purpose: cap.purpose,
            mode: cap.mode,
            screens,
            windows: cap.windows,
            drag: None,
            cursor_screen: cap.cursor_screen,
            frames: 0,
        }
    }

    /// Draws all overlay windows; returns the user's choice once made.
    pub(crate) fn show(&mut self, ctx: &egui::Context) -> Option<OverlayOutcome> {
        let mut outcome = None;
        for i in 0..self.screens.len() {
            let b = self.screens[i].screen.bounds();
            let scale = self.screens[i].screen.scale;
            let title = overlay_title(i);
            // Initial placement in points (approximate on mixed-DPI setups); corrected in
            // physical pixels right after.
            let builder = egui::ViewportBuilder::default()
                .with_title(&title)
                .with_position(b.min / scale)
                .with_inner_size(b.size() / scale)
                .with_decorations(false)
                .with_always_on_top()
                .with_taskbar(false)
                .with_resizable(false);
            let id = egui::ViewportId::from_hash_of(("snippy_overlay", i));
            let r = ctx.show_viewport_immediate(id, builder, |ui, _| self.screen_ui(ui, i));
            outcome = outcome.or(r);
            platform::place_window(&title, [b.min.x as i32, b.min.y as i32, b.max.x as i32, b.max.y as i32]);
            // Keyboard focus (for Esc) on the screen under the mouse.
            if i == self.cursor_screen && self.frames < 5 {
                ctx.send_viewport_cmd_to(id, egui::ViewportCommand::Focus);
            }
        }
        self.frames += 1;
        ctx.request_repaint();
        outcome
    }

    fn screen_ui(&mut self, ui: &mut egui::Ui, i: usize) -> Option<OverlayOutcome> {
        let ctx = ui.ctx().clone();
        if ui.input(|inp| inp.key_pressed(egui::Key::Escape) || inp.pointer.secondary_clicked()) {
            return Some(OverlayOutcome::Cancel);
        }
        let verb = if self.purpose == Purpose::Record { "record" } else { "snip" };
        let multi = self.screens.len() > 1;
        let mut outcome = None;

        egui::CentralPanel::default().frame(egui::Frame::NONE).show(ui, |ui| {
            let screen = ui.max_rect();
            let resp = ui.allocate_rect(screen, egui::Sense::click_and_drag());
            let painter = ui.painter();
            let s = &self.screens[i];
            painter.image(s.texture.id(), screen, unit_uv(), Color32::WHITE);

            // window points ↔ this screen's image pixels
            let (iw, ih) = (s.screen.image.width() as f32, s.screen.image.height() as f32);
            let (sx, sy) = (iw / screen.width(), ih / screen.height());
            let to_px = |p: Pos2| Pos2::new((p.x - screen.min.x) * sx, (p.y - screen.min.y) * sy);
            let to_screen = |r: Rect| {
                Rect::from_min_max(screen.min + r.min.to_vec2() / Vec2::new(sx, sy), screen.min + r.max.to_vec2() / Vec2::new(sx, sy))
            };
            let origin = s.screen.origin.to_vec2();
            let img_bounds = Rect::from_min_size(Pos2::ZERO, Vec2::new(iw, ih));

            let dim = Color32::from_black_alpha(130);
            let hint = |text: &str| {
                let pos = Pos2::new(screen.center().x, screen.min.y + 40.0);
                let g = painter.layout_no_wrap(text.to_owned(), egui::FontId::proportional(18.0), Color32::WHITE);
                let bg = Rect::from_center_size(pos, g.size() + Vec2::new(28.0, 16.0));
                painter.rect_filled(bg, 8.0, Color32::from_black_alpha(190));
                painter.galley(bg.center() - g.size() / 2.0, g, Color32::WHITE);
            };
            ctx.set_cursor_icon(egui::CursorIcon::Crosshair);

            match self.mode {
                CaptureMode::FullScreen => {
                    // Pick a screen (only shown when there's more than one).
                    if resp.hovered() {
                        painter.rect_stroke(screen.shrink(2.0), 0.0, egui::Stroke::new(5.0, ACCENT), egui::StrokeKind::Inside);
                        hint(&format!("Click to {verb} this screen  ·  Esc to cancel"));
                    } else {
                        painter.rect_filled(screen, 0.0, dim);
                    }
                    if resp.clicked() {
                        outcome = Some(OverlayOutcome::Selected { screen: i, px: img_bounds });
                    }
                }
                CaptureMode::Window => {
                    // Windows are in desktop pixels; this screen shows the part that's on it.
                    let hovered = resp.hover_pos().and_then(|p| {
                        let global = to_px(p) + origin;
                        self.windows.iter().find(|w| w.rect.contains(global))
                    });
                    let local = hovered.map(|w| (w, w.rect.translate(-origin).intersect(img_bounds)));
                    match local {
                        Some((w, r)) if r.width() >= 4.0 && r.height() >= 4.0 => {
                            let sel = to_screen(r);
                            dim_outside(painter, screen, sel, dim);
                            painter.rect_stroke(sel, 0.0, egui::Stroke::new(3.0, ACCENT), egui::StrokeKind::Inside);
                            hint(&truncate_chars(&w.title, 60));
                            if resp.clicked() {
                                outcome = Some(OverlayOutcome::Selected { screen: i, px: r });
                            }
                        }
                        _ => {
                            painter.rect_filled(screen, 0.0, dim);
                            hint(&format!("Click a window to {verb} it  ·  Esc to cancel"));
                        }
                    }
                }
                CaptureMode::Region => {
                    let mine = self.drag.is_none_or(|(s, _, _)| s == i);
                    if mine && resp.drag_started() {
                        if let Some(p) = ui.input(|inp| inp.pointer.press_origin()) {
                            self.drag = Some((i, p, p));
                        }
                    }
                    if let (true, Some(p), Some((s, _, end))) = (resp.dragged(), resp.interact_pointer_pos(), self.drag.as_mut()) {
                        if *s == i {
                            *end = p;
                        }
                    }
                    match self.drag {
                        Some((s, a, b)) if s == i => {
                            let sel = Rect::from_two_pos(a, b).intersect(screen);
                            dim_outside(painter, screen, sel, dim);
                            let edge = if self.purpose == Purpose::Record { REC_RED } else { Color32::WHITE };
                            painter.rect_stroke(sel, 0.0, egui::Stroke::new(2.0, edge), egui::StrokeKind::Outside);
                            let px = Rect::from_two_pos(to_px(sel.min), to_px(sel.max));
                            let label = format!("{} × {}", px.width().round(), px.height().round());
                            let pos = Pos2::new(sel.min.x, (sel.min.y - 24.0).max(screen.min.y + 2.0));
                            let g = painter.layout_no_wrap(label, egui::FontId::monospace(13.0), Color32::WHITE);
                            painter.rect_filled(Rect::from_min_size(pos, g.size() + Vec2::new(10.0, 6.0)), 4.0, Color32::from_black_alpha(190));
                            painter.galley(pos + Vec2::new(5.0, 3.0), g, Color32::WHITE);
                            if resp.drag_stopped() {
                                if sel.width() >= 3.0 && sel.height() >= 3.0 {
                                    outcome = Some(OverlayOutcome::Selected { screen: i, px });
                                }
                                self.drag = None;
                            }
                        }
                        Some(_) => {
                            painter.rect_filled(screen, 0.0, dim); // dragging on another screen
                        }
                        None => {
                            painter.rect_filled(screen, 0.0, dim);
                            let tail = if multi { "on any screen" } else { "" };
                            hint(&format!("Drag to select an area to {verb} {tail} ·  Esc or right-click to cancel"));
                        }
                    }
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
