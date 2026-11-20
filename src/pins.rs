use super::*;
use crate::icons::{self, Icon};
use egui::{Sense, Stroke, StrokeKind};

// ───────────────────────────── Pinned snips ─────────────────────────────
//
// A pinned snip is a small borderless, always-on-top window showing the image, for
// keeping a reference visible while you work. Drag to move, mouse wheel to zoom, hover
// for Copy / Close (Esc or double-click also closes).

pub(crate) struct Pin {
    id: u64,
    image: RgbaImage,
    texture: egui::TextureHandle,
    /// Image size in points at 100 % zoom (physical pixels / that monitor's scale).
    base_size: Vec2,
    zoom: f32,
    /// Initial placement in physical pixels; applied until it sticks, then the window is
    /// the user's to move.
    initial: Option<platform::PxRect>,
    first_pos: Pos2,
    closed: bool,
}

fn title(id: u64) -> String {
    format!("Snippy pin {id}")
}

impl SnippyApp {
    /// Pins `image` next to the mouse pointer (on the monitor of the last capture otherwise).
    pub(crate) fn add_pin(&mut self, ctx: &egui::Context, image: RgbaImage) {
        self.next_pin_id += 1;
        let id = self.next_pin_id;
        let probe = record::cursor_position()
            .map(|(x, y)| Pos2::new(x as f32, y as f32))
            .or(self.last_probe)
            .unwrap_or(Pos2::new(200.0, 200.0));
        let scale = Monitor::from_point(probe.x as i32, probe.y as i32)
            .ok()
            .and_then(|m| m.scale_factor().ok())
            .unwrap_or(1.0)
            .max(0.5);
        let img_px = Vec2::new(image.width() as f32, image.height() as f32);
        // Fit within ~60 % of the work area.
        let work = platform::work_area(probe.x as i32, probe.y as i32)
            .map(|[l, t, r, b]| Vec2::new((r - l) as f32, (b - t) as f32))
            .unwrap_or(Vec2::new(1920.0, 1080.0) * scale);
        let fit = ((work * 0.6).x / img_px.x).min((work * 0.6).y / img_px.y).min(1.0);
        let size_px = img_px * fit;
        let (x, y) = (probe.x + 24.0 * scale, probe.y + 24.0 * scale);
        let mut r = [x as i32, y as i32, (x + size_px.x) as i32, (y + size_px.y) as i32];
        if let Some([l, t, rr, b]) = platform::work_area(probe.x as i32, probe.y as i32) {
            // keep it on screen
            let (w, h) = (r[2] - r[0], r[3] - r[1]);
            let nx = r[0].clamp(l, (rr - w).max(l));
            let ny = r[1].clamp(t, (b - h).max(t));
            r = [nx, ny, nx + w, ny + h];
        }
        let texture = image_to_texture(ctx, &format!("pin_{id}"), &image);
        diag::log(&format!("app: pinned a {}x{} snip", image.width(), image.height()));
        self.pins.push(Pin {
            id,
            image,
            texture,
            base_size: img_px / scale,
            zoom: fit,
            initial: Some(r),
            first_pos: Pos2::new(r[0] as f32, r[1] as f32) / scale,
            closed: false,
        });
    }

    /// Draws every pin (called every frame, in every phase, so pins stay put).
    pub(crate) fn show_pins(&mut self, ctx: &egui::Context) {
        let look = self.settings.look;
        let mut copy: Option<RgbaImage> = None;
        for pin in &mut self.pins {
            let size = pin.base_size * pin.zoom;
            let builder = egui::ViewportBuilder::default()
                .with_title(title(pin.id))
                .with_position(pin.first_pos)
                .with_inner_size(size)
                .with_min_inner_size(Vec2::splat(24.0))
                .with_decorations(false)
                .with_always_on_top()
                .with_taskbar(false)
                .with_resizable(false);
            let vid = egui::ViewportId::from_hash_of(("snippy_pin", pin.id));
            ctx.show_viewport_immediate(vid, builder, |ui, _| {
                let full = ui.max_rect();
                let resp = ui.interact(full, ui.scope_id().with("pin_bg"), Sense::click_and_drag());
                let p = ui.painter();
                p.image(pin.texture.id(), full, unit_uv(), Color32::WHITE);
                p.rect_stroke(full, 0.0, Stroke::new(1.5, look.tint), StrokeKind::Inside);

                if resp.drag_started() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
                }
                if resp.double_clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) || ui.input(|i| i.viewport().close_requested()) {
                    pin.closed = true;
                }
                // Mouse wheel zooms (25 % … 400 %).
                if resp.hovered() {
                    let dy = ui.input(|i| i.smooth_scroll_delta.y);
                    if dy != 0.0 {
                        pin.zoom = (pin.zoom * (1.0 + dy * 0.0012)).clamp(0.25, 4.0);
                    }
                }
                // Hover controls: zoom % · Copy · Close
                if ui.rect_contains_pointer(full) && full.width() >= 90.0 {
                    let bar = Rect::from_min_size(Pos2::new(full.max.x - 86.0, full.min.y + 6.0), Vec2::new(80.0, 26.0));
                    p.rect_filled(bar, 13.0, Color32::from_black_alpha(170));
                    let copy_r = Rect::from_min_size(bar.min + Vec2::new(10.0, 3.0), Vec2::splat(20.0));
                    let close_r = Rect::from_min_size(bar.min + Vec2::new(50.0, 3.0), Vec2::splat(20.0));
                    let c = ui.interact(copy_r, ui.scope_id().with("pin_copy"), Sense::click());
                    let x = ui.interact(close_r, ui.scope_id().with("pin_close"), Sense::click());
                    icons::draw(p, Icon::Copy, copy_r.shrink(2.0), if c.hovered() { look.tint } else { Color32::WHITE });
                    icons::draw(p, Icon::Close, close_r.shrink(2.0), if x.hovered() { REC_RED } else { Color32::WHITE });
                    if c.clicked() {
                        copy = Some(pin.image.clone());
                    }
                    if x.clicked() {
                        pin.closed = true;
                    }
                    if (pin.zoom - 1.0).abs() > 0.01 {
                        let label = format!("{:.0}%", pin.zoom * 100.0);
                        let pos = Pos2::new(full.min.x + 6.0, full.min.y + 6.0);
                        let g = p.layout_no_wrap(label, egui::FontId::proportional(12.0), Color32::WHITE);
                        p.rect_filled(Rect::from_min_size(pos, g.size() + Vec2::new(12.0, 8.0)), 8.0, Color32::from_black_alpha(170));
                        p.galley(pos + Vec2::new(6.0, 4.0), g, Color32::WHITE);
                    }
                }
            });
            if let Some(r) = pin.initial {
                if platform::place_window(&title(pin.id), r) || !cfg!(windows) {
                    pin.initial = None;
                }
            }
        }
        self.pins.retain(|p| !p.closed);
        if let Some(img) = copy {
            let _ = self.copy_to_clipboard(&img);
        }
    }
}
