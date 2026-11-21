use super::*;
use crate::redact::RedactStyle;

// ───────────────────────────── Editor ─────────────────────────────
//
// Annotation model, the editor window, and the exporter that burns annotations into the image.

#[derive(PartialEq, Eq, Clone, Copy, Debug, Serialize, Deserialize)]
pub(crate) enum Tool {
    Pen,
    Highlighter,
    Arrow,
    Rect,
    /// Hide an area (pixelate / blur / black box).
    Redact,
    /// Typed text.
    Text,
    /// Numbered step marker (click), optionally with an arrow (drag).
    Step,
}

impl Tool {
    const ALL: [(Tool, &'static str, &'static str); 7] = [
        (Tool::Pen, "✏ Pen", "P"),
        (Tool::Highlighter, "🖍 Highlight", "H"),
        (Tool::Arrow, "➡ Arrow", "A"),
        (Tool::Rect, "⬜ Box", "R"),
        (Tool::Text, "🔤 Text", "T"),
        (Tool::Step, "① Step", "N"),
        (Tool::Redact, "▦ Redact", "X"),
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
    /// Redact only: how the area is hidden.
    pub(crate) style: RedactStyle,
    /// Text only: the text (lines separated by '\n'). `width` is the font size in px.
    pub(crate) text: String,
    /// Step only: the number. `width` is the circle's diameter in px.
    pub(crate) number: u32,
}

impl Annotation {
    /// A redaction of the area `r` (image pixels).
    pub(crate) fn redaction(r: [f32; 4], style: RedactStyle) -> Self {
        Annotation {
            tool: Tool::Redact,
            points: vec![Pos2::new(r[0], r[1]), Pos2::new(r[2], r[3])],
            color: Color32::BLACK,
            width: 1.0,
            style,
            text: String::new(),
            number: 0,
        }
    }

    /// Step with a drag: the arrow from the circle's edge to where the drag ended.
    fn step_arrow(&self) -> Option<Annotation> {
        let (c, end) = (self.points[0], *self.points.last()?);
        let d = end - c;
        let r = self.width / 2.0;
        if self.points.len() < 2 || d.length() <= r * 1.5 {
            return None;
        }
        let start = c + d.normalized() * r;
        Some(Annotation { tool: Tool::Arrow, points: vec![start, end], width: (self.width / 10.0).max(2.0), ..self.clone() })
    }

    fn rect(&self) -> [f32; 4] {
        let (a, b) = (self.points[0], *self.points.last().unwrap());
        [a.x, a.y, b.x, b.y]
    }
}

impl Annotation {
    /// The polylines that make up this annotation. Used by BOTH the on-screen preview and
    /// the exporter, so what you see is exactly what gets saved.
    pub(crate) fn polylines(&self) -> Vec<Vec<Pos2>> {
        match self.tool {
            Tool::Pen | Tool::Highlighter => vec![self.points.clone()],
            Tool::Text => vec![],
            Tool::Step => self.step_arrow().map(|a| a.polylines()).unwrap_or_default(),
            Tool::Rect | Tool::Redact => {
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
    /// While there are redactions: the rendered result (redactions can't be drawn as
    /// shapes over the image), and the edit state it was rendered for.
    pub(crate) preview: Option<(egui::TextureHandle, (usize, usize, usize))>,
    /// Text being typed (not yet an annotation).
    pub(crate) typing: Option<Annotation>,
    /// "Find personal info" running in the background.
    pub(crate) find_rx: Option<std::sync::mpsc::Receiver<Result<Vec<[f32; 4]>, String>>>,
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

    /// Annotations that can't be drawn as shapes over the image (redactions, text, steps):
    /// the editor then shows the rendered result.
    fn needs_render(&self) -> bool {
        self.annotations.iter().any(|a| matches!(a.tool, Tool::Redact | Tool::Text | Tool::Step))
    }

    /// The number the next step marker gets.
    fn next_step(&self) -> u32 {
        self.annotations.iter().filter(|a| a.tool == Tool::Step).map(|a| a.number).max().unwrap_or(0) + 1
    }

    /// Finishes the text being typed (empty text is dropped).
    pub(crate) fn commit_text(&mut self) {
        if let Some(t) = self.typing.take() {
            if !t.text.trim().is_empty() {
                self.checkpoint();
                self.annotations.push(t);
            }
        }
    }

    /// Keeps the rendered preview in step with the annotations.
    fn refresh_preview(&mut self, ctx: &egui::Context) {
        if !self.needs_render() {
            self.preview = None;
            return;
        }
        let key = (self.annotations.len(), self.undo.len(), self.redo.len());
        if self.preview.as_ref().is_some_and(|(_, k)| *k == key) {
            return;
        }
        let tex = image_to_texture(ctx, "editor-preview", &self.final_image());
        self.preview = Some((tex, key));
    }
}

impl SnippyApp {
    pub(crate) fn editor_ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        // Switching away from the text tool finishes the text being typed.
        if self.settings.tool != Tool::Text {
            if let Some(ed) = self.editor.as_mut() {
                ed.commit_text();
            }
        }
        if self.editor.as_ref().is_some_and(|e| e.typing.is_some()) {
            self.typing_keys(&ctx);
        } else {
            self.editor_shortcuts(&ctx);
        }

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
                if self.settings.tool == Tool::Redact {
                    for (style, label, tip) in RedactStyle::ALL {
                        ui.selectable_value(&mut self.settings.redact_style, style, label).on_hover_text(tip);
                    }
                } else if matches!(self.settings.tool, Tool::Text | Tool::Step) {
                    ui.color_edit_button_srgba(&mut self.settings.pen_color);
                    ui.add(egui::Slider::new(&mut self.settings.text_size, 12.0..=120.0).text("Size"));
                } else {
                    let color = if self.settings.tool == Tool::Highlighter { &mut self.settings.highlighter_color } else { &mut self.settings.pen_color };
                    ui.color_edit_button_srgba(color);
                    ui.add(egui::Slider::new(&mut self.settings.brush_size, 1.0..=24.0).text("Size"));
                }
                let finding = self.editor.as_ref().is_some_and(|e| e.find_rx.is_some());
                let find = ui
                    .add_enabled(!finding, egui::Button::new(if finding { "🔍 Looking…" } else { "🔍 Find personal info" }))
                    .on_hover_text("Hide email addresses, phone numbers and long numbers (card, account, order) automatically");
                if find.clicked() {
                    self.find_personal_info(&ctx);
                }

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

        self.poll_find(&ctx);
        let (tool, pen, hl, size, style, text_size) = (
            self.settings.tool,
            self.settings.pen_color,
            self.settings.highlighter_color,
            self.settings.brush_size,
            self.settings.redact_style,
            self.settings.text_size,
        );
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
            ed.refresh_preview(&ctx);
            let shown = ed.preview.as_ref().map_or(ed.texture.id(), |(t, _)| t.id());
            ui.painter().image(shown, rect, unit_uv(), Color32::WHITE);

            let to_px = |p: Pos2| -> Pos2 {
                let p = p.clamp(rect.min, rect.max);
                ((p - rect.min) / scale).to_pos2()
            };
            let next_step = ed.next_step();
            let new_ann = |p: Pos2| {
                let (color, width) = match tool {
                    Tool::Highlighter => (hl, size * 3.0),
                    Tool::Text => (pen, text_size),
                    Tool::Step => (pen, text_size * 1.3),
                    _ => (pen, size),
                };
                let points = match tool {
                    Tool::Pen | Tool::Highlighter | Tool::Text => vec![p],
                    Tool::Arrow | Tool::Rect | Tool::Redact | Tool::Step => vec![p, p],
                };
                Annotation { tool, points, color, width, style, text: String::new(), number: next_step }
            };

            if tool == Tool::Text {
                let at = if resp.clicked() {
                    resp.interact_pointer_pos()
                } else if resp.drag_started() {
                    ctx.input(|i| i.pointer.press_origin())
                } else {
                    None
                };
                {
                    if let Some(p) = at {
                        // Finish any text being typed, start new text where clicked (the
                        // click marks the middle of the first line's left edge).
                        ed.commit_text();
                        let p = to_px(p);
                        ed.typing = Some(new_ann(Pos2::new(p.x, p.y - text_size * 0.6)));
                    }
                }
            } else if resp.drag_started() {
                if let Some(p) = ctx.input(|i| i.pointer.press_origin()) {
                    ed.active = Some(new_ann(to_px(p)));
                }
            } else if resp.clicked() && matches!(tool, Tool::Pen | Tool::Highlighter | Tool::Step) {
                // A single click leaves a dot (pen) or a step marker.
                if let Some(p) = resp.interact_pointer_pos() {
                    let mut a = new_ann(to_px(p));
                    if tool == Tool::Step {
                        a.points.truncate(1);
                    }
                    ed.checkpoint();
                    ed.annotations.push(a);
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
                        Tool::Arrow | Tool::Rect | Tool::Redact | Tool::Step => a.points[1] = p,
                        Tool::Text => {}
                    }
                }
            }
            if resp.drag_stopped() {
                if let Some(a) = ed.active.take() {
                    let tiny = a.tool == Tool::Redact && {
                        let r = a.rect();
                        (r[2] - r[0]).abs() < 3.0 || (r[3] - r[1]).abs() < 3.0
                    };
                    if !tiny {
                        ed.checkpoint();
                        ed.annotations.push(a);
                    }
                }
            }

            let painter = ui.painter_at(rect);
            // With redactions the preview already shows every finished annotation.
            let finished: &[Annotation] = if ed.preview.is_some() { &[] } else { &ed.annotations };
            if let Some(a) = ed.active.as_ref().filter(|a| a.tool == Tool::Redact) {
                // Area being redacted: a hatched box until it's released.
                let r = a.rect();
                let r = Rect::from_two_pos(rect.min + Vec2::new(r[0], r[1]) * scale, rect.min + Vec2::new(r[2], r[3]) * scale);
                painter.rect_filled(r, 0.0, Color32::from_black_alpha(140));
                painter.rect_stroke(r, 0.0, egui::Stroke::new(1.5, Color32::WHITE), egui::StrokeKind::Inside);
            }
            let to_screen = |p: Pos2| rect.min + p.to_vec2() * scale;
            if let Some(a) = ed.active.as_ref().filter(|a| a.tool == Tool::Step) {
                // Step being dragged: its circle and number (the arrow is drawn below).
                let c = to_screen(a.points[0]);
                let r = a.width / 2.0 * scale;
                painter.circle(c, r, a.color, egui::Stroke::new((r / 8.0).max(1.0), Color32::WHITE));
                let ink = if text::outline_for(a.color) == Color32::WHITE { Color32::WHITE } else { Color32::BLACK };
                painter.text(c, egui::Align2::CENTER_CENTER, a.number.to_string(), egui::FontId::new(text::egui_size(a.width * 0.6) * scale, egui::FontFamily::Name(text::FAMILY.into())), ink);
            }
            if let Some(t) = &ed.typing {
                // Text being typed: drawn live with the same font, with a caret and a box.
                let font = egui::FontId::new(text::egui_size(t.width) * scale, egui::FontFamily::Name(text::FAMILY.into()));
                let origin = to_screen(t.points[0]);
                let shown = if t.text.is_empty() { " " } else { t.text.as_str() };
                let (w, h) = text::measure(shown, t.width);
                let outline = text::outline_for(t.color);
                let job = |c: Color32| egui::text::LayoutJob::simple(shown.to_string(), font.clone(), c, f32::INFINITY);
                let o = (t.width / 14.0).clamp(1.0, 6.0) * scale;
                for (dx, dy) in [(-o, 0.0), (o, 0.0), (0.0, -o), (0.0, o), (-o, -o), (o, o), (-o, o), (o, -o)] {
                    let g = ui.fonts_mut(|f| f.layout_job(job(outline)));
                    painter.galley(origin + Vec2::new(dx, dy), g, outline);
                }
                let g = ui.fonts_mut(|f| f.layout_job(job(t.color)));
                painter.galley(origin, g, t.color);
                let bx = Rect::from_min_size(origin, Vec2::new(w.max(t.width * 0.3), h) * scale).expand(4.0);
                painter.rect_stroke(bx, 2.0, egui::Stroke::new(1.0, ui.visuals().selection.stroke.color), egui::StrokeKind::Outside);
                // Blinking caret at the end of the last line.
                let blink = (ctx.input(|i| i.time) * 2.0) as i64 % 2 == 0;
                if blink {
                    let last = t.text.rsplit('\n').next().unwrap_or("");
                    let lines = t.text.split('\n').count().max(1) as f32;
                    let (lw, _) = text::measure(if last.is_empty() { "" } else { last }, t.width);
                    let line_h = h / lines;
                    let x = origin.x + lw * scale + 1.0;
                    let y = origin.y + (lines - 1.0) * line_h * scale;
                    painter.line_segment([Pos2::new(x, y), Pos2::new(x, y + line_h * scale)], egui::Stroke::new(1.5, t.color));
                }
                ctx.request_repaint_after(std::time::Duration::from_millis(250));
            }
            for ann in finished.iter().chain(ed.active.iter().filter(|a| a.tool != Tool::Redact)) {
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

    /// Keyboard while typing text: characters go into the text; Enter finishes
    /// (Shift+Enter: new line); Backspace deletes; Esc cancels.
    fn typing_keys(&mut self, ctx: &egui::Context) {
        let Some(ed) = self.editor.as_mut() else { return };
        let events = ctx.input(|i| i.events.clone());
        for e in events {
            let Some(t) = ed.typing.as_mut() else { break };
            match e {
                egui::Event::Text(s) => t.text.push_str(&s),
                egui::Event::Paste(s) => t.text.push_str(&s.replace("\r\n", "\n")),
                egui::Event::Key { key: egui::Key::Enter, pressed: true, modifiers, .. } => {
                    if modifiers.shift {
                        t.text.push('\n');
                    } else {
                        ed.commit_text();
                    }
                }
                egui::Event::Key { key: egui::Key::Backspace, pressed: true, .. } => {
                    t.text.pop();
                }
                egui::Event::Key { key: egui::Key::Escape, pressed: true, .. } => ed.typing = None,
                _ => {}
            }
        }
    }

    /// Starts looking for personal details to hide (in the background).
    pub(crate) fn find_personal_info(&mut self, ctx: &egui::Context) {
        let Some(ed) = self.editor.as_mut() else { return };
        // Look at the image as currently edited (so earlier redactions aren't re-found).
        let img = ed.final_image();
        let (tx, rx) = std::sync::mpsc::channel();
        let c = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(crate::ocr::recognize_words(&img).map(|lines| crate::redact::find_sensitive(&lines)));
            c.request_repaint();
        });
        ed.find_rx = Some(rx);
        self.status = "Looking for personal info…".into();
    }

    fn poll_find(&mut self, _ctx: &egui::Context) {
        let style = self.settings.redact_style;
        let Some(ed) = self.editor.as_mut() else { return };
        let Some(rx) = &ed.find_rx else { return };
        let res = match rx.try_recv() {
            Ok(r) => r,
            Err(std::sync::mpsc::TryRecvError::Empty) => return,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => Err("text recognition crashed".into()),
        };
        ed.find_rx = None;
        self.status = match res {
            Ok(rects) if rects.is_empty() => "No email addresses, phone numbers or long numbers found".into(),
            Ok(rects) => {
                ed.checkpoint();
                let n = rects.len();
                ed.annotations.extend(rects.into_iter().map(|r| Annotation::redaction(r, style)));
                diag::log(&format!("editor: hid {n} item(s) of personal info"));
                format!("Hid {n} item{} — check the result; Undo (Ctrl+Z) brings them back", if n == 1 { "" } else { "s" })
            }
            Err(e) => format!("Couldn't look for personal info: {e}"),
        };
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
    for ann in anns {
        match ann.tool {
            Tool::Redact => {
                // Applied to the image as it is at this point, so it also hides anything
                // drawn before it.
                crate::redact::apply(&mut out, ann.rect(), ann.style);
            }
            Tool::Text => text::draw_text(&mut out, ann.points[0], &ann.text, ann.width, ann.color),
            Tool::Step => {
                if let Some(arrow) = ann.step_arrow() {
                    draw_stroke(&mut out, &arrow);
                }
                text::draw_step(&mut out, ann.points[0], ann.number, ann.width, ann.color);
            }
            _ => draw_stroke(&mut out, ann),
        }
    }
    out
}

/// Rasterises one stroked annotation (pen, highlighter, arrow, box) onto `out`.
fn draw_stroke(out: &mut RgbaImage, ann: &Annotation) {
    let (w, h) = (out.width() as i32, out.height() as i32);
    {
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
            return;
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
            style: RedactStyle::Pixelate,
            text: String::new(),
            number: 0,
        };
        let out = render_annotations(&solid(100, 40), &[ann]);
        let p = out.get_pixel(30, 20);
        // 50% black over white ≈ 127, NOT ~0 as with per-step blending
        assert!((120..=135).contains(&p[0]), "got {p:?}");
    }
}
