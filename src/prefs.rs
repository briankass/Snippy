use super::*;
use crate::icons::{self, Icon};
use crate::updater::{self, Release};
use egui::{RichText, Sense};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

// ───────────────────────────── Settings window & updates ─────────────────────────────

const TITLE: &str = "Snippy settings";

pub(crate) enum UpdateState {
    Idle,
    Checking { rx: Receiver<Result<Option<Release>, String>>, manual: bool },
    UpToDate,
    Available(Release),
    Downloading { rx: Receiver<Result<PathBuf, String>>, progress: Arc<AtomicU64>, release: Release },
    Failed(String),
}

/// One-click looks (the tint colour is kept).
const PRESETS: [(&str, Look); 4] = [
    ("Clear", Look { backdrop: Backdrop::Clear, tint: ACCENT, tint_strength: 0.06, frosting: 0.0, darkness: 0.16 }),
    ("Frosted", Look { backdrop: Backdrop::Blur, tint: ACCENT, tint_strength: 0.10, frosting: 0.08, darkness: 0.45 }),
    ("Tinted", Look { backdrop: Backdrop::Blur, tint: ACCENT, tint_strength: 0.45, frosting: 0.10, darkness: 0.35 }),
    ("Solid", Look { backdrop: Backdrop::Off, tint: ACCENT, tint_strength: 0.10, frosting: 0.0, darkness: 0.9 }),
];

/// Swatches for the glass tint.
const TINTS: [(u8, u8, u8); 8] = [
    (255, 196, 77),  // amber
    (76, 194, 255),  // Windows blue
    (120, 220, 140), // green
    (190, 130, 255), // violet
    (255, 110, 160), // pink
    (255, 120, 80),  // orange
    (90, 210, 210),  // teal
    (200, 200, 205), // neutral
];

impl SnippyApp {
    pub(crate) fn check_for_updates(&mut self, ctx: &egui::Context, manual: bool) {
        if matches!(self.update, UpdateState::Checking { .. } | UpdateState::Downloading { .. }) {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(updater::check());
            ctx.request_repaint();
        });
        self.update = UpdateState::Checking { rx, manual };
    }

    pub(crate) fn start_download(&mut self, ctx: &egui::Context, release: Release) {
        let (tx, rx) = mpsc::channel();
        let progress = Arc::new(AtomicU64::new(0));
        let (p, rel, ctx) = (progress.clone(), release.clone(), ctx.clone());
        std::thread::spawn(move || {
            let res = updater::download(&rel, &p);
            let _ = tx.send(res);
            ctx.request_repaint();
        });
        self.update = UpdateState::Downloading { rx, progress, release };
    }

    /// Called every frame (also while hidden) to collect background results.
    pub(crate) fn poll_updates(&mut self, ctx: &egui::Context) {
        match &self.update {
            UpdateState::Checking { rx, manual } => {
                let manual = *manual;
                match rx.try_recv() {
                    Ok(Ok(Some(rel))) => {
                        // Automatic checks announce themselves with a flyout; manual ones
                        // show the result in the settings window.
                        if !manual && self.flyout.is_none() {
                            let anchor = self.flyout_anchor(ctx);
                            self.flyout = Some(Flyout::update(rel.clone(), anchor));
                        }
                        self.update = UpdateState::Available(rel);
                    }
                    Ok(Ok(None)) => self.update = UpdateState::UpToDate,
                    Ok(Err(e)) => self.update = if manual { UpdateState::Failed(e) } else { UpdateState::Idle },
                    Err(TryRecvError::Disconnected) => self.update = UpdateState::Failed("update check crashed".into()),
                    Err(TryRecvError::Empty) => {}
                }
            }
            UpdateState::Downloading { rx, .. } => match rx.try_recv() {
                Ok(Ok(path)) => match updater::install_and_restart(&path) {
                    Ok(()) => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
                    Err(e) => self.update_failed(ctx, e),
                },
                Ok(Err(e)) => self.update_failed(ctx, e),
                Err(TryRecvError::Disconnected) => self.update_failed(ctx, "download crashed".into()),
                Err(TryRecvError::Empty) => {
                    ctx.request_repaint_after(Duration::from_millis(200));
                    if let (Some(f), UpdateState::Downloading { progress, release, .. }) = (self.flyout.as_mut(), &self.update) {
                        if matches!(f.kind, FlyoutKind::Update(_)) {
                            f.flash(progress_text(progress.load(Ordering::Relaxed), release.size));
                        }
                    }
                }
            },
            _ => {}
        }
    }

    fn update_failed(&mut self, ctx: &egui::Context, e: String) {
        let anchor = self.flyout_anchor(ctx);
        self.flyout = Some(Flyout::message("Update failed", e.clone(), anchor));
        self.update = UpdateState::Failed(e);
    }

    pub(crate) fn settings_window(&mut self, ctx: &egui::Context) {
        let builder = egui::ViewportBuilder::default()
            .with_title(TITLE)
            .with_inner_size([620.0, 720.0])
            .with_min_inner_size([480.0, 360.0])
            .with_resizable(true);
        ctx.show_viewport_immediate(egui::ViewportId::from_hash_of("snippy_settings"), builder, |ui, _| {
            if ui.input(|i| i.viewport().close_requested()) {
                self.settings_open = false;
            }
            egui::CentralPanel::default().show(ui, |ui| {
                egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| self.settings_page(ui));
            });
        });
        if !self.settings_styled {
            // Dark title bar, and keep the window out of screenshots taken while it's open.
            if let Some(h) = platform::find_window(TITLE) {
                platform::style_window(h, Backdrop::Off);
                self.settings_styled = true;
            }
        }
    }

    fn settings_page(&mut self, ui: &mut egui::Ui) {
        ui.spacing_mut().item_spacing.y = 6.0;
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.heading("Snippy settings");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.weak(format!("Version {}", env!("CARGO_PKG_VERSION")));
            });
        });
        if !self.prefs_status.is_empty() {
            ui.colored_label(Color32::from_rgb(255, 196, 90), &self.prefs_status);
        }

        // ── General
        section(ui, "General", |ui| {
            let mut startup = *self.startup.get_or_insert_with(platform::startup_enabled);
            row(ui, Icon::Power, "Start with Windows", "Open Snippy automatically when you sign in", |ui| {
                if toggle(ui, &mut startup).changed() {
                    match platform::set_startup(startup) {
                        Ok(()) => {
                            self.startup = Some(startup);
                            self.prefs_status.clear();
                        }
                        Err(e) => {
                            self.startup = None;
                            self.prefs_status = e;
                        }
                    }
                }
            });
            let ctx = ui.ctx().clone();
            row(ui, Icon::Tray, "Show icon in the system tray", "Quick snips and recording from the taskbar corner; left-click shows or hides the bar", |ui| {
                if toggle(ui, &mut self.settings.tray_icon).changed() {
                    self.sync_tray(&ctx);
                }
            });
            if self.settings.tray_icon {
                row(ui, Icon::Grid, "Hide the floating bar", "Run from the tray and shortcuts only (left-click the tray icon to bring the bar back)", |ui| {
                    if toggle(ui, &mut self.settings.hide_bar).changed() && matches!(self.phase, Phase::Bar) {
                        self.show_bar(&ctx);
                    }
                });
            }
            row(ui, Icon::Refresh, "Check for updates automatically", "Looks for a new release on GitHub when Snippy starts", |ui| {
                toggle(ui, &mut self.settings.check_updates);
            });
            self.update_row(ui);
        });

        // ── Shortcuts
        section(ui, "Keyboard shortcuts", |ui| {
            let mut changed = false;
            row(ui, Icon::Keyboard, "Print Screen opens Snippy", "Instead of the Windows Snipping Tool (Alt+Print Screen keeps its usual job)", |ui| {
                changed |= toggle(ui, &mut self.settings.hotkey_print_screen).changed();
            });
            row(ui, Icon::Keyboard, "Windows + Shift + S opens Snippy", "Also the screenshot key on keyboards that send this combo (e.g. Microsoft Designer Compact). Replaces the Snipping Tool shortcut", |ui| {
                changed |= toggle(ui, &mut self.settings.hotkey_win_shift_s).changed();
            });
            if cfg!(windows) {
                // Mirrors the Windows setting itself (read live, written only when toggled).
                let mut windows_tool = !*self.prtsc_windows.get_or_insert_with(platform::prtsc_opens_snipping_tool);
                row(ui, Icon::Keyboard, "Stop Windows opening its Snipping Tool on Print Screen",
                    "Turns off Windows' own \"Use the Print Screen key to open screen capture\" setting, as a backup in case a key press ever slips past Snippy", |ui| {
                    if toggle(ui, &mut windows_tool).changed() {
                        match platform::set_prtsc_opens_snipping_tool(!windows_tool) {
                            Ok(()) => self.prtsc_windows = Some(!windows_tool),
                            Err(e) => {
                                self.prtsc_windows = None;
                                self.prefs_status = e;
                            }
                        }
                    }
                });
            }
            row(ui, Icon::Region, "When a shortcut is pressed", "Show the bar to pick what to capture, or start snipping right away", |ui| {
                ui.selectable_value(&mut self.settings.shortcut_action, ShortcutAction::Snip, "Start a snip");
                ui.selectable_value(&mut self.settings.shortcut_action, ShortcutAction::ShowBar, "Show the bar");
            });
            ui.horizontal(|ui| {
                ui.add_space(40.0);
                if ui.link("Open diagnostics log").on_hover_text(diag::path().display().to_string()).clicked() {
                    let p = diag::path();
                    if p.exists() {
                        let _ = reveal_in_folder(&p);
                    } else {
                        self.prefs_status = "No diagnostics logged yet (that's good)".into();
                    }
                }
            });
            if changed {
                hotkeys::configure(self.settings.hotkey_print_screen, self.settings.hotkey_win_shift_s);
            }
            ui.add(egui::Label::new(RichText::new(
                "\"Show the bar\" brings the bar to the front (also in tray-only mode, until your next capture). \
                 \"Start a snip\": Windows + Shift + S snips a region, Print Screen uses the mode selected on the bar. \
                 Windows doesn't pass keys to Snippy while an app running as administrator is in front.",
            ).size(11.0).weak()).wrap());
        });

        // ── Saving
        section(ui, "Saving", |ui| {
            let default_shots = platform::default_dir(Folder::Pictures);
            let default_videos = platform::default_dir(Folder::Videos);
            row(ui, Icon::Save, "Save screenshots automatically", "Every snip is saved as a PNG (and copied to the clipboard)", |ui| {
                toggle(ui, &mut self.settings.auto_save);
            });
            folder_row(ui, Icon::Folder, "Screenshots folder", &mut self.settings.screenshot_dir, &default_shots, &mut self.prefs_status);
            folder_row(ui, Icon::Video, "Recordings folder", &mut self.settings.video_dir, &default_videos, &mut self.prefs_status);
        });

        // ── Recording
        section(ui, "Recording", |ui| {
            row(ui, Icon::Video, "Frame rate", "Higher is smoother but makes bigger files", |ui| {
                for fps in [60, 30, 15] {
                    ui.selectable_value(&mut self.settings.fps, fps, format!("{fps} fps"));
                }
            });
            row(ui, Icon::Pointer, "Show mouse pointer", "Draw the pointer into recordings", |ui| {
                toggle(ui, &mut self.settings.show_cursor);
            });
            row(ui, Icon::Speaker, "Record system audio", "Sound playing on your PC (also on the bar in video mode)", |ui| {
                toggle(ui, &mut self.settings.system_audio);
            });
            row(ui, Icon::Mic, "Record microphone", "Your default microphone (also on the bar in video mode)", |ui| {
                toggle(ui, &mut self.settings.microphone);
            });
        });

        // ── Appearance
        let before = self.settings.look;
        section(ui, "Appearance", |ui| {
            let look = &mut self.settings.look;
            row(ui, Icon::Sun, "Look", "One-click styles; fine-tune them below", |ui| {
                for (name, preset) in PRESETS.iter().rev() {
                    let active = look.backdrop == preset.backdrop
                        && (look.darkness - preset.darkness).abs() < 0.01
                        && (look.frosting - preset.frosting).abs() < 0.01
                        && (look.tint_strength - preset.tint_strength).abs() < 0.01;
                    if ui.selectable_label(active, *name).clicked() {
                        *look = Look { tint: look.tint, ..*preset };
                    }
                }
            });
            row(ui, Icon::Grid, "Glass style", look.backdrop.help(), |ui| {
                egui::ComboBox::from_id_salt("backdrop")
                    .selected_text(look.backdrop.label())
                    .width(210.0)
                    .show_ui(ui, |ui| {
                        for b in Backdrop::ALL {
                            ui.selectable_value(&mut look.backdrop, b, b.label());
                        }
                    });
            });
            row(ui, Icon::Brush, "Tint colour", "The colour mixed into the glass and used for highlights", |ui| {
                ui.color_edit_button_srgba(&mut look.tint);
                for &(r, g, b) in TINTS.iter().rev() {
                    let c = Color32::from_rgb(r, g, b);
                    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(22.0), Sense::click());
                    let selected = look.tint == c;
                    ui.painter().circle_filled(rect.center(), 9.0, c);
                    if selected || resp.hovered() {
                        ui.painter().circle_stroke(rect.center(), 10.5, egui::Stroke::new(2.0, Color32::WHITE));
                    }
                    if resp.clicked() {
                        look.tint = c;
                    }
                }
            });
            percent_row(ui, Icon::Brush, "Tint strength", "How much colour is mixed in", &mut look.tint_strength);
            percent_row(ui, Icon::Sun, "Frosting", "Lower = clearer, more see-through", &mut look.frosting);
            percent_row(ui, Icon::Grid, "Panel darkness", "Darkness behind the icons; raise it if icons are hard to see", &mut look.darkness);
            ui.horizontal(|ui| {
                ui.add_space(40.0);
                if ui.button("Reset appearance").clicked() {
                    *look = Look::default();
                }
                if !matches!(look.backdrop, Backdrop::Off) && !self.glass && self.main_hwnd.is_some() {
                    ui.colored_label(Color32::from_rgb(255, 196, 90), "Windows didn't accept this glass style; showing the solid panel instead.");
                }
            });
        });
        if self.settings.look.backdrop != before.backdrop {
            self.restyle();
        }
        ui.add_space(8.0);
    }

    fn update_row(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let (detail, mut action): (String, Option<Release>) = match &self.update {
            UpdateState::Idle => ("Not checked yet".into(), None),
            UpdateState::Checking { .. } => ("Checking…".into(), None),
            UpdateState::UpToDate => (format!("You have the latest version ({})", env!("CARGO_PKG_VERSION")), None),
            UpdateState::Available(r) => (format!("Version {} is available", updater::version_string(r.version)), None),
            UpdateState::Downloading { progress, release, .. } => (progress_text(progress.load(Ordering::Relaxed), release.size), None),
            UpdateState::Failed(e) => (format!("Couldn't check: {e}"), None),
        };
        row(ui, Icon::Refresh, "Updates", &detail, |ui| match &self.update {
            UpdateState::Available(r) => {
                if ui.button(RichText::new(format!("Update to {}", updater::version_string(r.version))).strong()).clicked() {
                    action = Some(r.clone());
                }
                if ui.link("What's new").clicked() {
                    let _ = open_url(if r.page.is_empty() { &r.tag } else { &r.page });
                }
            }
            UpdateState::Checking { .. } => {
                ui.spinner();
            }
            UpdateState::Downloading { progress, release, .. } => {
                let frac = if release.size > 0 { progress.load(Ordering::Relaxed) as f32 / release.size as f32 } else { 0.0 };
                ui.add(egui::ProgressBar::new(frac).desired_width(160.0).show_percentage());
            }
            _ => {
                if ui.button("Check for updates").clicked() {
                    self.check_for_updates(&ctx, true);
                }
            }
        });
        if let Some(rel) = action {
            self.start_download(&ctx, rel);
        }
    }
}

fn progress_text(done: u64, total: u64) -> String {
    if total > 0 {
        format!("Downloading… {:.0}% of {:.1} MB", done as f64 * 100.0 / total as f64, total as f64 / 1_048_576.0)
    } else {
        format!("Downloading… {:.1} MB", done as f64 / 1_048_576.0)
    }
}

fn section(ui: &mut egui::Ui, title: &str, add: impl FnOnce(&mut egui::Ui)) {
    ui.add_space(8.0);
    ui.label(RichText::new(title).size(13.0).strong().color(Color32::from_gray(170)));
    egui::Frame::group(ui.style())
        .corner_radius(egui::CornerRadius::same(8))
        .inner_margin(egui::Margin::same(10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui);
        });
}

/// One settings row: icon, title + explanation on the left, control on the right.
fn row(ui: &mut egui::Ui, icon: Icon, title: &str, subtitle: &str, control: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal(|ui| {
        let (r, _) = ui.allocate_exact_size(Vec2::splat(26.0), Sense::hover());
        icons::draw(ui.painter(), icon, r, Color32::from_gray(225));
        ui.add_space(6.0);
        ui.vertical(|ui| {
            ui.set_max_width((ui.available_width() * 0.5).max(180.0));
            ui.label(RichText::new(title).size(14.0));
            ui.add(egui::Label::new(RichText::new(subtitle).size(11.5).weak()).wrap());
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), control);
    });
    ui.add_space(2.0);
}

fn percent_row(ui: &mut egui::Ui, icon: Icon, title: &str, subtitle: &str, value: &mut f32) {
    row(ui, icon, title, subtitle, |ui| {
        ui.add_sized([40.0, 18.0], egui::Label::new(format!("{:.0}%", *value * 100.0)));
        ui.spacing_mut().slider_width = 200.0;
        ui.add(egui::Slider::new(value, 0.0..=1.0).show_value(false));
    });
}

fn folder_row(ui: &mut egui::Ui, icon: Icon, title: &str, dir: &mut Option<PathBuf>, default: &Path, status: &mut String) {
    let shown = dir.clone().unwrap_or_else(|| default.to_path_buf());
    let subtitle = if dir.is_none() { format!("{} (default)", shown.display()) } else { shown.display().to_string() };
    row(ui, icon, title, &subtitle, |ui| {
        if dir.is_some() && ui.button("Reset").on_hover_text(format!("Back to {}", default.display())).clicked() {
            *dir = None;
        }
        if ui.button("Open").clicked() {
            let _ = std::fs::create_dir_all(&shown);
            if let Err(e) = open_path(&shown) {
                *status = format!("Couldn't open folder: {e}");
            }
        }
        if ui.button("Browse…").clicked() {
            if let Some(p) = rfd::FileDialog::new().set_directory(&shown).pick_folder() {
                *dir = Some(p);
            }
        }
    });
}

/// iOS-style switch.
fn toggle(ui: &mut egui::Ui, on: &mut bool) -> egui::Response {
    let size = Vec2::new(40.0, 22.0);
    let (rect, mut resp) = ui.allocate_exact_size(size, Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let t = ui.ctx().animate_bool_responsive(resp.id, *on);
    let p = ui.painter();
    let track = Color32::from_gray(80).lerp_to_gamma(Color32::from_rgb(76, 194, 255), t);
    p.rect_filled(rect, 11.0, track);
    let x = egui::lerp((rect.left() + 11.0)..=(rect.right() - 11.0), t);
    p.circle_filled(Pos2::new(x, rect.center().y), 8.0, Color32::WHITE);
    resp
}
