#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Snippy — a tiny floating screen snipping & recording tool.
//!
//! Snip:   Bar ──▶ Capturing (hidden) ──▶ Overlay (region / window) ──▶ copy + save ──▶ flyout
//!                                     └──▶ (full screen skips the overlay)
//! Record: Bar ──▶ Capturing (hidden) ──▶ Overlay ──▶ Recording (mini bar) ──▶ flyout
//! Edit:   flyout "Edit" ──▶ Editor

mod audio;
mod bar;
mod capture;
mod editor;
mod flyout;
mod hotkeys;
mod icons;
#[cfg(windows)]
mod mf;
mod overlay;
mod platform;
mod prefs;
mod record;
mod tray;
mod updater;
mod util;

use arboard::{Clipboard, ImageData};
use capture::*;
use editor::*;
use eframe::egui::{self, Color32, Pos2, Rect, Vec2};
use flyout::{Flyout, FlyoutAction, FlyoutKind};
use image::{imageops, DynamicImage, Rgba, RgbaImage};
use overlay::*;
use platform::{Backdrop, Folder};
use prefs::UpdateState;
use record::{RecordTarget, Recorder, RecordingInfo};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::Duration;
use util::*;
use xcap::{Monitor, Window};

/// Warm accent (selection, outlines) — echoes the Windows 11 "glass" look.
const ACCENT: Color32 = Color32::from_rgb(255, 196, 77);
const REC_RED: Color32 = Color32::from_rgb(232, 58, 58);
const RECORD_FPS: u32 = 30;
/// App icon artwork (window/taskbar icon; the .exe icon is embedded by build.rs).
const ICON_PNG: &[u8] = include_bytes!("../assets/snippy.png");
/// Simplified artwork that stays legible at tray size.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) const TRAY_PNG: &[u8] = include_bytes!("../assets/snippy-tray.png");

/// Things that happen outside the UI (tray menu, global shortcuts).
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) enum AppEvent {
    Tray(tray::TrayCmd),
    Hotkey(hotkeys::Hotkey),
}

/// Are we taking a screenshot or a video?
#[derive(PartialEq, Eq, Clone, Copy, Debug)]
enum Purpose {
    Snip,
    Record,
}

#[derive(PartialEq, Eq, Clone, Copy, Debug, Serialize, Deserialize)]
enum CaptureMode {
    Region,
    Window,
    FullScreen,
}

/// How the floating windows look (Settings → Appearance).
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
#[serde(default)]
struct Look {
    backdrop: Backdrop,
    tint: Color32,
    /// 0..1 — how much of the tint colour is mixed in.
    tint_strength: f32,
    /// 0..1 — milky white layer; lower = clearer.
    frosting: f32,
    /// 0..1 — dark layer behind the icons.
    darkness: f32,
}

impl Default for Look {
    fn default() -> Self {
        Self { backdrop: Backdrop::Blur, tint: ACCENT, tint_strength: 0.10, frosting: 0.08, darkness: 0.45 }
    }
}

/// Everything remembered between runs.
#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
struct Settings {
    mode: CaptureMode,
    record_mode: bool,
    delay_secs: u64,
    system_audio: bool,
    microphone: bool,
    tool: Tool,
    pen_color: Color32,
    highlighter_color: Color32,
    brush_size: f32,
    /// `None` = default (`Pictures\Screenshots`).
    screenshot_dir: Option<PathBuf>,
    /// `None` = default (`Videos\Snippy`).
    video_dir: Option<PathBuf>,
    auto_save: bool,
    fps: u32,
    show_cursor: bool,
    look: Look,
    check_updates: bool,
    tray_icon: bool,
    /// Tray-only mode: the floating bar stays hidden until shown from the tray.
    hide_bar: bool,
    hotkey_print_screen: bool,
    hotkey_win_shift_s: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            mode: CaptureMode::Region,
            record_mode: false,
            delay_secs: 0,
            system_audio: true,
            microphone: false,
            tool: Tool::Pen,
            pen_color: Color32::from_rgb(230, 40, 40),
            highlighter_color: Color32::from_rgba_unmultiplied(255, 235, 50, 110),
            brush_size: 4.0,
            screenshot_dir: None,
            video_dir: None,
            auto_save: true,
            fps: RECORD_FPS,
            show_cursor: true,
            look: Look::default(),
            check_updates: true,
            tray_icon: true,
            hide_bar: false,
            hotkey_print_screen: true,
            hotkey_win_shift_s: true,
        }
    }
}

enum Phase {
    Bar,
    Capturing,
    Overlay(Overlay),
    Editor,
    Recording { recorder: Recorder, border: Option<Rect> },
}

struct SnippyApp {
    phase: Phase,
    settings: Settings,
    status: String,
    capture_rx: Option<Receiver<Result<Capture, String>>>,
    editor: Option<Editor>,
    clipboard: Option<Clipboard>,
    flyout: Option<Flyout>,
    /// A glass backdrop is active (our painted layers go on top of it).
    glass: bool,
    main_hwnd: Option<platform::Hwnd>,
    /// Bumped when the look changes, so secondary windows re-style themselves.
    style_gen: u64,
    /// Hovered bar tooltip: (text, hover start time, style generation applied).
    tip: Option<(String, f64, Option<u64>)>,
    settings_open: bool,
    settings_styled: bool,
    update: UpdateState,
    /// Cached "start with Windows" state (reading the registry every frame is wasteful).
    startup: Option<bool>,
    prefs_status: String,
    tray: Option<tray::Tray>,
    /// Physical-pixel point on the monitor of the last capture (for placing the flyout).
    last_probe: Option<Pos2>,
    events_tx: mpsc::Sender<AppEvent>,
    events_rx: Receiver<AppEvent>,
}

impl SnippyApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let settings: Settings = cc.storage.and_then(|s| eframe::get_value(s, eframe::APP_KEY)).unwrap_or_default();
        // Glass backdrop + rounded corners + hidden from screen capture (so the bar is never
        // in screenshots or recordings).
        let main_hwnd = platform::main_hwnd(cc);
        let glass = main_hwnd.is_some_and(|h| platform::style_window(h, settings.look.backdrop));
        updater::cleanup_old();
        platform::refresh_startup_path();
        let (events_tx, events_rx) = mpsc::channel();
        hotkeys::install(&cc.egui_ctx, events_tx.clone());
        hotkeys::configure(settings.hotkey_print_screen, settings.hotkey_win_shift_s);
        let mut app = Self {
            phase: Phase::Bar,
            settings,
            status: "Ready".into(),
            capture_rx: None,
            editor: None,
            clipboard: None,
            flyout: None,
            glass,
            main_hwnd,
            style_gen: 0,
            tip: None,
            settings_open: false,
            settings_styled: false,
            update: UpdateState::Idle,
            startup: None,
            prefs_status: String::new(),
            tray: None,
            last_probe: None,
            events_tx,
            events_rx,
        };
        app.sync_tray(&cc.egui_ctx);
        if app.settings.check_updates {
            app.check_for_updates(&cc.egui_ctx, false);
        }
        app
    }

    /// Creates/removes the tray icon to match the setting.
    fn sync_tray(&mut self, ctx: &egui::Context) {
        match (self.settings.tray_icon, self.tray.is_some()) {
            (true, false) => match tray::Tray::new(ctx, self.events_tx.clone()) {
                Ok(t) => self.tray = Some(t),
                Err(e) => {
                    self.settings.tray_icon = false;
                    self.prefs_status = format!("Couldn't add the tray icon: {e}");
                }
            },
            (false, true) => self.tray = None,
            _ => {}
        }
        if self.tray.is_none() && self.settings.hide_bar {
            // Without a tray icon there'd be no way back to a hidden bar.
            self.settings.hide_bar = false;
            if matches!(self.phase, Phase::Bar) {
                self.show_bar(ctx);
            }
        }
    }

    /// Bar hidden in tray-only mode.
    fn bar_hidden(&self) -> bool {
        self.settings.hide_bar && self.tray.is_some()
    }

    fn handle_events(&mut self, ctx: &egui::Context) {
        while let Ok(ev) = self.events_rx.try_recv() {
            let snip = |app: &mut Self, mode: Option<CaptureMode>| {
                if let Some(m) = mode {
                    app.settings.mode = m;
                }
                app.start_capture(ctx, Purpose::Snip);
            };
            match ev {
                // Win+Shift+S always snips a region (like Windows); Print Screen uses the
                // mode selected on the bar.
                AppEvent::Hotkey(hotkeys::Hotkey::WinShiftS) => snip(self, Some(CaptureMode::Region)),
                AppEvent::Hotkey(hotkeys::Hotkey::PrintScreen) => snip(self, None),
                AppEvent::Tray(cmd) => match cmd {
                    tray::TrayCmd::SnipRegion => snip(self, Some(CaptureMode::Region)),
                    tray::TrayCmd::SnipWindow => snip(self, Some(CaptureMode::Window)),
                    tray::TrayCmd::SnipScreen => snip(self, Some(CaptureMode::FullScreen)),
                    tray::TrayCmd::Record => self.start_capture(ctx, Purpose::Record),
                    tray::TrayCmd::ToggleBar => {
                        self.settings.hide_bar = !self.settings.hide_bar;
                        if matches!(self.phase, Phase::Bar) {
                            self.show_bar(ctx);
                            if !self.bar_hidden() {
                                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                            }
                        }
                    }
                    tray::TrayCmd::Settings => self.settings_open = true,
                    tray::TrayCmd::Exit => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
                },
            }
        }
    }

    /// Re-applies the glass style to every window after it changed in Settings.
    fn restyle(&mut self) {
        if let Some(h) = self.main_hwnd {
            self.glass = platform::style_window(h, self.settings.look.backdrop);
        }
        self.style_gen += 1;
    }

    fn screenshot_dir(&self) -> PathBuf {
        self.settings.screenshot_dir.clone().unwrap_or_else(|| platform::default_dir(Folder::Pictures))
    }

    fn video_dir(&self) -> PathBuf {
        self.settings.video_dir.clone().unwrap_or_else(|| platform::default_dir(Folder::Videos))
    }
}

// ───────────────────────────── Window / phase transitions ─────────────────────────────

impl SnippyApp {
    fn show_bar(&mut self, ctx: &egui::Context) {
        self.phase = Phase::Bar;
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Decorations(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(egui::WindowLevel::AlwaysOnTop));
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(bar::bar_size(&self.settings)));
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(!self.bar_hidden()));
    }

    /// Back to the editor if one is open, otherwise to the bar.
    fn show_home(&mut self, ctx: &egui::Context) {
        if self.editor.is_some() {
            self.phase = Phase::Editor;
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Decorations(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(egui::WindowLevel::Normal));
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        } else {
            self.show_bar(ctx);
        }
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
        let size = want.max(Vec2::new(760.0, 420.0)).min(monitor * 0.85);
        self.status = format!("{} × {} — Ctrl+C copies, Ctrl+S saves", img.width(), img.height());

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
        self.start_capture(ctx, Purpose::Snip);
    }

    fn start_capture(&mut self, ctx: &egui::Context, purpose: Purpose) {
        if self.capture_rx.is_some() || matches!(self.phase, Phase::Recording { .. } | Phase::Overlay(_)) {
            return;
        }
        self.flyout = None;
        self.tip = None;

        // Capture the monitor under the mouse pointer (that's the bar's monitor when the bar
        // was clicked; for shortcuts and the tray it's where the user is working).
        let probe = record::cursor_position().map(|(x, y)| Pos2::new(x as f32, y as f32)).or_else(|| {
            ctx.input(|i| {
                let ppp = i.viewport().native_pixels_per_point.unwrap_or(1.0);
                i.viewport().outer_rect.map(|r| r.center() * ppp)
            })
        });
        self.last_probe = probe;
        let mode = self.settings.mode;
        // For snips the delay happens before the screenshot; for recordings it's a
        // countdown after you've picked the area.
        let delay = if purpose == Purpose::Snip { self.settings.delay_secs } else { 0 };
        let ctx = ctx.clone();
        let (tx, rx) = mpsc::channel();
        self.capture_rx = Some(rx);
        self.phase = Phase::Capturing;
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));

        std::thread::spawn(move || {
            // Let the OS finish hiding us (and any fade animation), plus the user's delay.
            std::thread::sleep(Duration::from_millis(300) + Duration::from_secs(delay));
            let _ = tx.send(capture(purpose, mode, probe));
            // eframe calls `App::logic` while hidden when a repaint is requested.
            ctx.request_repaint();
        });
    }

    /// Copies, auto-saves to Pictures\Screenshots, and shows the flyout.
    fn finish_snip(&mut self, ctx: &egui::Context, img: RgbaImage) {
        let anchor = self.flyout_anchor(ctx);
        let copied = self.copy_to_clipboard(&img);
        let (path, save_err) = if self.settings.auto_save {
            let dir = self.screenshot_dir();
            let path = dir.join(format!("Snippy {}.png", timestamp()));
            match std::fs::create_dir_all(&dir).map_err(|e| e.to_string()).and_then(|()| save_image(&img, &path)) {
                Ok(p) => (Some(p), None),
                Err(e) => (None, Some(e)),
            }
        } else {
            (None, None)
        };
        self.flyout = Some(Flyout::snip(ctx, img, path, copied, save_err, anchor));
        self.show_home(ctx);
    }

    /// The work area (screen minus taskbar) of the monitor Snippy is on, in global points.
    fn flyout_anchor(&self, ctx: &egui::Context) -> Rect {
        let (outer, ppp, monitor) = ctx.input(|i| {
            (
                i.viewport().outer_rect,
                i.viewport().native_pixels_per_point.unwrap_or(1.0),
                i.viewport().monitor_size.unwrap_or(Vec2::new(1920.0, 1080.0)),
            )
        });
        let probe = self.last_probe.unwrap_or_else(|| outer.map_or(Pos2::ZERO, |r| r.center()) * ppp);
        match platform::work_area(probe.x as i32, probe.y as i32) {
            Some([l, t, r, b]) => Rect::from_min_max(Pos2::new(l as f32, t as f32) / ppp, Pos2::new(r as f32, b as f32) / ppp),
            None => Rect::from_min_size(Pos2::ZERO, monitor - Vec2::new(0.0, 48.0)),
        }
    }

    fn handle_flyout(&mut self, ctx: &egui::Context) {
        let Some(f) = self.flyout.as_mut() else { return };
        let Some(action) = f.show(ctx, &self.settings.look, self.glass, self.style_gen) else { return };
        match action {
            FlyoutAction::Close => self.flyout = None,
            FlyoutAction::Edit => {
                if let Some(Flyout { kind: FlyoutKind::Snip { image, .. }, .. }) = self.flyout.take() {
                    self.show_editor(ctx, image);
                }
            }
            FlyoutAction::Copy => {
                if let Some(FlyoutKind::Snip { image, .. }) = self.flyout.as_ref().map(|f| &f.kind) {
                    let image = image.clone();
                    let msg = match self.copy_to_clipboard(&image) {
                        Ok(()) => "Copied to clipboard".to_string(),
                        Err(e) => format!("Copy failed: {e}"),
                    };
                    if let Some(f) = self.flyout.as_mut() {
                        f.flash(msg);
                    }
                }
            }
            FlyoutAction::SaveAs => self.flyout_save_as(),
            FlyoutAction::OpenFolder => {
                if let Some(p) = f.path() {
                    if let Err(e) = reveal_in_folder(p) {
                        f.flash(format!("Couldn't open folder: {e}"));
                    }
                }
            }
            FlyoutAction::Play => {
                if let Some(p) = f.path() {
                    if let Err(e) = open_path(p) {
                        f.flash(format!("Couldn't open: {e}"));
                    }
                }
            }
            FlyoutAction::Delete => {
                if let Some(p) = f.path() {
                    let _ = std::fs::remove_file(p);
                }
                self.flyout = None;
            }
            FlyoutAction::Update => {
                if let FlyoutKind::Update(rel) = &f.kind {
                    let rel = rel.clone();
                    f.flash("Downloading…");
                    f.hold();
                    self.start_download(ctx, rel);
                }
            }
            FlyoutAction::ReleaseNotes => {
                let url = match &f.kind {
                    FlyoutKind::Update(rel) if !rel.page.is_empty() => rel.page.clone(),
                    _ => updater::releases_page(),
                };
                let _ = open_url(&url);
            }
        }
    }

    fn flyout_save_as(&mut self) {
        let Some(f) = self.flyout.as_mut() else { return };
        let name = f.path().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let result = match &f.kind {
            FlyoutKind::Snip { image, .. } => {
                let Some(dest) = rfd::FileDialog::new()
                    .add_filter("PNG image", &["png"])
                    .add_filter("JPEG image", &["jpg", "jpeg"])
                    .set_file_name(name)
                    .save_file()
                else {
                    return;
                };
                save_image(image, &dest)
            }
            FlyoutKind::Recording(info) => {
                let Some(dest) = rfd::FileDialog::new().add_filter("MP4 video", &["mp4"]).set_file_name(name).save_file() else {
                    return;
                };
                let dest = if dest.extension().is_none() { dest.with_extension("mp4") } else { dest };
                move_file(&info.path, &dest).map(|()| dest).map_err(|e| e.to_string())
            }
            FlyoutKind::Update(_) | FlyoutKind::Message => return,
        };
        match result {
            Ok(p) => f.set_path(p),
            Err(e) => f.flash(format!("Save failed: {e}")),
        }
    }

    fn copy_to_clipboard(&mut self, img: &RgbaImage) -> Result<(), String> {
        // Keep one clipboard handle alive for the app's lifetime (required on Linux/X11,
        // where the data disappears when the owner is dropped; harmless on Windows).
        if self.clipboard.is_none() {
            self.clipboard = Some(Clipboard::new().map_err(|e| e.to_string())?);
        }
        let data = ImageData { width: img.width() as usize, height: img.height() as usize, bytes: Cow::Borrowed(img.as_raw()) };
        self.clipboard.as_mut().unwrap().set_image(data).map_err(|e| e.to_string())
    }
}

// ───────────────────────────── Recording ─────────────────────────────

impl SnippyApp {
    /// `border`: the recorded area in global points, to outline it on screen (None = full screen).
    fn begin_recording(&mut self, ctx: &egui::Context, target: RecordTarget, border: Option<Rect>) {
        let opts = record::RecordOptions {
            fps: self.settings.fps.clamp(5, 60),
            countdown: Duration::from_secs(self.settings.delay_secs),
            system_audio: self.settings.system_audio,
            microphone: self.settings.microphone,
            cursor: self.settings.show_cursor,
        };
        let dir = self.video_dir();
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(format!("Recording {}.mp4", timestamp()));
        let recorder = Recorder::start(target, opts, path);
        self.phase = Phase::Recording { recorder, border };

        // The mini bar stays visible while recording; it's excluded from capture.
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Decorations(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(egui::WindowLevel::AlwaysOnTop));
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(bar::rec_bar_size()));
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
    }

    fn stop_recording(&mut self, ctx: &egui::Context) {
        let Phase::Recording { recorder, .. } = std::mem::replace(&mut self.phase, Phase::Bar) else { return };
        let anchor = self.flyout_anchor(ctx);
        self.flyout = Some(match recorder.stop() {
            Ok(info) => Flyout::recording(ctx, info, anchor),
            Err(e) => Flyout::message("Recording failed", e, anchor),
        });
        self.show_bar(ctx);
    }
}

// ───────────────────────────── eframe integration ─────────────────────────────

impl eframe::App for SnippyApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        egui::Rgba::TRANSPARENT.to_array()
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, eframe::APP_KEY, &self.settings);
    }

    /// Runs even while the window is hidden, so capture results are handled here
    /// on the UI thread — the worker thread never touches the window.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_updates(ctx);
        self.handle_events(ctx);
        // In tray-only mode the main window is hidden, but flyouts and the settings window
        // are drawn from its frame: keep it painting while they're up.
        if matches!(self.phase, Phase::Bar) && self.bar_hidden() && (self.flyout.is_some() || self.settings_open) {
            ctx.send_viewport_cmd(egui::ViewportCommand::RequestPaintWhileHidden);
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        let Some(rx) = &self.capture_rx else { return };
        let result = match rx.try_recv() {
            Ok(r) => r,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => Err("capture thread crashed".into()),
        };
        self.capture_rx = None;
        match result {
            Ok(cap) if cap.mode == CaptureMode::FullScreen && cap.purpose == Purpose::Snip => self.finish_snip(ctx, cap.image),
            Ok(cap) if cap.mode == CaptureMode::FullScreen => {
                let target = RecordTarget { monitor_id: cap.monitor_id, region: None };
                self.begin_recording(ctx, target, None);
            }
            Ok(cap) => self.show_overlay(ctx, cap),
            Err(e) => {
                let anchor = self.flyout_anchor(ctx);
                self.flyout = Some(Flyout::message("Capture failed", e, anchor));
                self.show_home(ctx);
            }
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        match &mut self.phase {
            Phase::Overlay(overlay) => match overlay_ui(ui, overlay) {
                Some(OverlayOutcome::Selected { px, global }) => match overlay.purpose {
                    Purpose::Snip => match crop_px(&overlay.image, px) {
                        Some(img) => self.finish_snip(&ctx, img),
                        None => overlay.drag = None,
                    },
                    Purpose::Record => {
                        let r = [px.min.x, px.min.y, px.width(), px.height()].map(|v| v.round().max(0.0) as u32);
                        let target = RecordTarget { monitor_id: overlay.monitor_id, region: Some(r) };
                        self.begin_recording(&ctx, target, Some(global));
                    }
                },
                Some(OverlayOutcome::Cancel) => self.show_home(&ctx),
                None => {}
            },
            Phase::Bar | Phase::Capturing => self.bar_ui(ui),
            Phase::Editor => self.editor_ui(ui),
            Phase::Recording { .. } => self.recording_ui(ui),
        }
        if !matches!(self.phase, Phase::Capturing | Phase::Overlay(_)) {
            self.handle_flyout(&ctx);
            if self.settings_open {
                self.settings_window(&ctx);
            } else {
                self.settings_styled = false;
            }
        }
    }
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size(bar::bar_size(&Settings::default()))
            .with_title("Snippy")
            .with_app_id("snippy")
            .with_icon(app_icon())
            .with_decorations(false)
            .with_always_on_top()
            .with_transparent(true),
        persist_window: false,
        ..Default::default()
    };
    eframe::run_native(
        "Snippy",
        options,
        Box::new(|cc| {
            let app = SnippyApp::new(cc);
            // The window was created at the default size; fit it to the saved settings.
            cc.egui_ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(bar::bar_size(&app.settings)));
            if app.bar_hidden() {
                cc.egui_ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            }
            Ok(Box::new(app))
        }),
    )
}

fn app_icon() -> egui::IconData {
    match tray::icon_rgba(ICON_PNG) {
        Some((rgba, width, height)) => egui::IconData { rgba, width, height },
        None => egui::IconData::default(),
    }
}
