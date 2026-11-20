use super::*;
use crate::icons::{self, Icon};
use egui::{Sense, Stroke, StrokeKind};
use std::collections::HashMap;
use std::sync::Arc;

// ───────────────────────────── Capture history ─────────────────────────────
//
// Recent snips and recordings: kept in `%LOCALAPPDATA%\Snippy\history.json` with small
// thumbnails in `…\Snippy\thumbs\`. Snips that weren't saved (auto-save off) are kept in
// memory for the session only, so nothing is written to disk behind the user's back.

const MAX_ITEMS: usize = 60;
const TITLE: &str = "Snippy history";

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum HistoryKind {
    Snip,
    Recording,
}

#[derive(Serialize, Deserialize, Clone)]
pub(crate) struct HistoryItem {
    pub(crate) kind: HistoryKind,
    /// The saved file (None = an unsaved snip, kept in memory this session).
    pub(crate) path: Option<PathBuf>,
    pub(crate) thumb: Option<PathBuf>,
    /// Local time, e.g. "2026-11-20 13:40".
    pub(crate) when: String,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) duration_secs: Option<u64>,
    /// Unique id (also the thumbnail file name).
    pub(crate) id: String,
    #[serde(skip)]
    pub(crate) image: Option<Arc<RgbaImage>>,
}

fn dir() -> PathBuf {
    diag::path().parent().map(Path::to_path_buf).unwrap_or_else(std::env::temp_dir)
}

fn file() -> PathBuf {
    dir().join("history.json")
}

pub(crate) fn load() -> Vec<HistoryItem> {
    std::fs::read(file())
        .ok()
        .and_then(|b| serde_json::from_slice::<Vec<HistoryItem>>(&b).ok())
        .unwrap_or_default()
}

fn save(items: &[HistoryItem]) {
    let keep: Vec<&HistoryItem> = items.iter().filter(|i| i.path.is_some()).collect();
    if let Ok(json) = serde_json::to_vec_pretty(&keep) {
        let _ = std::fs::create_dir_all(dir());
        let _ = std::fs::write(file(), json);
    }
}

fn new_id() -> String {
    chrono::Local::now().format("%Y%m%d-%H%M%S%3f").to_string()
}

/// Writes a small thumbnail to the cache; returns its path.
fn write_thumb(id: &str, img: &RgbaImage) -> Option<PathBuf> {
    let p = dir().join("thumbs").join(format!("{id}.png"));
    std::fs::create_dir_all(p.parent()?).ok()?;
    record::make_thumbnail(img, 320).save(&p).ok()?;
    Some(p)
}

pub(crate) struct HistoryUi {
    textures: HashMap<String, Option<egui::TextureHandle>>,
}

impl HistoryUi {
    pub(crate) fn new() -> Self {
        Self { textures: HashMap::new() }
    }
}

enum Act {
    Copy,
    Edit,
    Pin,
    Open,
    Folder,
    Remove,
}

impl SnippyApp {
    pub(crate) fn history_add_snip(&mut self, img: &RgbaImage, path: Option<PathBuf>) {
        let id = new_id();
        let item = HistoryItem {
            kind: HistoryKind::Snip,
            thumb: path.as_ref().and_then(|_| write_thumb(&id, img)),
            image: if path.is_none() { Some(Arc::new(img.clone())) } else { None },
            path,
            when: chrono::Local::now().format("%Y-%m-%d %H:%M").to_string(),
            width: img.width(),
            height: img.height(),
            duration_secs: None,
            id,
        };
        self.push_history(item);
    }

    pub(crate) fn history_add_recording(&mut self, info: &RecordingInfo) {
        let id = new_id();
        let item = HistoryItem {
            kind: HistoryKind::Recording,
            thumb: info.thumbnail.as_ref().and_then(|t| write_thumb(&id, t)),
            image: None,
            path: Some(info.path.clone()),
            when: chrono::Local::now().format("%Y-%m-%d %H:%M").to_string(),
            width: info.width,
            height: info.height,
            duration_secs: Some(info.duration.as_secs()),
            id,
        };
        self.push_history(item);
    }

    fn push_history(&mut self, item: HistoryItem) {
        self.history.insert(0, item);
        while self.history.len() > MAX_ITEMS {
            if let Some(old) = self.history.pop() {
                if let Some(t) = old.thumb {
                    let _ = std::fs::remove_file(t);
                }
            }
        }
        save(&self.history);
    }

    fn history_image(item: &HistoryItem) -> Option<RgbaImage> {
        if let Some(img) = &item.image {
            return Some((**img).clone());
        }
        image::open(item.path.as_ref()?).ok().map(|i| i.to_rgba8())
    }

    pub(crate) fn history_window(&mut self, ctx: &egui::Context) {
        let builder = egui::ViewportBuilder::default()
            .with_title(TITLE)
            .with_inner_size([760.0, 600.0])
            .with_min_inner_size([420.0, 300.0])
            .with_resizable(true);
        let mut action: Option<(usize, Act)> = None;
        let mut clear = false;
        let tint = self.settings.look.tint;
        ctx.show_viewport_immediate(egui::ViewportId::from_hash_of("snippy_history"), builder, |ui, _| {
            if ui.input(|i| i.viewport().close_requested()) {
                self.history_open = false;
            }
            egui::Panel::top("hist_top").show(ui, |ui| {
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.heading("Recent captures");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if !self.history.is_empty() && ui.button("Clear list").on_hover_text("Removes the list only; your files stay where they are").clicked() {
                            clear = true;
                        }
                        ui.weak(format!("{} item{}", self.history.len(), if self.history.len() == 1 { "" } else { "s" }));
                    });
                });
                ui.add_space(4.0);
            });
            egui::CentralPanel::default().show(ui, |ui| {
                if self.history.is_empty() {
                    ui.centered_and_justified(|ui| ui.weak("Your snips and recordings will show up here."));
                    return;
                }
                egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                    let card = Vec2::new(220.0, 176.0);
                    let cols = ((ui.available_width() + 10.0) / (card.x + 10.0)).floor().max(1.0) as usize;
                    egui::Grid::new("hist_grid").spacing([10.0, 10.0]).show(ui, |ui| {
                        for (i, item) in self.history.iter().enumerate() {
                            let (rect, resp) = ui.allocate_exact_size(card, Sense::hover());
                            let p = ui.painter();
                            let hovered = resp.hovered() || ui.rect_contains_pointer(rect);
                            p.rect_filled(rect, 8.0, Color32::from_white_alpha(if hovered { 16 } else { 8 }));
                            p.rect_stroke(rect, 8.0, Stroke::new(1.0, if hovered { tint } else { Color32::from_white_alpha(30) }), StrokeKind::Inside);
                            let img_box = Rect::from_min_size(rect.min + Vec2::new(8.0, 8.0), Vec2::new(card.x - 16.0, 112.0));
                            p.rect_filled(img_box, 6.0, Color32::from_black_alpha(80));
                            // thumbnail (cached texture)
                            let tex = self.history_ui.textures.entry(item.id.clone()).or_insert_with(|| {
                                let img = match (&item.thumb, &item.image) {
                                    (Some(t), _) => image::open(t).ok().map(|i| i.to_rgba8()),
                                    (None, Some(img)) => Some(record::make_thumbnail(img, 320)),
                                    _ => None,
                                };
                                img.map(|i| image_to_texture(ui.ctx(), &format!("hist_{}", item.id), &i))
                            });
                            if let Some(t) = tex {
                                let sz = t.size_vec2();
                                let s = (img_box.width() / sz.x).min(img_box.height() / sz.y);
                                p.image(t.id(), Rect::from_center_size(img_box.center(), sz * s), unit_uv(), Color32::WHITE);
                            }
                            if item.kind == HistoryKind::Recording {
                                p.circle_filled(img_box.center(), 14.0, Color32::from_black_alpha(150));
                                icons::draw(p, Icon::Play, Rect::from_center_size(img_box.center() + Vec2::new(1.0, 0.0), Vec2::splat(16.0)), Color32::WHITE);
                            }
                            let missing = item.path.as_ref().is_some_and(|p| !p.exists());
                            let caption = match item.kind {
                                HistoryKind::Snip => format!("{} × {}", item.width, item.height),
                                HistoryKind::Recording => {
                                    let d = item.duration_secs.unwrap_or(0);
                                    format!("Video {}:{:02}", d / 60, d % 60)
                                }
                            };
                            p.text(Pos2::new(rect.min.x + 10.0, img_box.max.y + 8.0), egui::Align2::LEFT_TOP, &item.when,
                                egui::FontId::proportional(12.5), Color32::from_gray(225));
                            p.text(Pos2::new(rect.min.x + 10.0, img_box.max.y + 26.0), egui::Align2::LEFT_TOP,
                                if missing { "file moved or deleted".to_string() } else { caption },
                                egui::FontId::proportional(11.5), if missing { Color32::from_rgb(255, 160, 90) } else { Color32::from_gray(160) });

                            // hover actions
                            if hovered {
                                let acts: Vec<(Act, Icon, &str)> = match (item.kind, missing) {
                                    (_, true) => vec![(Act::Remove, Icon::Trash, "Remove from list")],
                                    (HistoryKind::Snip, false) => {
                                        let mut v = vec![
                                            (Act::Copy, Icon::Copy, "Copy"),
                                            (Act::Edit, Icon::Edit, "Edit"),
                                            (Act::Pin, Icon::Pin, "Pin to screen"),
                                        ];
                                        if item.path.is_some() {
                                            v.push((Act::Folder, Icon::Folder, "Show in folder"));
                                        }
                                        v.push((Act::Remove, Icon::Trash, "Remove from list"));
                                        v
                                    }
                                    (HistoryKind::Recording, false) => vec![
                                        (Act::Open, Icon::Play, "Play"),
                                        (Act::Folder, Icon::Folder, "Show in folder"),
                                        (Act::Remove, Icon::Trash, "Remove from list"),
                                    ],
                                };
                                let n = acts.len() as f32;
                                let w = n * 30.0 + 8.0;
                                let bar = Rect::from_min_size(Pos2::new(img_box.center().x - w / 2.0, img_box.max.y - 36.0), Vec2::new(w, 30.0));
                                p.rect_filled(bar, 15.0, Color32::from_black_alpha(190));
                                for (k, (act, icon, tip)) in acts.into_iter().enumerate() {
                                    let r = Rect::from_min_size(bar.min + Vec2::new(4.0 + k as f32 * 30.0, 2.0), Vec2::splat(26.0));
                                    let b = ui.interact(r, ui.scope_id().with(("hist", i, k)), Sense::click()).on_hover_text(tip);
                                    if b.hovered() {
                                        ui.painter().rect_filled(r, 13.0, Color32::from_white_alpha(40));
                                    }
                                    icons::draw(ui.painter(), icon, r.shrink(5.0), Color32::WHITE);
                                    if b.clicked() {
                                        action = Some((i, act));
                                    }
                                }
                            }
                            if (i + 1) % cols == 0 {
                                ui.end_row();
                            }
                        }
                    });
                });
            });
        });

        if clear {
            for it in self.history.drain(..) {
                if let Some(t) = it.thumb {
                    let _ = std::fs::remove_file(t);
                }
            }
            self.history_ui.textures.clear();
            save(&self.history);
        }
        if let Some((i, act)) = action {
            let Some(item) = self.history.get(i).cloned() else { return };
            match act {
                Act::Copy => {
                    if let Some(img) = Self::history_image(&item) {
                        let _ = self.copy_to_clipboard(&img);
                    }
                }
                Act::Edit => {
                    if let Some(img) = Self::history_image(&item) {
                        self.history_open = false;
                        self.show_editor(ctx, img);
                    }
                }
                Act::Pin => {
                    if let Some(img) = Self::history_image(&item) {
                        self.add_pin(ctx, img);
                    }
                }
                Act::Open => {
                    if let Some(p) = &item.path {
                        let _ = open_path(p);
                    }
                }
                Act::Folder => {
                    if let Some(p) = &item.path {
                        let _ = reveal_in_folder(p);
                    }
                }
                Act::Remove => {
                    let it = self.history.remove(i);
                    if let Some(t) = it.thumb {
                        let _ = std::fs::remove_file(t);
                    }
                    self.history_ui.textures.remove(&it.id);
                    save(&self.history);
                }
            }
        }
    }
}
