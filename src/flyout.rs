use super::*;
use crate::bar::{paint_glass_panel_min, style_child_window};
use crate::icons::{self, Icon};
use egui::{Sense, Stroke, StrokeKind};

// ───────────────────────────── Notification flyout ─────────────────────────────
//
// A small glass card in the bottom-right corner (above the taskbar) after a snip or a
// recording: thumbnail, where it was saved, and quick actions. Auto-dismisses unless hovered.

const W: f32 = 590.0;
const H: f32 = 136.0;
const NOTE_H: f32 = 16.0;
const TITLE: &str = "Snippy notification";

/// Where flyouts go: a monitor's work area (screen minus taskbar) in physical pixels, and
/// that monitor's display scale. Placing in physical pixels keeps the flyout in the right
/// corner even when monitors use different scaling (logical coordinates don't map
/// consistently across mixed-DPI monitors).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Anchor {
    pub(crate) work: Rect,
    pub(crate) scale: f32,
}

pub(crate) enum FlyoutKind {
    Snip { image: RgbaImage, path: Option<PathBuf> },
    Recording(RecordingInfo),
    Update(updater::Release),
    /// Text recognised from the screen (already on the clipboard).
    Text(String),
    /// A colour picked from the screen.
    Color([u8; 3]),
    Message,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum FlyoutAction {
    Close,
    Edit,
    Copy,
    SaveAs,
    OpenFolder,
    Play,
    Delete,
    Update,
    ReleaseNotes,
    Pin,
    CopyText,
    CopyHex,
    CopyRgb,
}

pub(crate) struct Flyout {
    pub(crate) kind: FlyoutKind,
    pub(crate) title: String,
    pub(crate) subtitle: String,
    pub(crate) notes: Vec<String>,
    thumb: Option<egui::TextureHandle>,
    thumb_size: Vec2,
    anchor: Anchor,
    lifetime: f64,
    deadline: Option<f64>,
    styled_gen: Option<u64>,
}

fn folder_label(path: &Path) -> String {
    path.parent().map(|p| format!("Saved in {}", p.display())).unwrap_or_default()
}

impl Flyout {
    fn new(kind: FlyoutKind, title: String, subtitle: String, notes: Vec<String>, thumb: Option<(&egui::Context, &RgbaImage)>, anchor: Anchor) -> Self {
        let (thumb, thumb_size) = match thumb {
            Some((ctx, img)) => {
                let t = record::make_thumbnail(img, 360);
                let size = Vec2::new(t.width() as f32, t.height() as f32);
                (Some(image_to_texture(ctx, "flyout_thumb", &t)), size)
            }
            None => (None, Vec2::ZERO),
        };
        let lifetime = if notes.is_empty() { 8.0 } else { 15.0 };
        Self { kind, title, subtitle, notes, thumb, thumb_size, anchor, lifetime, deadline: None, styled_gen: None }
    }

    pub(crate) fn snip(ctx: &egui::Context, image: RgbaImage, path: Option<PathBuf>, copied: Result<(), String>, save_err: Option<String>, anchor: Anchor) -> Self {
        let mut notes = Vec::new();
        let title = match &copied {
            Ok(()) => "Copied to clipboard".to_string(),
            Err(e) => {
                notes.push(format!("Couldn't copy to clipboard: {e}"));
                "Screenshot taken".to_string()
            }
        };
        notes.extend(save_err.map(|e| format!("Couldn't save: {e}")));
        let subtitle = path.as_deref().map(folder_label).unwrap_or_else(|| "Not saved".into());
        let mut f = Self::new(FlyoutKind::Message, title, subtitle, notes, Some((ctx, &image)), anchor);
        f.kind = FlyoutKind::Snip { image, path };
        f
    }

    pub(crate) fn recording(ctx: &egui::Context, info: RecordingInfo, anchor: Anchor) -> Self {
        let secs = info.duration.as_secs();
        let size = if info.bytes >= 1_048_576 {
            format!("{:.1} MB", info.bytes as f64 / 1_048_576.0)
        } else {
            format!("{} KB", info.bytes.div_ceil(1024))
        };
        let title = format!("Recording saved  ·  {}:{:02}  ·  {size}", secs / 60, secs % 60);
        let mut notes = info.notes.clone();
        if info.dropped > 0 && info.dropped * 10 > info.frames {
            notes.push(format!("{} frames were skipped to keep up", info.dropped));
        }
        let subtitle = folder_label(&info.path);
        let thumb_img = info.thumbnail.clone();
        let mut f = Self::new(FlyoutKind::Message, title, subtitle, notes, thumb_img.as_ref().map(|i| (ctx, i)), anchor);
        f.kind = FlyoutKind::Recording(info);
        f
    }

    pub(crate) fn update(rel: updater::Release, anchor: Anchor) -> Self {
        let title = format!("Snippy {} is available", updater::version_string(rel.version));
        let subtitle = format!("You have {}. Updating takes a few seconds.", updater::version_string(updater::current_version()));
        let mut f = Self::new(FlyoutKind::Message, title, subtitle, Vec::new(), None, anchor);
        f.lifetime = 20.0;
        f.kind = FlyoutKind::Update(rel);
        f
    }

    /// Text copied from the screen: shows a preview.
    pub(crate) fn text(text: String, anchor: Anchor) -> Self {
        let lines = text.lines().filter(|l| !l.trim().is_empty()).count();
        let words = text.split_whitespace().count();
        let (title, subtitle) = if text.trim().is_empty() {
            ("No text found".to_string(), "Try selecting a larger area, or text with more contrast".to_string())
        } else {
            (
                format!("Text copied  ·  {words} word{}, {lines} line{}", if words == 1 { "" } else { "s" }, if lines == 1 { "" } else { "s" }),
                text.split_whitespace().collect::<Vec<_>>().join(" "),
            )
        };
        let mut f = Self::new(FlyoutKind::Message, title, subtitle, Vec::new(), None, anchor);
        f.kind = FlyoutKind::Text(text);
        f
    }

    /// A picked colour (hex already on the clipboard).
    pub(crate) fn color(rgb: [u8; 3], anchor: Anchor) -> Self {
        let [r, g, b] = rgb;
        let title = format!("Copied #{r:02X}{g:02X}{b:02X}");
        let subtitle = format!("rgb({r}, {g}, {b})");
        let mut f = Self::new(FlyoutKind::Message, title, subtitle, Vec::new(), None, anchor);
        f.kind = FlyoutKind::Color(rgb);
        f
    }

    pub(crate) fn message(title: impl Into<String>, detail: impl Into<String>, anchor: Anchor) -> Self {
        Self::new(FlyoutKind::Message, title.into(), detail.into(), Vec::new(), None, anchor)
    }

    pub(crate) fn path(&self) -> Option<&Path> {
        match &self.kind {
            FlyoutKind::Snip { path, .. } => path.as_deref(),
            FlyoutKind::Recording(info) => Some(&info.path),
            FlyoutKind::Update(_) | FlyoutKind::Text(_) | FlyoutKind::Color(_) | FlyoutKind::Message => None,
        }
    }

    pub(crate) fn set_path(&mut self, new: PathBuf) {
        self.subtitle = folder_label(&new);
        match &mut self.kind {
            FlyoutKind::Snip { path, .. } => *path = Some(new),
            FlyoutKind::Recording(info) => info.path = new,
            FlyoutKind::Update(_) | FlyoutKind::Text(_) | FlyoutKind::Color(_) | FlyoutKind::Message => {}
        }
    }

    /// Restart the auto-dismiss timer (e.g. after an action) with a status line.
    pub(crate) fn flash(&mut self, msg: impl Into<String>) {
        self.subtitle = msg.into();
        self.deadline = None;
    }

    /// Shows the flyout window. Returns an action if a button was clicked.
    /// Keep the flyout open (e.g. while an update downloads).
    pub(crate) fn hold(&mut self) {
        self.deadline = None;
        self.lifetime = 3600.0;
    }

    pub(crate) fn show(&mut self, ctx: &egui::Context, look: &Look, glass: bool, style_gen: u64) -> Option<FlyoutAction> {
        let now = ctx.input(|i| i.time);
        let deadline = *self.deadline.get_or_insert(now + self.lifetime);
        if now > deadline {
            return Some(FlyoutAction::Close);
        }
        ctx.request_repaint_after(Duration::from_millis(250));

        let size = Vec2::new(W, H + NOTE_H * self.notes.len().min(3) as f32);
        // Bottom-right of the work area, in physical pixels.
        let sc = self.anchor.scale;
        let max = self.anchor.work.max - Vec2::splat(14.0 * sc);
        let rect_px = Rect::from_min_max(max - size * sc, max);
        let pos = rect_px.min / sc; // initial guess in points; corrected below
        let builder = egui::ViewportBuilder::default()
            .with_title(TITLE)
            .with_position(pos)
            .with_inner_size(size)
            .with_decorations(false)
            .with_transparent(true)
            .with_always_on_top()
            .with_taskbar(false)
            .with_resizable(false)
            .with_active(false);

        let mut action = None;
        let mut hovered = false;
        let look = *look;
        ctx.show_viewport_immediate(egui::ViewportId::from_hash_of("snippy_flyout"), builder, |ui, _| {
            let full = ui.max_rect();
            hovered = ui.rect_contains_pointer(full);
            paint_glass_panel_min(ui.painter(), full, &look, glass, 10.0, 0.55);
            let p = ui.painter().clone();

            // Close (×), top-right
            let close = Rect::from_min_size(Pos2::new(full.max.x - 32.0, full.min.y + 8.0), Vec2::splat(24.0));
            let r = ui.interact(close, ui.scope_id().with("close"), Sense::click());
            if r.hovered() {
                p.rect_filled(close, 6.0, Color32::from_white_alpha(28));
            }
            icons::draw(&p, Icon::Close, close.shrink(5.0), Color32::from_gray(200));
            if r.clicked() {
                action = Some(FlyoutAction::Close);
            }

            // Thumbnail (or colour swatch)
            let mut text_x = full.min.x + 18.0;
            if let FlyoutKind::Color([r, g, b]) = self.kind {
                let boxr = Rect::from_min_size(full.min + Vec2::new(12.0, 12.0), Vec2::new(150.0, H - 24.0));
                p.rect_filled(boxr, 8.0, Color32::from_rgb(r, g, b));
                p.rect_stroke(boxr, 8.0, Stroke::new(1.0, Color32::from_white_alpha(90)), StrokeKind::Inside);
                text_x = boxr.max.x + 16.0;
            }
            if let Some(tex) = &self.thumb {
                let boxr = Rect::from_min_size(full.min + Vec2::new(12.0, 12.0), Vec2::new(150.0, H - 24.0));
                let scale = (boxr.width() / self.thumb_size.x).min(boxr.height() / self.thumb_size.y);
                let img_r = Rect::from_center_size(boxr.center(), self.thumb_size * scale);
                p.rect_filled(boxr, 8.0, Color32::from_black_alpha(90));
                p.image(tex.id(), img_r, unit_uv(), Color32::WHITE);
                p.rect_stroke(img_r, 4.0, Stroke::new(1.0, look.tint.gamma_multiply(0.6)), StrokeKind::Outside);
                if matches!(self.kind, FlyoutKind::Recording(_)) {
                    p.circle_filled(img_r.center(), 16.0, Color32::from_black_alpha(150));
                    icons::draw(&p, Icon::Play, Rect::from_center_size(img_r.center() + Vec2::new(1.5, 0.0), Vec2::splat(20.0)), Color32::WHITE);
                }
                let thumb_click = ui.interact(img_r, ui.scope_id().with("thumb"), Sense::click());
                if thumb_click.clicked() {
                    action = Some(match self.kind {
                        FlyoutKind::Recording(_) => FlyoutAction::Play,
                        _ => FlyoutAction::Edit,
                    });
                }
                text_x = boxr.max.x + 16.0;
            }

            // Texts
            let max_chars = ((full.max.x - 40.0 - text_x) / 6.6) as usize;
            p.text(Pos2::new(text_x, full.min.y + 16.0), egui::Align2::LEFT_TOP, truncate_chars(&self.title, max_chars),
                egui::FontId::proportional(16.5), Color32::WHITE);
            p.text(Pos2::new(text_x, full.min.y + 41.0), egui::Align2::LEFT_TOP, truncate_chars(&self.subtitle, max_chars + 8),
                egui::FontId::proportional(12.0), Color32::from_gray(190));
            for (i, note) in self.notes.iter().take(3).enumerate() {
                p.text(Pos2::new(text_x, full.min.y + 60.0 + i as f32 * NOTE_H), egui::Align2::LEFT_TOP,
                    truncate_chars(note, max_chars + 10), egui::FontId::proportional(11.5), Color32::from_rgb(255, 196, 90));
            }

            // Buttons
            let buttons: Vec<(FlyoutAction, Icon, String)> = match &self.kind {
                FlyoutKind::Snip { .. } => vec![
                    (FlyoutAction::Edit, Icon::Edit, "Edit".into()),
                    (FlyoutAction::Copy, Icon::Copy, "Copy".into()),
                    (FlyoutAction::Pin, Icon::Pin, "Pin".into()),
                    (FlyoutAction::SaveAs, Icon::Save, "Save as".into()),
                    (FlyoutAction::OpenFolder, Icon::Folder, String::new()),
                ],
                FlyoutKind::Recording(_) => vec![
                    (FlyoutAction::Play, Icon::Play, "Play".into()),
                    (FlyoutAction::SaveAs, Icon::Save, "Save as".into()),
                    (FlyoutAction::Delete, Icon::Trash, "Delete".into()),
                    (FlyoutAction::OpenFolder, Icon::Folder, String::new()),
                ],
                FlyoutKind::Update(_) => vec![
                    (FlyoutAction::Update, Icon::Refresh, "Update now".into()),
                    (FlyoutAction::ReleaseNotes, Icon::Edit, "What's new".into()),
                ],
                FlyoutKind::Text(t) if !t.trim().is_empty() => vec![(FlyoutAction::CopyText, Icon::Copy, "Copy again".into())],
                FlyoutKind::Color([r, g, b]) => vec![
                    (FlyoutAction::CopyHex, Icon::Copy, format!("#{r:02X}{g:02X}{b:02X}")),
                    (FlyoutAction::CopyRgb, Icon::Copy, format!("rgb({r}, {g}, {b})")),
                ],
                FlyoutKind::Text(_) | FlyoutKind::Message => vec![],
            };
            let mut x = text_x;
            let y = full.max.y - 14.0 - 32.0;
            for (act, icon, label) in buttons {
                let label = label.as_str();
                let font = egui::FontId::proportional(13.5);
                let galley = p.layout_no_wrap(label.to_string(), font, Color32::WHITE);
                let w = if label.is_empty() { 32.0 } else { 36.0 + galley.size().x + 12.0 };
                let r = Rect::from_min_size(Pos2::new(x, y), Vec2::new(w, 32.0));
                x += w + 8.0;
                let resp = ui.interact(r, ui.scope_id().with(label).with(icon as u8), Sense::click());
                let fill = if resp.is_pointer_button_down_on() { 50 } else if resp.hovered() { 34 } else { 16 };
                p.rect_filled(r, 16.0, Color32::from_white_alpha(fill));
                p.rect_stroke(r, 16.0, Stroke::new(1.0, look.tint.gamma_multiply(0.45)), StrokeKind::Inside);
                let icon_r = Rect::from_center_size(Pos2::new(r.min.x + 17.0, r.center().y), Vec2::splat(17.0));
                let icon_r = if label.is_empty() { Rect::from_center_size(r.center(), Vec2::splat(17.0)) } else { icon_r };
                icons::draw(&p, icon, icon_r, Color32::WHITE);
                if !label.is_empty() {
                    p.galley(Pos2::new(r.min.x + 32.0, r.center().y - galley.size().y / 2.0), galley, Color32::WHITE);
                }
                if resp.clicked() {
                    action = Some(act);
                }
            }
        });

        platform::place_window(TITLE, [rect_px.min.x, rect_px.min.y, rect_px.max.x, rect_px.max.y].map(|v| v.round() as i32));
        if hovered {
            self.deadline = Some(now + 4.0f64.max(deadline - now));
        }
        if self.styled_gen != Some(style_gen) && style_child_window(TITLE, look.backdrop).is_some() {
            self.styled_gen = Some(style_gen);
        }
        action
    }
}
