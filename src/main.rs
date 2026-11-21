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
mod diag;
mod editor;
mod flyout;
mod gifsink;
mod history;
mod hotkeys;
mod icons;
#[cfg(windows)]
mod mf;
mod ocr;
mod overlay;
mod pins;
mod platform;
mod prefs;
mod record;
mod redact;
mod scroll;
mod text;
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
    /// Copy text from a region (OCR).
    Ocr,
    /// Pick a colour from the screen.
    ColorPick,
    /// Scrolling capture of a long page.
    Scroll,
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

/// What Print Screen / Win+Shift+S do.
#[derive(PartialEq, Eq, Clone, Copy, Debug, Serialize, Deserialize)]
enum ShortcutAction {
    /// Bring up the floating bar so you can pick what to capture.
    ShowBar,
    /// Start a snip right away.
    Snip,
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
    redact_style: redact::RedactStyle,
    /// Record animated GIFs instead of MP4 videos.
    record_gif: bool,
    /// Text size / step-marker size (image pixels).
    text_size: f32,
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
    shortcut_action: ShortcutAction,
    /// The "Snippy is in the tray" tip has been shown once.
    tray_hint_shown: bool,
    /// Close a Windows Snipping Tool that pops up right after a shortcut Snippy handled.
    close_snipping_tool: bool,
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
            redact_style: redact::RedactStyle::default(),
            record_gif: false,
            text_size: 32.0,
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
            shortcut_action: ShortcutAction::ShowBar,
            tray_hint_shown: false,
            close_snipping_tool: true,
        }
    }
}

enum Phase {
    Bar,
    Capturing,
    Overlay(Overlay),
    Editor,
    /// `border`: recorded area in virtual-desktop physical pixels (None = whole monitor).
    Recording { recorder: Recorder, border: Option<platform::PxRect> },
    /// Scrolling capture in progress; the main window is a small progress panel.
    Scrolling {
        rx: Receiver<Result<(RgbaImage, Vec<String>), String>>,
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
        progress: std::sync::Arc<std::sync::atomic::AtomicU32>,
        border: platform::PxRect,
        panel: platform::PxRect,
    },
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
    pins: Vec<pins::Pin>,
    next_pin_id: u64,
    history: Vec<history::HistoryItem>,
    history_open: bool,
    history_ui: history::HistoryUi,
    /// Text recognition running in the background.
    ocr_rx: Option<Receiver<Result<String, String>>>,
    settings_open: bool,
    settings_styled: bool,
    update: UpdateState,
    /// Cached "start with Windows" state (reading the registry every frame is wasteful).
    startup: Option<bool>,
    /// Cached: does Windows open its Snipping Tool on Print Screen?
    prtsc_windows: Option<bool>,
    prefs_status: String,
    tray: Option<tray::Tray>,
    /// Where the bar was before a capture took over the main window: (physical pixels,
    /// points). Restored afterwards.
    saved_pos: Option<(Option<platform::PxRect>, Option<Pos2>)>,
    /// The glass backdrop is switched off while the overlay covers the desktop.
    overlay_unstyled: bool,
    /// Tray-only mode, but the bar was brought up by a shortcut (until the next capture).
    bar_revealed: bool,
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
        text::register(&cc.egui_ctx);
        hotkeys::configure(settings.hotkey_print_screen, settings.hotkey_win_shift_s, settings.close_snipping_tool);
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
            pins: Vec::new(),
            next_pin_id: 0,
            history: history::load(),
            history_open: false,
            history_ui: history::HistoryUi::new(),
            ocr_rx: None,
            settings_open: false,
            settings_styled: false,
            update: UpdateState::Idle,
            startup: None,
            prtsc_windows: None,
            prefs_status: String::new(),
            tray: None,
            last_probe: None,
            bar_revealed: false,
            saved_pos: None,
            overlay_unstyled: false,
            events_tx,
            events_rx,
        };
        app.sync_tray(&cc.egui_ctx);
        diag::log(&format!(
            "--- Snippy {} started. shortcuts: PrintScreen={} Win+Shift+S={} action={:?} close_snipping_tool={}; tray={} bar_hidden={}; \
             Windows PrintScreen->SnippingTool={}; glass={:?}",
            env!("CARGO_PKG_VERSION"),
            app.settings.hotkey_print_screen,
            app.settings.hotkey_win_shift_s,
            app.settings.shortcut_action,
            app.settings.close_snipping_tool,
            app.tray.is_some(),
            app.bar_hidden(),
            platform::prtsc_opens_snipping_tool(),
            app.settings.look.backdrop,
        ));
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

    /// The bar's "hide to tray" button: hides the bar until the tray icon is clicked (or a
    /// shortcut brings it up). Turns the tray icon on if needed, so Snippy stays reachable.
    pub(crate) fn hide_to_tray(&mut self, ctx: &egui::Context) {
        diag::log("app: hide to tray");
        if !self.settings.tray_icon {
            self.settings.tray_icon = true;
            self.sync_tray(ctx);
        }
        if self.tray.is_none() {
            // No tray on this system: hiding would make Snippy unreachable, so exit instead.
            quit(ctx);
            return;
        }
        self.tip = None;
        self.bar_revealed = false;
        self.settings.hide_bar = true;
        self.show_bar(ctx);
        if !self.settings.tray_hint_shown {
            self.settings.tray_hint_shown = true;
            let anchor = self.flyout_anchor(ctx);
            self.flyout = Some(Flyout::message(
                "Snippy is in the system tray",
                "Click its icon to bring the bar back (look under the ^ arrow).",
                anchor,
            ));
        }
    }

    /// Bar hidden in tray-only mode.
    fn bar_hidden(&self) -> bool {
        self.settings.hide_bar && self.tray.is_some() && !self.bar_revealed
    }

    /// If the bar is on a different monitor than the mouse pointer, moves it next to the
    /// pointer (clamped to that monitor's work area). Positions are physical pixels, so this
    /// works across monitors with different display scaling.
    fn bring_bar_to_pointer(&self) {
        let (Some(h), Some((cx, cy))) = (self.main_hwnd, record::cursor_position()) else { return };
        let Some(target) = platform::work_area(cx, cy) else { return };
        let bar = platform::window_rect(h);
        let bar_area = bar.and_then(|[l, t, r, b]| platform::work_area((l + r) / 2, (t + b) / 2));
        if bar_area == Some(target) && !self.bar_hidden_before_reveal() {
            return; // already on this monitor
        }
        let scale = Monitor::from_point(cx, cy).ok().and_then(|m| m.scale_factor().ok()).unwrap_or(1.0);
        let size = bar::bar_size(&self.settings) * scale;
        let (w, hgt) = (size.x.round() as i32, size.y.round() as i32);
        let gap = (24.0 * scale) as i32;
        let [l, t, r, b] = target;
        let margin = (8.0 * scale) as i32;
        let mut x = cx + gap;
        if x + w > r - margin {
            x = cx - gap - w; // not enough room on the right: put it on the left of the pointer
        }
        let y = cy - hgt / 2;
        let x = x.clamp(l + margin, (r - margin - w).max(l + margin));
        let y = y.clamp(t + margin, (b - margin - hgt).max(t + margin));
        platform::move_window(h, x, y);
    }

    /// In tray-only mode the bar was hidden until this reveal; always bring it to the pointer then.
    fn bar_hidden_before_reveal(&self) -> bool {
        self.settings.hide_bar && self.tray.is_some()
    }

    /// Logs a tray command and passes it through.
    fn log_tray(&self, cmd: tray::TrayCmd) -> tray::TrayCmd {
        diag::log(&format!("app: tray {cmd:?} while '{}'", self.phase_name()));
        cmd
    }

    fn phase_name(&self) -> &'static str {
        match self.phase {
            Phase::Bar if self.bar_hidden() => "hidden in tray",
            Phase::Bar => "bar",
            Phase::Capturing => "capturing",
            Phase::Overlay(_) => "selecting",
            Phase::Editor => "editor",
            Phase::Recording { .. } => "recording",
            Phase::Scrolling { .. } => "scrolling",
        }
    }

    /// Shows the bar (even in tray-only mode) and brings it to the front.
    fn reveal_bar(&mut self, ctx: &egui::Context) {
        diag::log(&format!("app: show bar requested while '{}'", self.phase_name()));
        match self.phase {
            Phase::Bar => {
                self.bar_revealed = true;
                self.bring_bar_to_pointer();
                self.show_bar(ctx);
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            }
            // The editor is the same window; just bring it forward.
            Phase::Editor => ctx.send_viewport_cmd(egui::ViewportCommand::Focus),
            // Busy capturing / selecting / recording: ignore.
            _ => diag::log("app: ignored (busy)"),
        }
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
                AppEvent::Hotkey(h) if self.settings.shortcut_action == ShortcutAction::ShowBar => {
                    diag::log(&format!("app: received {h:?}"));
                    self.reveal_bar(ctx)
                }
                // "Start a snip": Win+Shift+S snips a region (like Windows); Print Screen uses
                // the mode selected on the bar.
                AppEvent::Hotkey(hotkeys::Hotkey::WinShiftS) => snip(self, Some(CaptureMode::Region)),
                AppEvent::Hotkey(hotkeys::Hotkey::PrintScreen) => snip(self, None),
                AppEvent::Tray(cmd) => match self.log_tray(cmd) {
                    tray::TrayCmd::SnipRegion => snip(self, Some(CaptureMode::Region)),
                    tray::TrayCmd::SnipWindow => snip(self, Some(CaptureMode::Window)),
                    tray::TrayCmd::SnipScreen => snip(self, Some(CaptureMode::FullScreen)),
                    tray::TrayCmd::Record => self.start_capture(ctx, Purpose::Record),
                    tray::TrayCmd::ScrollCapture => self.start_capture(ctx, Purpose::Scroll),
                    tray::TrayCmd::CopyText => self.start_capture(ctx, Purpose::Ocr),
                    tray::TrayCmd::PickColor => self.start_capture(ctx, Purpose::ColorPick),
                    tray::TrayCmd::History => self.history_open = true,
                    tray::TrayCmd::ToggleBar => {
                        if self.bar_revealed {
                            self.bar_revealed = false; // shown by a shortcut: tuck it away again
                        } else {
                            self.settings.hide_bar = !self.settings.hide_bar;
                        }
                        if matches!(self.phase, Phase::Bar) {
                            self.show_bar(ctx);
                            if !self.bar_hidden() {
                                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                            }
                        }
                    }
                    tray::TrayCmd::Settings => self.settings_open = true,
                    tray::TrayCmd::Exit => quit(ctx),
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
        self.restore_main(ctx);
        if self.bar_hidden() {
            ghost_main(ctx);
            return;
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Decorations(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(egui::WindowLevel::AlwaysOnTop));
        ctx.send_viewport_cmd(egui::ViewportCommand::MousePassthrough(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(bar::bar_size(&self.settings)));
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
    }

    /// Back to the editor if one is open, otherwise to the bar.
    fn show_home(&mut self, ctx: &egui::Context) {
        self.restore_main(ctx);
        if let Some(ed) = &self.editor {
            self.phase = Phase::Editor;
            let (ppp, monitor) = ctx.input(|i| {
                (
                    i.viewport().native_pixels_per_point.unwrap_or(1.0),
                    i.viewport().monitor_size.unwrap_or(Vec2::new(1920.0, 1080.0)),
                )
            });
            let size = editor_size(Vec2::new(ed.image.width() as f32, ed.image.height() as f32), ppp, monitor);
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Decorations(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(egui::WindowLevel::Normal));
            ctx.send_viewport_cmd(egui::ViewportCommand::MousePassthrough(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        } else {
            self.show_bar(ctx);
        }
    }

    /// Shows the selection overlay: the main window is stretched over every monitor.
    fn show_overlay(&mut self, ctx: &egui::Context, cap: Capture) {
        let ov = Overlay::new(ctx, cap);
        let desktop = ov.desktop();
        self.phase = Phase::Overlay(ov);
        if let Some(h) = self.main_hwnd {
            // No glass behind the overlay (it would flash before the first frame).
            platform::style_window(h, Backdrop::Off);
            self.overlay_unstyled = true;
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Decorations(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(egui::WindowLevel::AlwaysOnTop));
        ctx.send_viewport_cmd(egui::ViewportCommand::MousePassthrough(false));
        self.cover_desktop(ctx, desktop);
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    }

    /// Places the main window over `desktop` (physical pixels). On Windows this is done in
    /// physical pixels and re-checked every frame (Windows resizes a window when its DPI
    /// changes); elsewhere through egui in points.
    fn cover_desktop(&self, ctx: &egui::Context, desktop: Rect) {
        let r = [desktop.min.x, desktop.min.y, desktop.max.x, desktop.max.y].map(|v| v.round() as i32);
        if self.main_hwnd.is_some_and(|h| platform::place_hwnd(h, r)) {
            return;
        }
        let ppp = ctx.input(|i| i.viewport().native_pixels_per_point.unwrap_or(1.0));
        ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(desktop.min / ppp));
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(desktop.size() / ppp));
    }

    /// After a capture: put the window back where the bar was and restore the glass.
    fn restore_main(&mut self, ctx: &egui::Context) {
        if let Some((phys, points)) = self.saved_pos.take() {
            match (self.main_hwnd, phys) {
                // Position *and* size in one step, so the desktop-sized window never
                // flashes at the bar's spot.
                (Some(h), Some(r)) => {
                    platform::place_hwnd(h, r);
                }
                _ => {
                    if let Some(p) = points {
                        ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(p));
                    }
                }
            }
        }
        if self.overlay_unstyled {
            self.overlay_unstyled = false;
            if let Some(h) = self.main_hwnd {
                self.glass = platform::style_window(h, self.settings.look.backdrop);
            }
        }
    }

    /// Acts on a choice made in the overlay.
    fn overlay_selected(&mut self, ctx: &egui::Context, screen: usize, px: Rect) {
        diag::log(&format!("app: selected {}x{} on screen {screen}", px.width().round(), px.height().round()));
        let Phase::Overlay(ov) = &mut self.phase else { return };
        let s = &ov.screens[screen].screen;
        self.last_probe = Some(s.bounds().center());
        let full = Rect::from_min_size(Pos2::ZERO, Vec2::new(s.image.width() as f32, s.image.height() as f32));
        match ov.purpose {
            Purpose::Snip => match crop_px(&s.image, px) {
                Some(img) => self.finish_snip(ctx, img),
                None => ov.drag = None,
            },
            Purpose::Ocr => match crop_px(&s.image, px) {
                Some(img) => self.start_ocr(ctx, img),
                None => ov.drag = None,
            },
            Purpose::ColorPick => {}
            Purpose::Scroll => {
                let [x, y] = [px.min.x, px.min.y].map(|v| v.round().max(0.0) as u32);
                let [w, h] = [px.width(), px.height()].map(|v| v.round().max(1.0) as u32);
                let (origin, monitor_id, scale, bounds) = (s.origin, s.monitor_id, s.scale, s.bounds());
                self.begin_scroll(ctx, monitor_id, [x, y, w, h], origin, scale, bounds);
            }
            Purpose::Record => {
                let monitor_id = s.monitor_id;
                let whole = px.min.distance(full.min) < 1.0 && px.max.distance(full.max) < 1.0;
                let (region, border) = if whole {
                    (None, None)
                } else {
                    let o = s.origin;
                    let b = [o.x + px.min.x, o.y + px.min.y, o.x + px.max.x, o.y + px.max.y].map(|v| v.round() as i32);
                    (Some([b[0], b[1], b[2] - b[0], b[3] - b[1]]), Some(b))
                };
                self.begin_recording(ctx, RecordTarget { monitor_id, region }, border);
            }
        }
    }

    fn show_editor(&mut self, ctx: &egui::Context, img: RgbaImage) {
        // Size the editor to the snip (at native pixel size), within sensible bounds.
        let (ppp, monitor) = ctx.input(|i| {
            (
                i.viewport().native_pixels_per_point.unwrap_or(1.0),
                i.viewport().monitor_size.unwrap_or(Vec2::new(1920.0, 1080.0)),
            )
        });
        let size = editor_size(Vec2::new(img.width() as f32, img.height() as f32), ppp, monitor);
        self.status = format!("{} × {} — Ctrl+C copies, Ctrl+S saves", img.width(), img.height());

        self.editor = Some(Editor::new(ctx, img));
        self.phase = Phase::Editor;

        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Decorations(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(egui::WindowLevel::Normal));
        ctx.send_viewport_cmd(egui::ViewportCommand::MousePassthrough(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        self.center_on_capture_monitor(size);
    }

    /// Centres a main window of `size` points on the monitor of the last capture.
    fn center_on_capture_monitor(&self, size: Vec2) {
        let (Some(h), Some(p)) = (self.main_hwnd, self.last_probe) else { return };
        let Some([l, t, r, b]) = platform::work_area(p.x as i32, p.y as i32) else { return };
        let scale = Monitor::from_point(p.x as i32, p.y as i32).ok().and_then(|m| m.scale_factor().ok()).unwrap_or(1.0);
        let (w, hgt) = ((size.x * scale) as i32, (size.y * scale) as i32);
        platform::move_window(h, l + ((r - l - w) / 2).max(0), t + ((b - t - hgt) / 2).max(0));
    }

    fn start_snip(&mut self, ctx: &egui::Context) {
        self.start_capture(ctx, Purpose::Snip);
    }

    fn start_capture(&mut self, ctx: &egui::Context, purpose: Purpose) {
        if self.capture_rx.is_some() || matches!(self.phase, Phase::Recording { .. } | Phase::Overlay(_) | Phase::Scrolling { .. }) {
            diag::log(&format!("app: capture refused (busy: '{}')", self.phase_name()));
            return;
        }
        diag::log(&format!("app: capture started ({purpose:?}, {:?})", self.settings.mode));
        self.flyout = None;
        self.tip = None;
        self.bar_revealed = false; // tray-only mode: the bar goes away again after this capture

        // Capture the monitor under the mouse pointer (that's the bar's monitor when the bar
        // was clicked; for shortcuts and the tray it's where the user is working).
        let probe = record::cursor_position().map(|(x, y)| Pos2::new(x as f32, y as f32)).or_else(|| {
            ctx.input(|i| {
                let ppp = i.viewport().native_pixels_per_point.unwrap_or(1.0);
                i.viewport().outer_rect.map(|r| r.center() * ppp)
            })
        });
        self.last_probe = probe;
        if self.saved_pos.is_none() {
            let phys = self.main_hwnd.and_then(platform::window_rect);
            let points = ctx.input(|i| i.viewport().outer_rect.map(|r| r.min));
            self.saved_pos = Some((phys, points));
        }
        let mode = match purpose {
            Purpose::Snip | Purpose::Record => self.settings.mode,
            // Text and colour always pick from a frozen view of the screen.
            Purpose::Ocr | Purpose::ColorPick | Purpose::Scroll => CaptureMode::Region,
        };
        // For snips the delay happens before the screenshot; for recordings it's a
        // countdown after you've picked the area.
        let delay = if purpose == Purpose::Snip { self.settings.delay_secs } else { 0 };
        let ctx = ctx.clone();
        let (tx, rx) = mpsc::channel();
        self.capture_rx = Some(rx);
        self.phase = Phase::Capturing;
        // Shrink to an invisible ghost (never truly hide: see `ghost_main`). On Windows the
        // window is excluded from capture anyway; elsewhere it's a single transparent pixel.
        ghost_main(&ctx);

        std::thread::spawn(move || {
            // Let the OS finish hiding us (and any fade animation), plus the user's delay.
            // On Windows Snippy's windows are excluded from capture, so only a short pause is
            // needed; elsewhere give the bar time to disappear.
            let settle = if cfg!(windows) { 120 } else { 300 };
            std::thread::sleep(Duration::from_millis(settle) + Duration::from_secs(delay));
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
        diag::log(&format!("app: snip done (copied={}, saved={}) -> flyout", copied.is_ok(), path.is_some()));
        self.history_add_snip(&img, path.clone());
        self.flyout = Some(Flyout::snip(ctx, img, path, copied, save_err, anchor));
        self.show_home(ctx);
    }

    /// Where flyouts go: the work area (screen minus taskbar) of the monitor of the last
    /// capture (or of the bar), in physical pixels, with that monitor's scale.
    fn flyout_anchor(&self, ctx: &egui::Context) -> flyout::Anchor {
        let (outer, ppp, monitor) = ctx.input(|i| {
            (
                i.viewport().outer_rect,
                i.viewport().native_pixels_per_point.unwrap_or(1.0),
                i.viewport().monitor_size.unwrap_or(Vec2::new(1920.0, 1080.0)),
            )
        });
        let probe = self.last_probe.unwrap_or_else(|| outer.map_or(Pos2::ZERO, |r| r.center()) * ppp);
        match platform::work_area(probe.x as i32, probe.y as i32) {
            Some([l, t, r, b]) => {
                let scale = Monitor::from_point(probe.x as i32, probe.y as i32)
                    .ok()
                    .and_then(|m| m.scale_factor().ok())
                    .unwrap_or(ppp);
                flyout::Anchor { work: Rect::from_min_max(Pos2::new(l as f32, t as f32), Pos2::new(r as f32, b as f32)), scale }
            }
            // No OS work-area info: assume the monitor at the origin, minus a taskbar.
            None => flyout::Anchor { work: Rect::from_min_size(Pos2::ZERO, (monitor - Vec2::new(0.0, 48.0)) * ppp), scale: ppp },
        }
    }

    fn handle_flyout(&mut self, ctx: &egui::Context) {
        let Some(f) = self.flyout.as_mut() else { return };
        let Some(action) = f.show(ctx, &self.settings.look, self.glass, self.style_gen) else { return };
        if action != FlyoutAction::Close {
            diag::log(&format!("app: flyout {action:?}"));
        }
        match action {
            FlyoutAction::Close => {
                diag::log("app: flyout closed");
                self.flyout = None
            }
            FlyoutAction::Edit => {
                if let Some(Flyout { kind: FlyoutKind::Snip { image, .. }, .. }) = self.flyout.take() {
                    self.show_editor(ctx, image);
                }
            }
            FlyoutAction::Copy if matches!(f.kind, FlyoutKind::Recording(_)) => {
                if let Some(p) = f.path().map(|p| p.to_path_buf()) {
                    let res = match self.clipboard.as_mut() {
                        Some(c) => c.set().file_list(&[&p]).map_err(|e| e.to_string()),
                        None => Err("clipboard unavailable".to_string()),
                    };
                    f.flash(match res {
                        Ok(()) => "Copied: paste it into a chat, email or folder".to_string(),
                        Err(e) => format!("Copy failed: {e}"),
                    });
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
            FlyoutAction::Pin => {
                if let Some(FlyoutKind::Snip { image, .. }) = self.flyout.as_ref().map(|f| &f.kind) {
                    let image = image.clone();
                    self.add_pin(ctx, image);
                    self.flyout = None;
                }
            }
            FlyoutAction::CopyText => {
                if let FlyoutKind::Text(t) = &f.kind {
                    let t = t.clone();
                    let msg = self.copy_text(&t).map_or_else(|e| format!("Copy failed: {e}"), |()| "Copied again".into());
                    if let Some(f) = self.flyout.as_mut() {
                        f.flash(msg);
                    }
                }
            }
            FlyoutAction::CopyHex | FlyoutAction::CopyRgb => {
                if let FlyoutKind::Color([r, g, b]) = f.kind {
                    let text = if action == FlyoutAction::CopyHex { format!("#{r:02X}{g:02X}{b:02X}") } else { format!("rgb({r}, {g}, {b})") };
                    let msg = self.copy_text(&text).map_or_else(|e| format!("Copy failed: {e}"), |()| format!("Copied {text}"));
                    if let Some(f) = self.flyout.as_mut() {
                        f.flash(msg);
                    }
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
            FlyoutKind::Update(_) | FlyoutKind::Text(_) | FlyoutKind::Color(_) | FlyoutKind::Message => return,
        };
        match result {
            Ok(p) => f.set_path(p),
            Err(e) => f.flash(format!("Save failed: {e}")),
        }
    }

    /// Starts a scrolling capture of `region` (monitor pixels). The main window becomes a
    /// small progress panel beside the area, out of the way of the pointer.
    fn begin_scroll(&mut self, ctx: &egui::Context, monitor_id: Option<u32>, region: [u32; 4], origin: Pos2, scale: f32, monitor: Rect) {
        use std::sync::atomic::{AtomicBool, AtomicU32};
        use std::sync::Arc;
        let [x, y, w, h] = region;
        let (ox, oy) = (origin.x.round() as i32, origin.y.round() as i32);
        let border = [ox + x as i32, oy + y as i32, ox + (x + w) as i32, oy + (y + h) as i32];
        diag::log(&format!("app: scrolling capture of a {w}x{h} area"));
        let stop = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(AtomicU32::new(h));
        let (tx, rx) = mpsc::channel();
        let (st, pr, c) = (stop.clone(), progress.clone(), ctx.clone());
        let target = scroll::ScrollTarget { monitor_id, region, origin: (ox, oy) };
        std::thread::spawn(move || {
            let _ = tx.send(scroll::run(target, st, pr));
            c.request_repaint();
        });

        // Progress panel: above the area if there's room, else below, else inside the top
        // edge (it's excluded from capture on Windows, and the pointer sits mid-area).
        let (pw, ph) = ((300.0 * scale) as i32, (46.0 * scale) as i32);
        let gap = (10.0 * scale) as i32;
        let (mon_top, mon_bottom) = (monitor.min.y as i32, monitor.max.y as i32);
        let px = ((border[0] + border[2]) / 2 - pw / 2).clamp(monitor.min.x as i32, (monitor.max.x as i32 - pw).max(monitor.min.x as i32));
        let py = if border[1] - gap - ph >= mon_top {
            border[1] - gap - ph // above the area
        } else if border[3] + gap + ph <= mon_bottom {
            border[3] + gap // below
        } else {
            border[1] + gap // inside the top edge
        };
        let panel = [px, py, px + pw, py + ph];
        self.phase = Phase::Scrolling { rx, stop, progress, border, panel };
        // Restore the glass that the overlay switched off, but keep the saved bar position
        // for when the capture finishes.
        if self.overlay_unstyled {
            self.overlay_unstyled = false;
            if let Some(hw) = self.main_hwnd {
                self.glass = platform::style_window(hw, self.settings.look.backdrop);
            }
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Decorations(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(egui::WindowLevel::AlwaysOnTop));
        ctx.send_viewport_cmd(egui::ViewportCommand::MousePassthrough(false));
        if !self.main_hwnd.is_some_and(|hw| platform::place_hwnd(hw, panel)) {
            ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(Pos2::new(panel[0] as f32, panel[1] as f32) / scale));
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(Vec2::new(300.0, 46.0)));
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
    }

    fn poll_scroll(&mut self, ctx: &egui::Context) {
        let Phase::Scrolling { rx, .. } = &self.phase else { return };
        let res = match rx.try_recv() {
            Ok(r) => r,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => Err("scrolling capture crashed".into()),
        };
        match res {
            Ok((img, notes)) => {
                diag::log(&format!("app: scrolling capture done: {}x{}", img.width(), img.height()));
                self.finish_snip(ctx, img);
                if let Some(f) = self.flyout.as_mut() {
                    f.notes.extend(notes);
                }
            }
            Err(e) => {
                diag::log(&format!("app: scrolling capture failed: {e}"));
                let anchor = self.flyout_anchor(ctx);
                self.flyout = Some(Flyout::message("Scrolling capture failed", e, anchor));
                self.show_home(ctx);
            }
        }
    }

    /// Recognises text in `img` on a background thread (see `poll_ocr`).
    fn start_ocr(&mut self, ctx: &egui::Context, img: RgbaImage) {
        diag::log(&format!("app: reading text from a {}x{} area", img.width(), img.height()));
        let (tx, rx) = mpsc::channel();
        let c = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(ocr::recognize(&img));
            c.request_repaint();
        });
        self.ocr_rx = Some(rx);
        let anchor = self.flyout_anchor(ctx);
        let mut f = Flyout::message("Reading text…", "", anchor);
        f.hold();
        self.flyout = Some(f);
        self.show_home(ctx);
    }

    fn poll_ocr(&mut self, ctx: &egui::Context) {
        let Some(rx) = &self.ocr_rx else { return };
        let res = match rx.try_recv() {
            Ok(r) => r,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => Err("text recognition crashed".into()),
        };
        self.ocr_rx = None;
        let anchor = self.flyout_anchor(ctx);
        self.flyout = Some(match res {
            Ok(text) => {
                let text = text.trim().to_string();
                diag::log(&format!("app: text recognised ({} chars)", text.chars().count()));
                if !text.is_empty() {
                    if let Err(e) = self.copy_text(&text) {
                        diag::log(&format!("app: copying text failed: {e}"));
                    }
                }
                Flyout::text(text, anchor)
            }
            Err(e) => {
                diag::log(&format!("app: text recognition failed: {e}"));
                Flyout::message("Couldn't read text", e, anchor)
            }
        });
    }

    fn color_picked(&mut self, ctx: &egui::Context, screen: usize, rgb: [u8; 3]) {
        if let Phase::Overlay(ov) = &self.phase {
            self.last_probe = ov.screens.get(screen).map(|s| s.screen.bounds().center());
        }
        let [r, g, b] = rgb;
        let hex = format!("#{r:02X}{g:02X}{b:02X}");
        diag::log(&format!("app: picked colour {hex}"));
        let _ = self.copy_text(&hex);
        let anchor = self.flyout_anchor(ctx);
        self.flyout = Some(Flyout::color(rgb, anchor));
        self.show_home(ctx);
    }

    fn copy_text(&mut self, text: &str) -> Result<(), String> {
        if self.clipboard.is_none() {
            self.clipboard = Some(Clipboard::new().map_err(|e| e.to_string())?);
        }
        self.clipboard.as_mut().unwrap().set_text(text.to_owned()).map_err(|e| e.to_string())
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
    fn begin_recording(&mut self, ctx: &egui::Context, target: RecordTarget, border: Option<platform::PxRect>) {
        let gif = self.settings.record_gif;
        let opts = record::RecordOptions {
            fps: self.settings.fps.clamp(5, 60),
            countdown: Duration::from_secs(self.settings.delay_secs),
            // GIFs have no sound.
            system_audio: self.settings.system_audio && !gif,
            microphone: self.settings.microphone && !gif,
            cursor: self.settings.show_cursor,
            gif,
        };
        let dir = self.video_dir();
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(format!("Recording {}.{}", timestamp(), if gif { "gif" } else { "mp4" }));
        diag::log(&format!("app: recording started ({})", if gif { "GIF" } else { "MP4" }));
        let recorder = Recorder::start(target, opts, path);
        self.phase = Phase::Recording { recorder, border };
        self.restore_main(ctx);

        // The mini bar stays visible while recording; it's excluded from capture.
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Decorations(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(egui::WindowLevel::AlwaysOnTop));
        ctx.send_viewport_cmd(egui::ViewportCommand::MousePassthrough(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(bar::rec_bar_size()));
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
    }

    fn stop_recording(&mut self, ctx: &egui::Context) {
        let Phase::Recording { recorder, .. } = std::mem::replace(&mut self.phase, Phase::Bar) else { return };
        diag::log("app: recording stopped");
        let anchor = self.flyout_anchor(ctx);
        self.flyout = Some(match recorder.stop() {
            Ok(info) => {
                self.history_add_recording(&info);
                Flyout::recording(ctx, info, anchor)
            }
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

    fn on_exit(&mut self) {
        diag::log("--- Snippy exited normally");
        std::thread::sleep(std::time::Duration::from_millis(50)); // let the log writer flush
    }

    /// Runs even while the window is hidden, so capture results are handled here
    /// on the UI thread — the worker thread never touches the window.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // The editor's close button closes the editor, not Snippy.
        if ctx.input(|i| i.viewport().close_requested()) && !QUITTING.load(std::sync::atomic::Ordering::Relaxed) {
            if matches!(self.phase, Phase::Editor) {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                diag::log("app: editor closed");
                self.editor = None;
                self.show_bar(ctx);
            } else {
                QUITTING.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
        self.poll_updates(ctx);
        self.poll_ocr(ctx);
        self.poll_scroll(ctx);
        self.handle_events(ctx);
        let Some(rx) = &self.capture_rx else { return };
        let result = match rx.try_recv() {
            Ok(r) => r,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => Err("capture thread crashed".into()),
        };
        self.capture_rx = None;
        match &result {
            Ok(cap) => diag::log(&format!("app: captured {} screen(s)", cap.screens.len())),
            Err(e) => diag::log(&format!("app: capture failed: {e}")),
        }
        match result {
            // Full screen with a single monitor: no need to ask which one.
            Ok(mut cap) if cap.mode == CaptureMode::FullScreen && cap.screens.len() == 1 && matches!(cap.purpose, Purpose::Snip | Purpose::Record) => {
                let s = cap.screens.remove(0);
                self.last_probe = Some(s.bounds().center());
                if cap.purpose == Purpose::Snip {
                    self.finish_snip(ctx, s.image);
                } else {
                    self.begin_recording(ctx, RecordTarget { monitor_id: s.monitor_id, region: None }, None);
                }
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
        let ghost = self.bar_hidden();
        match &mut self.phase {
            Phase::Overlay(overlay) => {
                // Keep covering the desktop (corrects Windows' DPI-change resize).
                let desktop = overlay.desktop();
                let r = [desktop.min.x, desktop.min.y, desktop.max.x, desktop.max.y].map(|v| v.round() as i32);
                if let Some(h) = self.main_hwnd {
                    platform::place_hwnd(h, r);
                }
                let outcome = overlay.ui(ui);
                match outcome {
                    Some(OverlayOutcome::Selected { screen, px }) => self.overlay_selected(&ctx, screen, px),
                    Some(OverlayOutcome::Color { screen, rgb }) => self.color_picked(&ctx, screen, rgb),
                    Some(OverlayOutcome::Cancel) => {
                        diag::log("app: selection cancelled");
                        self.show_home(&ctx)
                    }
                    None => {}
                }
            }
            // Tray-only mode: the main window is a ghost; draw nothing in it.
            Phase::Bar if ghost => {}
            Phase::Bar | Phase::Capturing => self.bar_ui(ui),
            Phase::Editor => self.editor_ui(ui),
            Phase::Recording { .. } => self.recording_ui(ui),
            Phase::Scrolling { .. } => self.scrolling_ui(ui),
        }
        // Pins stay on screen in every phase (an immediate window not shown in a frame closes).
        self.show_pins(&ctx);
        if !matches!(self.phase, Phase::Capturing | Phase::Overlay(_)) {
            self.handle_flyout(&ctx);
            if self.history_open {
                self.history_window(&ctx);
            }
            if self.settings_open {
                self.settings_window(&ctx);
            } else {
                self.settings_styled = false;
            }
        }
    }
}

fn main() -> eframe::Result<()> {
    diag::install_panic_hook();
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
            let mut app = app;
            if app.bar_hidden() {
                ghost_main(&cc.egui_ctx);
            }
            // For testing: open an image straight in the editor.
            if let Some(img) = std::env::var_os("SNIPPY_OPEN_EDITOR").and_then(|p| image::open(p).ok()) {
                app.show_editor(&cc.egui_ctx, img.to_rgba8());
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

/// Makes the main window invisible to the user *without* hiding it from Windows: 1×1 px,
/// transparent (nothing is painted) and click-through.
///
/// Why not just hide it: eframe draws secondary windows (flyout, settings, overlays) from
/// the main window's frame, and while the main window is hidden it can't *create* new ones.
fn ghost_main(ctx: &egui::Context) {
    ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
    ctx.send_viewport_cmd(egui::ViewportCommand::Decorations(false));
    ctx.send_viewport_cmd(egui::ViewportCommand::MousePassthrough(true));
    ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(Vec2::splat(1.0)));
    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
}

/// Editor window size for an image of `img` pixels: native size where it fits.
/// Set once Snippy is really exiting (as opposed to the editor window's close button).
static QUITTING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Exits Snippy.
pub(crate) fn quit(ctx: &egui::Context) {
    QUITTING.store(true, std::sync::atomic::Ordering::Relaxed);
    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
}

fn editor_size(img: Vec2, ppp: f32, monitor: Vec2) -> Vec2 {
    (img / ppp + Vec2::new(32.0, 110.0)).max(Vec2::new(760.0, 420.0)).min(monitor * 0.85)
}
