use super::*;

// ───────────────────────────── Editor ─────────────────────────────
//
// Annotation model, the editor window, and the exporter that burns annotations into the image.

#[derive(PartialEq, Eq, Clone, Copy, Debug, Serialize, Deserialize)]
pub(crate) enum Tool {
    Pen,
    Highlighter,
    Arrow,
    Rect,
}

impl Tool {
    const ALL: [(Tool, &'static str, &'static str); 4] = [
        (Tool::Pen, "✏ Pen", "P"),
        (Tool::Highlighter, "🖍 Highlight", "H"),
        (Tool::Arrow, "➡ Arrow", "A"),
        (Tool::Rect, "⬜ Box", "R"),
    ];
}

/// One annotation, stored in *image pixel* coordinates so it is resolution independent.
#[derive(Clone)]
pub(crate) struct Annotation {
    pub(crate) tool: Tool,
    /// Freehand tools: the whole path. Arrow / Rect: exactly [start, end].
    pub(crate) points: Vec<Pos2>,
    pub(crate) color: Color32,
    pub(crate) width: f32,
}

impl Annotation {
    /// The polylines that make up this annotation. Used by BOTH the on-screen preview and
    /// the exporter, so what you see is exactly what gets saved.
    pub(crate) fn polylines(&self) -> Vec<Vec<Pos2>> {
        match self.tool {
            Tool::Pen | Tool::Highlighter => vec![self.points.clone()],
            Tool::Rect => {
                let (a, b) = (self.points[0], *self.points.last().unwrap());
                vec![vec![a, Pos2::new(b.x, a.y), b, Pos2::new(a.x, b.y), a]]
            }
            Tool::Arrow => {
                let (a, b) = (self.points[0], *self.points.last().unwrap());
                let d = b - a;
                let len = d.length();
                if len < 1.0 {
                    return vec![vec![a, b]];
                }
                let dir = d / len;
                let head = (self.width * 4.0).max(10.0).min(len * 0.6);
                let back = b - dir * head;
                let perp = Vec2::new(-dir.y, dir.x) * head * 0.55;
                vec![vec![a, b], vec![back + perp, b, back - perp]]
            }
        }
    }
}

pub(crate) struct Editor {
    pub(crate) image: RgbaImage,
    pub(crate) texture: egui::TextureHandle,
    pub(crate) annotations: Vec<Annotation>,
    pub(crate) active: Option<Annotation>,
    pub(crate) undo: Vec<Vec<Annotation>>,
    pub(crate) redo: Vec<Vec<Annotation>>,
}

impl Editor {
    pub(crate) fn checkpoint(&mut self) {
        self.undo.push(self.annotations.clone());
        self.redo.clear();
    }
    pub(crate) fn undo(&mut self) {
        if let Some(prev) = self.undo.pop() {
            self.redo.push(std::mem::replace(&mut self.annotations, prev));
        }
    }
    pub(crate) fn redo(&mut self) {
        if let Some(next) = self.redo.pop() {
            self.undo.push(std::mem::replace(&mut self.annotations, next));
        }
    }
    pub(crate) fn clear(&mut self) {
        if !self.annotations.is_empty() {
            self.checkpoint();
            self.annotations.clear();
        }
    }
    pub(crate) fn final_image(&self) -> RgbaImage {
        render_annotations(&self.image, &self.annotations)
    }
}

impl SnippyApp {
    pub(crate) fn editor_ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        self.editor_shortcuts(&ctx);

        egui::Panel::top("toolbar").show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                if ui.button("✂ New").on_hover_text("New snip (Ctrl+N)").clicked() {
                    self.start_snip(&ctx);
                }
                ui.separator();

                for (tool, label, key) in Tool::ALL {
                    ui.selectable_value(&mut self.settings.tool, tool, label).on_hover_text(format!("Shortcut: {key}"));
                }
                let color = if self.settings.tool == Tool::Highlighter { &mut self.settings.highlighter_color } else { &mut self.settings.pen_color };
                ui.color_edit_button_srgba(color);
                ui.add(egui::Slider::new(&mut self.settings.brush_size, 1.0..=24.0).text("Size"));

                ui.separator();
                let Some(ed) = self.editor.as_mut() else { return };
                if ui.add_enabled(!ed.undo.is_empty(), egui::Button::new("↩ Undo")).on_hover_text("Ctrl+Z").clicked() {
                    ed.undo();
                }
                if ui.add_enabled(!ed.redo.is_empty(), egui::Button::new("↪ Redo")).on_hover_text("Ctrl+Y").clicked() {
                    ed.redo();
                }
                if ui.add_enabled(!ed.annotations.is_empty(), egui::Button::new("🗑 Clear")).clicked() {
                    ed.clear();
                }
                ui.separator();
                if ui.button("📋 Copy").on_hover_text("Ctrl+C").clicked() {
                    self.copy_final();
                }
                if ui.button("💾 Save…").on_hover_text("Ctrl+S").clicked() {
                    self.save_final();
                }
                ui.separator();
                if ui.button("📌 Pin").on_hover_text("Pin to screen (stays on top)").clicked() {
                    if let Some(img) = self.editor.as_ref().map(Editor::final_image) {
                        self.add_pin(&ctx, img);
                    }
                }
                ui.separator();
                if ui.button("✕ Close").on_hover_text("Close the editor").clicked() {
                    self.editor = None;
                    self.show_bar(&ctx);
                }
            });
            ui.add_space(4.0);
        });

        egui::Panel::bottom("status").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(&self.status);
                if let Some(ed) = &self.editor {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.weak(format!("{} × {} px", ed.image.width(), ed.image.height()));
                    });
                }
            });
        });

        let (tool, pen, hl, size) = (self.settings.tool, self.settings.pen_color, self.settings.highlighter_color, self.settings.brush_size);
        egui::CentralPanel::default().show(ui, |ui| {
            let Some(ed) = self.editor.as_mut() else { return };
            let (img_w, img_h) = (ed.image.width() as f32, ed.image.height() as f32);
            let avail = ui.available_rect_before_wrap();

            // Fit to the panel, but never upscale past 1 *physical* pixel per image pixel (sharp on HiDPI).
            let ppp = ctx.pixels_per_point();
            let scale = (avail.width() / img_w).min(avail.height() / img_h).min(1.0 / ppp);
            let rect = Rect::from_center_size(avail.center(), Vec2::new(img_w, img_h) * scale);
            let resp = ui.allocate_rect(rect, egui::Sense::click_and_drag());
            let resp = resp.on_hover_cursor(egui::CursorIcon::Crosshair);

            ui.painter().rect_stroke(rect.expand(1.0), 0.0, ui.visuals().widgets.noninteractive.bg_stroke, egui::StrokeKind::Outside);
            ui.painter().image(ed.texture.id(), rect, unit_uv(), Color32::WHITE);

            let to_px = |p: Pos2| -> Pos2 {
                let p = p.clamp(rect.min, rect.max);
                ((p - rect.min) / scale).to_pos2()
            };
            let new_ann = |p: Pos2| {
                let (color, width) = match tool {
                    Tool::Highlighter => (hl, size * 3.0),
                    _ => (pen, size),
                };
                let points = match tool {
                    Tool::Pen | Tool::Highlighter => vec![p],
                    Tool::Arrow | Tool::Rect => vec![p, p],
                };
                Annotation { tool, points, color, width }
            };

            if resp.drag_started() {
                if let Some(p) = ctx.input(|i| i.pointer.press_origin()) {
                    ed.active = Some(new_ann(to_px(p)));
                }
            } else if resp.clicked() && matches!(tool, Tool::Pen | Tool::Highlighter) {
                // A single click leaves a dot.
                if let Some(p) = resp.interact_pointer_pos() {
                    ed.checkpoint();
                    ed.annotations.push(new_ann(to_px(p)));
                }
            }
            if resp.dragged() {
                if let (Some(p), Some(a)) = (resp.interact_pointer_pos(), ed.active.as_mut()) {
                    let p = to_px(p);
                    match a.tool {
                        Tool::Pen | Tool::Highlighter => {
                            if a.points.last().is_none_or(|last| last.distance(p) >= 1.0) {
                                a.points.push(p);
                            }
                        }
                        Tool::Arrow | Tool::Rect => a.points[1] = p,
                    }
                }
            }
            if resp.drag_stopped() {
                if let Some(a) = ed.active.take() {
                    ed.checkpoint();
                    ed.annotations.push(a);
                }
            }

            let painter = ui.painter_at(rect);
            for ann in ed.annotations.iter().chain(ed.active.iter()) {
                let w = ann.width * scale;
                for line in ann.polylines() {
                    let pts: Vec<Pos2> = line.iter().map(|p| rect.min + p.to_vec2() * scale).collect();
                    if pts.len() == 1 {
                        painter.circle_filled(pts[0], w * 0.5, ann.color);
                    } else {
                        painter.add(egui::Shape::line(pts, egui::Stroke::new(w, ann.color)));
                    }
                }
            }
        });
    }

    pub(crate) fn editor_shortcuts(&mut self, ctx: &egui::Context) {
        use egui::{Key, KeyboardShortcut as K, Modifiers as M};
        let cmd_shift = M::COMMAND | M::SHIFT;
        let (new, redo, undo, save, copy) = ctx.input_mut(|i| {
            (
                i.consume_shortcut(&K::new(M::COMMAND, Key::N)),
                // check the Shift variant before plain Ctrl+Z
                i.consume_shortcut(&K::new(cmd_shift, Key::Z)) || i.consume_shortcut(&K::new(M::COMMAND, Key::Y)),
                i.consume_shortcut(&K::new(M::COMMAND, Key::Z)),
                i.consume_shortcut(&K::new(M::COMMAND, Key::S)),
                // egui turns Ctrl+C into an Event::Copy rather than a key press
                i.events.iter().any(|e| matches!(e, egui::Event::Copy)),
            )
        });

        // Single-letter tool keys, only when no text field / slider has keyboard focus.
        if ctx.memory(|m| m.focused().is_none()) {
            ctx.input(|i| {
                for (tool, _, key) in Tool::ALL {
                    if let Some(k) = Key::from_name(key) {
                        if i.key_pressed(k) && i.modifiers.is_none() {
                            self.settings.tool = tool;
                        }
                    }
                }
            });
        }

        if new {
            self.start_snip(ctx);
        }
        if let Some(ed) = self.editor.as_mut() {
            if redo {
                ed.redo();
            } else if undo {
                ed.undo();
            }
        }
        if copy {
            self.copy_final();
        }
        if save {
            self.save_final();
        }
    }

    pub(crate) fn copy_final(&mut self) {
        let Some(img) = self.editor.as_ref().map(Editor::final_image) else { return };
        self.status = match self.copy_to_clipboard(&img) {
            Ok(()) => "Copied to clipboard".into(),
            Err(e) => format!("Copy failed: {e}"),
        };
    }

    pub(crate) fn save_final(&mut self) {
        let Some(img) = self.editor.as_ref().map(Editor::final_image) else { return };
        let Some(path) = rfd::FileDialog::new()
            .add_filter("PNG image", &["png"])
            .add_filter("JPEG image", &["jpg", "jpeg"])
            .set_file_name("snip.png")
            .save_file()
        else {
            return;
        };
        self.status = match save_image(&img, &path) {
            Ok(p) => format!("Saved to {}", p.display()),
            Err(e) => format!("Save failed: {e}"),
        };
    }
}

/// Rasterises annotations onto a copy of `base`.
///
/// Each annotation is first drawn into its own anti-aliased coverage mask (taking the max
/// where segments overlap), then blended onto the image ONCE. This keeps translucent
/// highlighter strokes at their real opacity instead of building up to solid colour.
pub(crate) fn render_annotations(base: &RgbaImage, anns: &[Annotation]) -> RgbaImage {
    let mut out = base.clone();
    let (w, h) = (out.width() as i32, out.height() as i32);

    for ann in anns {
        let radius = (ann.width * 0.5).max(0.5);
        let lines = ann.polylines();
        let pad = radius + 1.0;

        // Bounding box of the annotation, clipped to the image.
        let (mut min, mut max) = (Pos2::new(f32::MAX, f32::MAX), Pos2::new(f32::MIN, f32::MIN));
        for p in lines.iter().flatten() {
            min = min.min(*p);
            max = max.max(*p);
        }
        let bx0 = ((min.x - pad).floor() as i32).max(0);
        let by0 = ((min.y - pad).floor() as i32).max(0);
        let bx1 = ((max.x + pad).ceil() as i32).min(w);
        let by1 = ((max.y + pad).ceil() as i32).min(h);
        if bx1 <= bx0 || by1 <= by0 {
            continue;
        }
        let bw = (bx1 - bx0) as usize;
        let mut cov = vec![0f32; bw * (by1 - by0) as usize];

        for line in &lines {
            let segs: Vec<(Pos2, Pos2)> = if line.len() == 1 {
                vec![(line[0], line[0])]
            } else {
                line.windows(2).map(|s| (s[0], s[1])).collect()
            };
            for (a, b) in segs {
                let x0 = ((a.x.min(b.x) - pad).floor() as i32).max(bx0);
                let x1 = ((a.x.max(b.x) + pad).ceil() as i32).min(bx1);
                let y0 = ((a.y.min(b.y) - pad).floor() as i32).max(by0);
                let y1 = ((a.y.max(b.y) + pad).ceil() as i32).min(by1);
                for py in y0..y1 {
                    for px in x0..x1 {
                        let d = dist_to_segment(Pos2::new(px as f32 + 0.5, py as f32 + 0.5), a, b);
                        let c = (radius + 0.5 - d).clamp(0.0, 1.0);
                        let i = (py - by0) as usize * bw + (px - bx0) as usize;
                        if c > cov[i] {
                            cov[i] = c;
                        }
                    }
                }
            }
        }

        let [sr, sg, sb, sa] = ann.color.to_srgba_unmultiplied();
        let src_a = sa as f32 / 255.0;
        for (i, &c) in cov.iter().enumerate() {
            if c <= 0.0 {
                continue;
            }
            let (px, py) = ((i % bw) as i32 + bx0, (i / bw) as i32 + by0);
            let dst = out.get_pixel_mut(px as u32, py as u32);
            let a = src_a * c;
            let mix = |s: u8, d: u8| (s as f32 * a + d as f32 * (1.0 - a)).round() as u8;
            let out_a = (a * 255.0 + dst[3] as f32 * (1.0 - a)).round() as u8;
            *dst = Rgba([mix(sr, dst[0]), mix(sg, dst[1]), mix(sb, dst[2]), out_a]);
        }
    }
    out
}

pub(crate) fn dist_to_segment(p: Pos2, a: Pos2, b: Pos2) -> f32 {
    let ab = b - a;
    let len2 = ab.length_sq();
    let t = if len2 > 0.0 { ((p - a).dot(ab) / len2).clamp(0.0, 1.0) } else { 0.0 };
    p.distance(a + ab * t)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: u32, h: u32) -> RgbaImage {
        RgbaImage::from_pixel(w, h, Rgba([255, 255, 255, 255]))
    }

    #[test]
    fn highlighter_does_not_accumulate_alpha() {
        let ann = Annotation {
            tool: Tool::Highlighter,
            // long, dense, self-overlapping path
            points: (0..200).map(|i| Pos2::new(10.0 + (i % 50) as f32, 20.0)).collect(),
            color: Color32::from_rgba_unmultiplied(0, 0, 0, 128),
            width: 10.0,
        };
        let out = render_annotations(&solid(100, 40), &[ann]);
        let p = out.get_pixel(30, 20);
        // 50% black over white ≈ 127, NOT ~0 as with per-step blending
        assert!((120..=135).contains(&p[0]), "got {p:?}");
    }
}
