#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Snippy — a tiny floating screen-snipping tool.
//!
//! Snip:   Bar ──▶ Capturing (hidden) ──▶ Overlay (pick region / window) ──▶ Editor
//!                                     └──▶ Editor (full-screen mode skips the overlay)
//! Record: Bar ──▶ Capturing (hidden) ──▶ Overlay ──▶ Recording (mini bar) ──▶ RecordingDone

mod audio;
#[cfg(windows)]
mod mf;
mod record;

use arboard::{Clipboard, ImageData};
use eframe::egui::{self, Color32, Pos2, Rect, Vec2};
use image::{imageops, DynamicImage, Rgba, RgbaImage};
use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use record::{RecordTarget, Recorder, RecordingInfo};
use std::time::Duration;
use xcap::{Monitor, Window};

const BAR_SIZE: Vec2 = Vec2::new(62.0, 358.0);
const REC_BAR_SIZE: Vec2 = Vec2::new(62.0, 196.0);
const ACCENT: Color32 = Color32::from_rgb(76, 194, 255);
const REC_RED: Color32 = Color32::from_rgb(232, 58, 58);
const RECORD_FPS: u32 = 30;

/// Are we taking a screenshot or a video?
#[derive(PartialEq, Eq, Clone, Copy, Debug)]
enum Purpose {
    Snip,
    Record,
}

// ───────────────────────────── Model ─────────────────────────────

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
enum CaptureMode {
    Region,
    Window,
    FullScreen,
}

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
enum Tool {
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
struct Annotation {
    tool: Tool,
    /// Freehand tools: the whole path. Arrow / Rect: exactly [start, end].
    points: Vec<Pos2>,
    color: Color32,
    width: f32,
}

impl Annotation {
    /// The polylines that make up this annotation. Used by BOTH the on-screen preview and
    /// the exporter, so what you see is exactly what gets saved.
    fn polylines(&self) -> Vec<Vec<Pos2>> {
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

/// A window visible on the captured monitor, in image-pixel coordinates.
struct WindowHit {
    rect: Rect,
    title: String,
}

struct Capture {
    purpose: Purpose,
    mode: CaptureMode,
    monitor_id: Option<u32>,
    image: RgbaImage,
    /// Top-most first. Only filled in Window mode.
    windows: Vec<WindowHit>,
}

struct Overlay {
    purpose: Purpose,
    mode: CaptureMode,
    monitor_id: Option<u32>,
    image: RgbaImage,
    texture: egui::TextureHandle,
    windows: Vec<WindowHit>,
    drag: Option<(Pos2, Pos2)>,
}

enum OverlayOutcome {
    Cancel,
    /// `px`: selection in image pixels. `global`: the same area in global egui points
    /// (used to place the red "recording" border).
    Selected { px: Rect, global: Rect },
}

struct Editor {
    image: RgbaImage,
    texture: egui::TextureHandle,
    annotations: Vec<Annotation>,
    active: Option<Annotation>,
    undo: Vec<Vec<Annotation>>,
    redo: Vec<Vec<Annotation>>,
}

impl Editor {
    fn checkpoint(&mut self) {
        self.undo.push(self.annotations.clone());
        self.redo.clear();
    }
    fn undo(&mut self) {
        if let Some(prev) = self.undo.pop() {
            self.redo.push(std::mem::replace(&mut self.annotations, prev));
        }
    }
    fn redo(&mut self) {
        if let Some(next) = self.redo.pop() {
            self.undo.push(std::mem::replace(&mut self.annotations, next));
        }
    }
    fn clear(&mut self) {
        if !self.annotations.is_empty() {
            self.checkpoint();
            self.annotations.clear();
        }
    }
    fn final_image(&self) -> RgbaImage {
        render_annotations(&self.image, &self.annotations)
    }
}

enum Phase {
    Bar,
    Capturing,
    Overlay(Overlay),
    Editor,
    Recording { recorder: Recorder, border: Option<Rect> },
    RecordingDone(RecordingInfo),
}

struct SnippyApp {
    phase: Phase,
    purpose: Purpose,
    mode: CaptureMode,
    delay_secs: u64,
    status: String,
    capture_rx: Option<Receiver<Result<Capture, String>>>,
    editor: Option<Editor>,
    clipboard: Option<Clipboard>,

    tool: Tool,
    pen_color: Color32,
    highlighter_color: Color32,
    brush_size: f32,
}

impl Default for SnippyApp {
    fn default() -> Self {
        Self {
            phase: Phase::Bar,
            purpose: Purpose::Snip,
            mode: CaptureMode::Region,
            delay_secs: 0,
            status: "Ready".into(),
            capture_rx: None,
            editor: None,
            clipboard: None,
            tool: Tool::Pen,
            pen_color: Color32::from_rgb(230, 40, 40),
            highlighter_color: Color32::from_rgba_unmultiplied(255, 235, 50, 110),
            brush_size: 4.0,
        }
    }
}

// ───────────────────────────── Window / phase transitions ─────────────────────────────
//
// All viewport commands are issued from these few functions instead of a "synced" flag
// that every code path has to remember to reset.

impl SnippyApp {
    fn show_bar(&mut self, ctx: &egui::Context) {
        self.phase = Phase::Bar;
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Decorations(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(egui::WindowLevel::AlwaysOnTop));
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(BAR_SIZE));
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
    }

    fn show_overlay(&mut self, ctx: &egui::Context, cap: Capture) {
        let texture = image_to_texture(ctx, "overlay", &cap.image);
        self.phase = Phase::Overlay(Overlay {
            purpose: cap.purpose,
            mode: cap.mode,
            monitor_id: cap.monitor_id,
            image: cap.image,
            texture,
            windows: cap.windows,
            drag: None,
        });
        // Still hidden here, so the user never sees the bar stretched to full screen.
        ctx.send_viewport_cmd(egui::ViewportCommand::Decorations(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(egui::WindowLevel::AlwaysOnTop));
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    }

    fn show_editor(&mut self, ctx: &egui::Context, img: RgbaImage) {
        // Size the editor to the snip (at native pixel size), within sensible bounds.
        let (ppp, monitor) = ctx.input(|i| {
            (
                i.viewport().native_pixels_per_point.unwrap_or(1.0),
                i.viewport().monitor_size.unwrap_or(Vec2::new(1920.0, 1080.0)),
            )
        });
        let want = Vec2::new(img.width() as f32, img.height() as f32) / ppp + Vec2::new(32.0, 110.0);
        let size = want
            .max(Vec2::new(760.0, 420.0))
            .min(monitor * 0.85);

        let copied = self.copy_to_clipboard(&img);
        self.status = match copied {
            Ok(()) => format!("Captured {}×{} — copied to clipboard", img.width(), img.height()),
            Err(e) => format!("Captured, but copy failed: {e}"),
        };

        let texture = image_to_texture(ctx, "editor", &img);
        self.editor = Some(Editor {
            image: img,
            texture,
            annotations: Vec::new(),
            active: None,
            undo: Vec::new(),
            redo: Vec::new(),
        });
        self.phase = Phase::Editor;

        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Decorations(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(egui::WindowLevel::Normal));
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    }

    fn start_snip(&mut self, ctx: &egui::Context) {
        self.purpose = Purpose::Snip;
        self.start_capture(ctx);
    }

    fn start_capture(&mut self, ctx: &egui::Context) {
        if self.capture_rx.is_some() {
            return; // already capturing
        }

        // Capture the monitor Snippy is currently on (not always the primary one).
        let probe = ctx.input(|i| {
            let ppp = i.viewport().native_pixels_per_point.unwrap_or(1.0);
            i.viewport().outer_rect.map(|r| r.center() * ppp)
        });

        let mode = self.mode;
        let purpose = self.purpose;
        // For snips the delay happens before the screenshot; for recordings it's a
        // countdown after you've picked the area.
        let delay = if purpose == Purpose::Snip { self.delay_secs } else { 0 };
        let ctx = ctx.clone();
        let (tx, rx) = mpsc::channel();
        self.capture_rx = Some(rx);
        self.phase = Phase::Capturing;
        self.status = "Capturing…".into();
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));

        std::thread::spawn(move || {
            // Let the OS finish hiding us (and any fade animation), plus the user's delay.
            std::thread::sleep(Duration::from_millis(300) + Duration::from_secs(delay));
            let _ = tx.send(capture(purpose, mode, probe));
            // eframe calls `App::logic` while hidden when a repaint is requested.
            ctx.request_repaint();
        });
    }
}

// ───────────────────────────── eframe integration ─────────────────────────────

impl eframe::App for SnippyApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        egui::Rgba::TRANSPARENT.to_array()
    }

    /// Runs even while the window is hidden, so capture results are handled here
    /// on the UI thread — the worker thread never touches the window.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let Some(rx) = &self.capture_rx else { return };
        let result = match rx.try_recv() {
            Ok(r) => r,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => Err("capture thread crashed".into()),
        };
        self.capture_rx = None;
        match result {
            Ok(cap) if cap.mode == CaptureMode::FullScreen && cap.purpose == Purpose::Snip => {
                self.show_editor(ctx, cap.image)
            }
            Ok(cap) if cap.mode == CaptureMode::FullScreen => {
                let target = RecordTarget { monitor_id: cap.monitor_id, region: None };
                self.begin_recording(ctx, target, None);
            }
            Ok(cap) => self.show_overlay(ctx, cap),
            Err(e) => {
                self.status = format!("Capture failed: {e}");
                if self.editor.is_some() {
                    self.phase = Phase::Editor;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                } else {
                    self.show_bar(ctx);
                }
            }
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        match &mut self.phase {
            Phase::Overlay(overlay) => match overlay_ui(ui, overlay) {
                Some(OverlayOutcome::Selected { px, global }) => match overlay.purpose {
                    Purpose::Snip => match crop_px(&overlay.image, px) {
                        Some(img) => self.show_editor(&ctx, img),
                        None => overlay.drag = None,
                    },
                    Purpose::Record => {
                        let r = [px.min.x, px.min.y, px.width(), px.height()].map(|v| v.round().max(0.0) as u32);
                        let target = RecordTarget { monitor_id: overlay.monitor_id, region: Some(r) };
                        self.begin_recording(&ctx, target, Some(global));
                    }
                },
                Some(OverlayOutcome::Cancel) => {
                    self.status = "Cancelled".into();
                    if self.editor.is_some() {
                        // Came from the editor's "New" button: go back to it.
                        self.phase = Phase::Editor;
                        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
                        ctx.send_viewport_cmd(egui::ViewportCommand::Decorations(true));
                        ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(
                            egui::WindowLevel::Normal,
                        ));
                    } else {
                        self.show_bar(&ctx);
                    }
                }
                None => {}
            },
            Phase::Bar | Phase::Capturing => self.bar_ui(ui),
            Phase::Editor => self.editor_ui(ui),
            Phase::Recording { .. } => self.recording_ui(ui),
            Phase::RecordingDone(_) => self.recording_done_ui(ui),
        }
    }
}

// ───────────────────────────── Floating bar ─────────────────────────────

impl SnippyApp {
    fn bar_ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let capturing = matches!(self.phase, Phase::Capturing);

        let frame = egui::Frame::window(ui.style())
            .inner_margin(egui::Margin::same(6))
            .corner_radius(egui::CornerRadius::same(10));

        egui::CentralPanel::default().frame(frame).show(ui, |ui| {
            ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.y = 4.0;
                let btn = Vec2::new(46.0, 40.0);

                // Drag handle
                let (rect, res) =
                    ui.allocate_exact_size(Vec2::new(ui.available_width(), 18.0), egui::Sense::drag());
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    "⋮⋮",
                    egui::FontId::proportional(14.0),
                    ui.visuals().weak_text_color(),
                );
                if res.drag_started() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                }
                res.on_hover_cursor(egui::CursorIcon::Grab);

                // Main action: new snip / new recording
                let (icon, fg, bg, tip) = match self.purpose {
                    Purpose::Snip => ("✂", Color32::BLACK, ACCENT, "New snip"),
                    Purpose::Record => ("⏺", Color32::WHITE, REC_RED, "New recording"),
                };
                let main = egui::Button::new(egui::RichText::new(icon).size(22.0).color(fg))
                    .fill(bg)
                    .corner_radius(egui::CornerRadius::same(8));
                if ui
                    .add_enabled_ui(!capturing, |ui| ui.add_sized(Vec2::new(46.0, 44.0), main))
                    .inner
                    .on_hover_text(tip)
                    .clicked()
                {
                    self.start_capture(&ctx);
                }

                // Snip / Record toggle
                ui.allocate_ui_with_layout(
                    Vec2::new(46.0, 26.0),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        ui.spacing_mut().item_spacing.x = 2.0;
                        for (p, icon, tip) in [(Purpose::Snip, "📷", "Screenshot"), (Purpose::Record, "🎥", "Screen recording (MP4)")] {
                            let b = egui::Button::selectable(self.purpose == p, egui::RichText::new(icon).size(13.0))
                                .corner_radius(egui::CornerRadius::same(6));
                            if ui.add_sized(Vec2::new(22.0, 26.0), b).on_hover_text(tip).clicked() {
                                self.purpose = p;
                            }
                        }
                    },
                );

                ui.separator();

                // Mode: three toggle buttons instead of menus (menus get clipped in a 62px window)
                for (mode, icon, tip) in [
                    (CaptureMode::Region, "⬚", "Region — drag to select"),
                    (CaptureMode::Window, "🗔", "Window — click a window"),
                    (CaptureMode::FullScreen, "🖵", "Full screen"),
                ] {
                    let selected = self.mode == mode;
                    let b = egui::Button::selectable(selected, egui::RichText::new(icon).size(18.0))
                        .corner_radius(egui::CornerRadius::same(8));
                    if ui.add_sized(btn, b).on_hover_text(tip).clicked() {
                        self.mode = mode;
                    }
                }

                ui.separator();

                // Delay: click cycles 0 → 3 → 5 → 10, right-click resets
                let label = if self.delay_secs == 0 { "⏱".to_owned() } else { format!("{}s", self.delay_secs) };
                let b = egui::Button::new(egui::RichText::new(label).size(17.0))
                    .corner_radius(egui::CornerRadius::same(8));
                let r = ui
                    .add_sized(btn, b)
                    .on_hover_text(format!(
                        "{}: {}s\nClick to change, right-click to reset",
                        if self.purpose == Purpose::Snip { "Delay" } else { "Countdown" },
                        self.delay_secs
                    ));
                if r.clicked() {
                    self.delay_secs = match self.delay_secs {
                        0 => 3,
                        3 => 5,
                        5 => 10,
                        _ => 0,
                    };
                }
                if r.secondary_clicked() {
                    self.delay_secs = 0;
                }

                ui.separator();

                let exit = egui::Button::new(egui::RichText::new("✕").size(15.0))
                    .corner_radius(egui::CornerRadius::same(8));
                if ui.add_sized(Vec2::new(46.0, 30.0), exit).on_hover_text("Exit Snippy").clicked() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
        });
    }
}

// ───────────────────────────── Selection overlay ─────────────────────────────

fn overlay_ui(ui: &mut egui::Ui, ov: &mut Overlay) -> Option<OverlayOutcome> {
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

fn dim_outside(painter: &egui::Painter, screen: Rect, sel: Rect, dim: Color32) {
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

// ───────────────────────────── Editor ─────────────────────────────

impl SnippyApp {
    fn editor_ui(&mut self, ui: &mut egui::Ui) {
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
                    ui.selectable_value(&mut self.tool, tool, label).on_hover_text(format!("Shortcut: {key}"));
                }
                let color = if self.tool == Tool::Highlighter { &mut self.highlighter_color } else { &mut self.pen_color };
                ui.color_edit_button_srgba(color);
                ui.add(egui::Slider::new(&mut self.brush_size, 1.0..=24.0).text("Size"));

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
                if ui.button("⬅ Bar").on_hover_text("Close editor, back to the floating bar").clicked() {
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

        let (tool, pen, hl, size) = (self.tool, self.pen_color, self.highlighter_color, self.brush_size);
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

    fn editor_shortcuts(&mut self, ctx: &egui::Context) {
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
                            self.tool = tool;
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

    fn copy_final(&mut self) {
        let Some(img) = self.editor.as_ref().map(Editor::final_image) else { return };
        self.status = match self.copy_to_clipboard(&img) {
            Ok(()) => "Copied to clipboard".into(),
            Err(e) => format!("Copy failed: {e}"),
        };
    }

    fn save_final(&mut self) {
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

    fn copy_to_clipboard(&mut self, img: &RgbaImage) -> Result<(), String> {
        // Keep one clipboard handle alive for the app's lifetime (required on Linux/X11,
        // where the data disappears when the owner is dropped; harmless on Windows).
        if self.clipboard.is_none() {
            self.clipboard = Some(Clipboard::new().map_err(|e| e.to_string())?);
        }
        let data = ImageData {
            width: img.width() as usize,
            height: img.height() as usize,
            bytes: Cow::Borrowed(img.as_raw()),
        };
        self.clipboard.as_mut().unwrap().set_image(data).map_err(|e| e.to_string())
    }
}

// ───────────────────────────── Recording ─────────────────────────────

impl SnippyApp {
    /// `border`: the recorded area in global points, to outline it on screen (None = full screen).
    fn begin_recording(&mut self, ctx: &egui::Context, target: RecordTarget, border: Option<Rect>) {
        let countdown = Duration::from_secs(self.delay_secs);
        let opts = record::RecordOptions { fps: RECORD_FPS, countdown, system_audio: false, microphone: false };
        let recorder = Recorder::start(target, opts, record::default_recording_path());
        self.status = "Recording…".into();
        self.phase = Phase::Recording { recorder, border };

        // The mini bar stays visible while recording. On Windows it is excluded from capture
        // (see `exclude_from_capture`), so it never shows up in the video.
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Decorations(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(egui::WindowLevel::AlwaysOnTop));
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(REC_BAR_SIZE));
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
    }

    fn stop_recording(&mut self, ctx: &egui::Context) {
        let Phase::Recording { recorder, .. } = std::mem::replace(&mut self.phase, Phase::Bar) else { return };
        match recorder.stop() {
            Ok(info) => {
                self.status = "Recording saved".into();
                self.phase = Phase::RecordingDone(info);
                ctx.send_viewport_cmd(egui::ViewportCommand::Decorations(true));
                ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(egui::WindowLevel::Normal));
                ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(Vec2::new(560.0, 230.0)));
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            }
            Err(e) => {
                self.status = format!("Recording failed: {e}");
                self.show_bar(ctx);
            }
        }
    }

    fn recording_ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        ctx.request_repaint_after(Duration::from_millis(200)); // keep the timer ticking

        let Phase::Recording { recorder, border } = &self.phase else { return };
        if recorder.has_failed() {
            self.stop_recording(&ctx);
            return;
        }
        let (countdown, elapsed, paused, border) =
            (recorder.countdown_remaining(), recorder.elapsed(), recorder.is_paused(), *border);

        if let Some(area) = border {
            show_recording_border(&ctx, area, paused);
        }

        let mut stop = false;
        let mut toggle_pause = false;
        let frame = egui::Frame::window(ui.style())
            .inner_margin(egui::Margin::same(6))
            .corner_radius(egui::CornerRadius::same(10));
        egui::CentralPanel::default().frame(frame).show(ui, |ui| {
            ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.y = 4.0;
                let (rect, res) =
                    ui.allocate_exact_size(Vec2::new(ui.available_width(), 18.0), egui::Sense::drag());
                ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, "⋮⋮",
                    egui::FontId::proportional(14.0), ui.visuals().weak_text_color());
                if res.drag_started() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                }

                // Status: countdown, or blinking dot + elapsed time
                let (rect, _) = ui.allocate_exact_size(Vec2::new(46.0, 40.0), egui::Sense::hover());
                let p = ui.painter();
                match countdown {
                    Some(n) => {
                        p.text(rect.center(), egui::Align2::CENTER_CENTER, n.to_string(),
                            egui::FontId::proportional(26.0), REC_RED);
                    }
                    None => {
                        let blink = paused || (ctx.input(|i| i.time) * 2.0) as i64 % 2 == 0;
                        let dot = if paused { ui.visuals().weak_text_color() } else { REC_RED };
                        if blink {
                            p.circle_filled(rect.center_top() + Vec2::new(0.0, 9.0), 6.0, dot);
                        }
                        let secs = elapsed.as_secs();
                        p.text(rect.center_bottom() - Vec2::new(0.0, 9.0), egui::Align2::CENTER_CENTER,
                            format!("{}:{:02}", secs / 60, secs % 60),
                            egui::FontId::monospace(13.0), ui.visuals().strong_text_color());
                    }
                }

                ui.separator();
                let btn = Vec2::new(46.0, 40.0);
                let (icon, tip) = if paused { ("▶", "Resume") } else { ("⏸", "Pause") };
                let pause = egui::Button::new(egui::RichText::new(icon).size(18.0))
                    .corner_radius(egui::CornerRadius::same(8));
                if ui.add_enabled_ui(countdown.is_none(), |ui| ui.add_sized(btn, pause)).inner.on_hover_text(tip).clicked() {
                    toggle_pause = true;
                }
                let stop_btn = egui::Button::new(egui::RichText::new("⏹").size(20.0).color(Color32::WHITE))
                    .fill(REC_RED)
                    .corner_radius(egui::CornerRadius::same(8));
                if ui.add_sized(btn, stop_btn).on_hover_text("Stop and save").clicked() {
                    stop = true;
                }
            });
        });

        if toggle_pause {
            if let Phase::Recording { recorder, .. } = &self.phase {
                recorder.set_paused(!paused);
            }
        }
        if stop {
            self.stop_recording(&ctx);
        }
    }

    fn recording_done_ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let Phase::RecordingDone(info) = &mut self.phase else { return };
        let mut back_to_bar = false;
        let mut new_recording = false;

        egui::Panel::bottom("status").show(ui, |ui| ui.label(&self.status));
        egui::CentralPanel::default().show(ui, |ui| {
            ui.add_space(6.0);
            ui.heading("🎥 Recording saved");
            ui.add_space(4.0);
            ui.add(egui::Label::new(egui::RichText::new(info.path.display().to_string()).monospace()).wrap());
            let secs = info.duration.as_secs();
            let size = if info.bytes >= 1_048_576 {
                format!("{:.1} MB", info.bytes as f64 / 1_048_576.0)
            } else {
                format!("{} KB", info.bytes.div_ceil(1024))
            };
            let mut details = format!(
                "{} × {}  ·  {}:{:02}  ·  {size}  ·  {} frames",
                info.width, info.height, secs / 60, secs % 60, info.frames
            );
            if info.dropped > 0 {
                details += &format!(" ({} dropped)", info.dropped);
            }
            ui.weak(details);
            ui.add_space(10.0);

            ui.horizontal_wrapped(|ui| {
                if ui.button("▶ Play").clicked() {
                    if let Err(e) = open_path(&info.path) {
                        self.status = format!("Couldn't open: {e}");
                    }
                }
                if ui.button("📂 Show in folder").clicked() {
                    if let Err(e) = reveal_in_folder(&info.path) {
                        self.status = format!("Couldn't open folder: {e}");
                    }
                }
                if ui.button("💾 Save As…").clicked() {
                    let name = info.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    if let Some(dest) = rfd::FileDialog::new().add_filter("MP4 video", &["mp4"]).set_file_name(name).save_file() {
                        let dest = if dest.extension().is_none() { dest.with_extension("mp4") } else { dest };
                        match move_file(&info.path, &dest) {
                            Ok(()) => {
                                self.status = format!("Moved to {}", dest.display());
                                info.path = dest;
                            }
                            Err(e) => self.status = format!("Save failed: {e}"),
                        }
                    }
                }
                if ui.button("🗑 Delete").clicked() {
                    self.status = match std::fs::remove_file(&info.path) {
                        Ok(()) => "Recording deleted".into(),
                        Err(e) => format!("Delete failed: {e}"),
                    };
                    back_to_bar = true;
                }
            });
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("⏺ New recording").clicked() {
                    new_recording = true;
                }
                if ui.button("⬅ Bar").clicked() {
                    back_to_bar = true;
                }
            });
        });

        if new_recording {
            self.purpose = Purpose::Record;
            self.start_capture(&ctx);
        } else if back_to_bar {
            self.show_bar(&ctx);
        }
    }
}

/// Outlines the recorded area with four thin, solid, always-on-top strips placed just
/// *outside* it. They never overlap the area, so they can't end up in the video, and they
/// don't rely on window transparency (which isn't available everywhere).
fn show_recording_border(ctx: &egui::Context, area: Rect, paused: bool) {
    const GAP: f32 = 1.0;
    const T: f32 = 3.0;
    let o = area.expand(GAP);
    let strips = [
        Rect::from_min_max(Pos2::new(o.min.x - T, o.min.y - T), Pos2::new(o.max.x + T, o.min.y)), // top
        Rect::from_min_max(Pos2::new(o.min.x - T, o.max.y), Pos2::new(o.max.x + T, o.max.y + T)), // bottom
        Rect::from_min_max(Pos2::new(o.min.x - T, o.min.y), Pos2::new(o.min.x, o.max.y)),         // left
        Rect::from_min_max(Pos2::new(o.max.x, o.min.y), Pos2::new(o.max.x + T, o.max.y)),         // right
    ];
    let color = if paused { Color32::from_rgb(240, 180, 40) } else { REC_RED };
    for (i, r) in strips.into_iter().enumerate() {
        let builder = egui::ViewportBuilder::default()
            .with_title("Snippy recording area")
            .with_position(r.min)
            .with_inner_size(r.size())
            .with_min_inner_size(Vec2::splat(1.0))
            .with_decorations(false)
            .with_always_on_top()
            .with_mouse_passthrough(true)
            .with_taskbar(false)
            .with_resizable(false);
        ctx.show_viewport_immediate(egui::ViewportId::from_hash_of(("snippy_rec_border", i)), builder, |ui, _| {
            let rect = ui.max_rect();
            ui.painter().rect_filled(rect, 0.0, color);
        });
    }
}

fn open_path(path: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    let mut cmd = std::process::Command::new("explorer");
    #[cfg(target_os = "macos")]
    let mut cmd = std::process::Command::new("open");
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = std::process::Command::new("xdg-open");
    cmd.arg(path).spawn().map(|_| ())
}

fn reveal_in_folder(path: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // explorer needs `/select,"path"` unsplit, so pass it raw.
        std::process::Command::new("explorer")
            .raw_arg(format!("/select,\"{}\"", path.display()))
            .spawn()
            .map(|_| ())
    }
    #[cfg(not(windows))]
    {
        open_path(path.parent().unwrap_or(path))
    }
}

fn move_file(from: &Path, to: &Path) -> std::io::Result<()> {
    if from == to {
        return Ok(());
    }
    // rename fails across drives; fall back to copy + delete.
    std::fs::rename(from, to).or_else(|_| {
        std::fs::copy(from, to)?;
        std::fs::remove_file(from)
    })
}

/// Hide Snippy's own windows from screen capture (Windows 10 2004+), so the floating bar
/// doesn't appear in recordings.
#[cfg(windows)]
fn exclude_from_capture(cc: &eframe::CreationContext<'_>) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows_sys::Win32::UI::WindowsAndMessaging::{SetWindowDisplayAffinity, WDA_EXCLUDEFROMCAPTURE};
    if let Ok(handle) = cc.window_handle() {
        if let RawWindowHandle::Win32(h) = handle.as_raw() {
            // SAFETY: the HWND comes straight from the live eframe window.
            unsafe { SetWindowDisplayAffinity(h.hwnd.get() as _, WDA_EXCLUDEFROMCAPTURE) };
        }
    }
}

#[cfg(not(windows))]
fn exclude_from_capture(_cc: &eframe::CreationContext<'_>) {}

// ───────────────────────────── Capture backend ─────────────────────────────

/// `probe` is a physical-pixel point on the monitor to capture (Snippy's own position).
fn capture(purpose: Purpose, mode: CaptureMode, probe: Option<Pos2>) -> Result<Capture, String> {
    let monitor = probe
        .and_then(|p| Monitor::from_point(p.x as i32, p.y as i32).ok())
        .or_else(|| {
            Monitor::all()
                .ok()?
                .into_iter()
                .find(|m| m.is_primary().unwrap_or(false))
        })
        .ok_or("no monitor found")?;

    let image = monitor.capture_image().map_err(|e| e.to_string())?;
    let windows = if mode == CaptureMode::Window {
        list_windows(&monitor, &image)
    } else {
        Vec::new()
    };
    Ok(Capture { purpose, mode, monitor_id: monitor.id().ok(), image, windows })
}

fn list_windows(monitor: &Monitor, image: &RgbaImage) -> Vec<WindowHit> {
    let (mx, my) = (monitor.x().unwrap_or(0) as f32, monitor.y().unwrap_or(0) as f32);
    let bounds = Rect::from_min_size(Pos2::ZERO, Vec2::new(image.width() as f32, image.height() as f32));
    let me = std::process::id();

    let Ok(mut windows) = Window::all() else { return Vec::new() };
    windows.sort_by_key(|w| std::cmp::Reverse(w.z().unwrap_or(0))); // top-most first

    windows
        .into_iter()
        .filter(|w| !w.is_minimized().unwrap_or(true) && w.pid().ok() != Some(me))
        .filter_map(|w| {
            let (x, y) = (w.x().ok()? as f32 - mx, w.y().ok()? as f32 - my);
            let (ww, wh) = (w.width().ok()? as f32, w.height().ok()? as f32);
            let rect = Rect::from_min_size(Pos2::new(x, y), Vec2::new(ww, wh)).intersect(bounds);
            if rect.width() < 8.0 || rect.height() < 8.0 {
                return None;
            }
            let title = w.title().unwrap_or_default();
            let title = if title.trim().is_empty() { w.app_name().unwrap_or_default() } else { title };
            Some(WindowHit { rect, title: title.trim().to_owned() })
        })
        .collect()
}

// ───────────────────────────── Image helpers ─────────────────────────────

fn crop_px(img: &RgbaImage, r: Rect) -> Option<RgbaImage> {
    let (iw, ih) = (img.width() as f32, img.height() as f32);
    let x0 = r.min.x.round().clamp(0.0, iw) as u32;
    let y0 = r.min.y.round().clamp(0.0, ih) as u32;
    let x1 = r.max.x.round().clamp(0.0, iw) as u32;
    let y1 = r.max.y.round().clamp(0.0, ih) as u32;
    (x1 > x0 && y1 > y0).then(|| imageops::crop_imm(img, x0, y0, x1 - x0, y1 - y0).to_image())
}

fn save_image(img: &RgbaImage, path: &Path) -> Result<PathBuf, String> {
    let mut path = path.to_path_buf();
    let ext = path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase);
    let res = match ext.as_deref() {
        // The JPEG encoder can't take RGBA — flatten to RGB first.
        Some("jpg" | "jpeg") => DynamicImage::ImageRgba8(img.clone()).to_rgb8().save(&path),
        Some("png") => img.save(&path),
        _ => {
            path.set_extension("png");
            img.save(&path)
        }
    };
    res.map(|()| path).map_err(|e| e.to_string())
}

/// Rasterises annotations onto a copy of `base`.
///
/// Each annotation is first drawn into its own anti-aliased coverage mask (taking the max
/// where segments overlap), then blended onto the image ONCE. This keeps translucent
/// highlighter strokes at their real opacity instead of building up to solid colour.
fn render_annotations(base: &RgbaImage, anns: &[Annotation]) -> RgbaImage {
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

fn dist_to_segment(p: Pos2, a: Pos2, b: Pos2) -> f32 {
    let ab = b - a;
    let len2 = ab.length_sq();
    let t = if len2 > 0.0 { ((p - a).dot(ab) / len2).clamp(0.0, 1.0) } else { 0.0 };
    p.distance(a + ab * t)
}

fn image_to_texture(ctx: &egui::Context, name: &str, img: &RgbaImage) -> egui::TextureHandle {
    let size = [img.width() as usize, img.height() as usize];
    let color = egui::ColorImage::from_rgba_unmultiplied(size, img.as_raw());
    ctx.load_texture(name, color, egui::TextureOptions::LINEAR)
}

fn unit_uv() -> Rect {
    Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0))
}

/// Truncate by characters, not bytes (slicing `&s[..28]` panics on multi-byte UTF-8).
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        s.chars().take(max.saturating_sub(1)).chain(std::iter::once('…')).collect()
    }
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size(BAR_SIZE)
            .with_title("Snippy")
            .with_decorations(false)
            .with_always_on_top()
            .with_transparent(true),
        ..Default::default()
    };
    eframe::run_native("Snippy", options, Box::new(|cc| {
            exclude_from_capture(cc);
            Ok(Box::new(SnippyApp::default()))
        }))
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

    #[test]
    fn crop_clamps_to_image() {
        let img = solid(50, 50);
        let c = crop_px(&img, Rect::from_min_max(Pos2::new(-10.0, 40.0), Pos2::new(100.0, 100.0))).unwrap();
        assert_eq!((c.width(), c.height()), (50, 10));
        assert!(crop_px(&img, Rect::from_min_max(Pos2::new(60.0, 0.0), Pos2::new(70.0, 10.0))).is_none());
    }

    #[test]
    fn truncate_is_utf8_safe() {
        let t = "😀".repeat(40);
        assert_eq!(truncate_chars(&t, 10).chars().count(), 10);
    }

    #[test]
    fn jpeg_save_works_from_rgba() {
        let dir = std::env::temp_dir().join(format!("snippy-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = save_image(&solid(8, 8), &dir.join("a.jpg")).unwrap();
        assert!(p.exists());
        let p = save_image(&solid(8, 8), &dir.join("noext")).unwrap();
        assert_eq!(p.extension().unwrap(), "png");
        let _ = std::fs::remove_dir_all(dir);
    }
}
