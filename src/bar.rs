use super::*;
use crate::icons::{self, Icon};
use egui::{Sense, Stroke, StrokeKind};

// ───────────────────────────── Floating bars ─────────────────────────────
//
// Both bars (the main one and the compact recording one) are painted by hand in a fixed
// layout: vector icons, no fonts for glyphs, nothing that can overflow the 52 px window.

pub(crate) const BAR_W: f32 = 52.0;
const ITEM: f32 = 40.0;
const GAP: f32 = 4.0;
const PAD: f32 = 6.0;
const GRIP_H: f32 = 14.0;
const SEP_H: f32 = 7.0;
const STATUS_H: f32 = 44.0;
const RADIUS: f32 = 8.0; // matches the Windows 11 DWM window-corner radius

#[derive(Clone, Copy, PartialEq, Debug)]
enum Item {
    Grip,
    Sep,
    Mode(CaptureMode),
    RecordToggle,
    Speaker,
    Mic,
    Timer,
    Ocr,
    ColorPick,
    History,
    Settings,
    HideToTray,
    // recording bar
    Status,
    Pause,
    Stop,
}

impl Item {
    fn height(self) -> f32 {
        match self {
            Item::Grip => GRIP_H,
            Item::Sep => SEP_H,
            Item::Status => STATUS_H,
            _ => ITEM,
        }
    }
}

fn bar_items(s: &Settings) -> Vec<Item> {
    use CaptureMode::*;
    let mut v = vec![Item::Grip, Item::Mode(Region), Item::Mode(Window), Item::Mode(FullScreen)];
    if !s.record_mode {
        v.extend([Item::Ocr, Item::ColorPick]);
    }
    v.extend([Item::Sep, Item::RecordToggle]);
    if s.record_mode {
        v.extend([Item::Speaker, Item::Mic]);
    }
    v.extend([Item::Timer, Item::Sep, Item::History, Item::Settings, Item::HideToTray]);
    v
}

const REC_ITEMS: [Item; 5] = [Item::Grip, Item::Status, Item::Sep, Item::Pause, Item::Stop];

fn size_of(items: &[Item]) -> Vec2 {
    let h: f32 = items.iter().map(|i| i.height()).sum::<f32>() + GAP * (items.len() as f32 - 1.0) + PAD * 2.0;
    Vec2::new(BAR_W, h)
}

pub(crate) fn bar_size(s: &Settings) -> Vec2 {
    size_of(&bar_items(s))
}

pub(crate) fn rec_bar_size() -> Vec2 {
    size_of(&REC_ITEMS)
}

/// Background shared by the bars, tooltip and flyout. Over a glass backdrop it's three
/// translucent layers (darkness, tint colour, frosting) the user can tune in Settings;
/// without glass it's a nearly opaque dark panel with a hint of the tint.
pub(crate) fn paint_glass_panel(p: &egui::Painter, rect: Rect, look: &Look, glass: bool, radius: f32) {
    paint_glass_panel_min(p, rect, look, glass, radius, 0.0);
}

/// Like [`paint_glass_panel`], with a minimum darkness so text stays readable (tooltips,
/// flyout) even with a very clear look.
pub(crate) fn paint_glass_panel_min(p: &egui::Painter, rect: Rect, look: &Look, glass: bool, radius: f32, min_dark: f32) {
    let a = |f: f32, max: f32| (f.clamp(0.0, 1.0) * max) as u8;
    let [tr, tg, tb, _] = look.tint.to_array();
    if glass {
        p.rect_filled(rect, radius, Color32::from_black_alpha(a(look.darkness.max(min_dark), 235.0)));
        p.rect_filled(rect, radius, Color32::from_rgba_unmultiplied(tr, tg, tb, a(look.tint_strength, 200.0)));
        p.rect_filled(rect, radius, Color32::from_white_alpha(a(look.frosting, 120.0)));
        // Soft reflection across the top, like light on a pane of glass.
        let top = Rect::from_min_max(
            Pos2::new(rect.min.x + 1.0, rect.min.y + radius * 0.6),
            Pos2::new(rect.max.x - 1.0, rect.min.y + (rect.height() * 0.35).min(120.0)),
        );
        let mut mesh = egui::Mesh::default();
        let (hi, lo) = (Color32::from_white_alpha(30), Color32::TRANSPARENT);
        mesh.colored_vertex(top.left_top(), hi);
        mesh.colored_vertex(top.right_top(), hi);
        mesh.colored_vertex(top.right_bottom(), lo);
        mesh.colored_vertex(top.left_bottom(), lo);
        mesh.add_triangle(0, 1, 2);
        mesh.add_triangle(0, 2, 3);
        p.add(mesh);
        p.rect_filled(Rect::from_min_max(rect.min + Vec2::new(radius, 0.5), Pos2::new(rect.max.x - radius, rect.min.y + 1.5)), 0.0, Color32::from_white_alpha(50));
    } else {
        p.rect_filled(rect, radius, Color32::from_rgba_unmultiplied(30, 30, 34, 250));
        p.rect_filled(rect, radius, Color32::from_rgba_unmultiplied(tr, tg, tb, a(look.tint_strength, 50.0)));
    }
    p.rect_stroke(rect.shrink(0.5), radius, Stroke::new(1.0, look.tint.gamma_multiply(0.6)), StrokeKind::Inside);
}

/// Paints a square icon button and returns its response.
fn icon_button(ui: &mut egui::Ui, id: egui::Id, rect: Rect, icon: Icon, fill: Option<Color32>, enabled: bool, shadow: bool) -> egui::Response {
    let resp = ui.interact(rect, id, if enabled { Sense::click() } else { Sense::hover() });
    let p = ui.painter();
    let mut icon_color = Color32::from_gray(236);
    if let Some(f) = fill {
        p.rect_filled(rect, 10.0, f);
        // dark icon on light fills, white on dark/saturated ones
        let lum = 0.299 * f.r() as f32 + 0.587 * f.g() as f32 + 0.114 * f.b() as f32;
        icon_color = if lum > 150.0 { Color32::from_gray(24) } else { Color32::WHITE };
    }
    if enabled && resp.hovered() {
        let a = if resp.is_pointer_button_down_on() { 46 } else { 26 };
        p.rect_filled(rect, 10.0, Color32::from_white_alpha(a));
    }
    if !enabled {
        icon_color = icon_color.gamma_multiply(0.4);
    }
    if shadow && fill.is_none() {
        // Over clear glass the background can be anything; a soft shadow keeps icons legible.
        for off in [Vec2::new(0.0, 1.2), Vec2::new(1.0, 1.0), Vec2::new(-1.0, 1.0), Vec2::new(0.0, -0.6)] {
            icons::draw(p, icon, rect.shrink(9.0).translate(off), Color32::from_black_alpha(110));
        }
    }
    icons::draw(p, icon, rect.shrink(9.0), icon_color);
    resp
}

impl SnippyApp {
    pub(crate) fn bar_ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let capturing = matches!(self.phase, Phase::Capturing);
        let record = self.settings.record_mode;
        let sel_fill = if record { REC_RED } else { self.settings.look.tint };
        let mut hovered_tip: Option<(String, f32)> = None;
        let mut resize = false;
        let clear = self.settings.look.backdrop == platform::Backdrop::Clear && self.glass;

        egui::CentralPanel::default().frame(egui::Frame::NONE).show(ui, |ui| {
            let full = ui.max_rect();
            paint_glass_panel(ui.painter(), full, &self.settings.look, self.glass, RADIUS);
            // Dragging anywhere that isn't a button moves the bar.
            let bg = ui.interact(full, ui.scope_id().with("bar_bg"), Sense::click_and_drag());
            if bg.drag_started() {
                ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
            }

            let mut y = full.min.y + PAD;
            for (i, item) in bar_items(&self.settings).into_iter().enumerate() {
                let h = item.height();
                let r = Rect::from_min_size(Pos2::new(full.center().x - ITEM / 2.0, y), Vec2::new(ITEM, h));
                y += h + GAP;
                let id = ui.scope_id().with(("bar_item", i));
                let p = ui.painter();
                let (icon, tip, fill) = match item {
                    Item::Grip => {
                        icons::draw(p, Icon::Grip, Rect::from_center_size(r.center(), Vec2::splat(22.0)), Color32::from_gray(150));
                        continue;
                    }
                    Item::Sep => {
                        p.hline(r.x_range().shrink(4.0), r.center().y, Stroke::new(1.0, Color32::from_white_alpha(38)));
                        continue;
                    }
                    Item::Mode(m) => {
                        let (icon, what) = match m {
                            CaptureMode::Region => (Icon::Region, "region"),
                            CaptureMode::Window => (Icon::Window, "window"),
                            CaptureMode::FullScreen => (Icon::Screen, "full screen"),
                        };
                        let verb = if record { "Record" } else { "Snip" };
                        (icon, format!("{verb} {what}"), (self.settings.mode == m).then_some(sel_fill))
                    }
                    Item::RecordToggle => (
                        Icon::Video,
                        if record { "Video mode (click for screenshots)".into() } else { "Screenshot mode (click for video)".into() },
                        record.then_some(REC_RED),
                    ),
                    Item::Speaker => (
                        if self.settings.system_audio { Icon::Speaker } else { Icon::SpeakerOff },
                        format!("System audio: {}", on_off(self.settings.system_audio)),
                        self.settings.system_audio.then_some(Color32::from_white_alpha(30)),
                    ),
                    Item::Mic => (
                        if self.settings.microphone { Icon::Mic } else { Icon::MicOff },
                        format!("Microphone: {}", on_off(self.settings.microphone)),
                        self.settings.microphone.then_some(Color32::from_white_alpha(30)),
                    ),
                    Item::Timer => (
                        Icon::Timer,
                        match self.settings.delay_secs {
                            0 if record => "Countdown: off (click to change)".into(),
                            0 => "Delay: off (click to change)".into(),
                            n if record => format!("Countdown: {n}s (right-click to turn off)"),
                            n => format!("Delay: {n}s (right-click to turn off)"),
                        },
                        None,
                    ),
                    Item::Ocr => (Icon::TextScan, "Copy text from screen".into(), None),
                    Item::ColorPick => (Icon::Pipette, "Pick a colour".into(), None),
                    Item::History => (Icon::History, "Recent captures".into(), self.history_open.then_some(Color32::from_white_alpha(30))),
                    Item::Settings => (Icon::Gear, "Settings".into(), self.settings_open.then_some(Color32::from_white_alpha(30))),
                    Item::HideToTray => (Icon::HideToTray, "Hide to system tray (right-click to exit)".into(), None),
                    Item::Status | Item::Pause | Item::Stop => continue,
                };

                let enabled = !capturing || matches!(item, Item::HideToTray | Item::Settings | Item::History);
                let resp = icon_button(ui, id, r, icon, fill, enabled, clear);
                if item == Item::Timer && self.settings.delay_secs > 0 {
                    // little badge with the seconds
                    let badge = Rect::from_center_size(r.right_bottom() - Vec2::new(8.0, 8.0), Vec2::new(18.0, 14.0));
                    ui.painter().rect_filled(badge, 7.0, ACCENT);
                    ui.painter().text(badge.center(), egui::Align2::CENTER_CENTER, self.settings.delay_secs.to_string(),
                        egui::FontId::proportional(10.0), Color32::from_gray(20));
                }
                if resp.hovered() {
                    hovered_tip = Some((tip, r.center().y - full.min.y));
                }
                if resp.clicked() {
                    match item {
                        Item::Mode(m) => {
                            self.settings.mode = m;
                            let purpose = if record { Purpose::Record } else { Purpose::Snip };
                            self.start_capture(&ctx, purpose);
                        }
                        Item::RecordToggle => {
                            self.settings.record_mode = !record;
                            resize = true;
                        }
                        Item::Speaker => self.settings.system_audio ^= true,
                        Item::Mic => self.settings.microphone ^= true,
                        Item::Timer => {
                            self.settings.delay_secs = match self.settings.delay_secs {
                                0 => 3,
                                3 => 5,
                                5 => 10,
                                _ => 0,
                            }
                        }
                        Item::Ocr => self.start_capture(&ctx, Purpose::Ocr),
                        Item::ColorPick => self.start_capture(&ctx, Purpose::ColorPick),
                        Item::History => self.history_open = !self.history_open,
                        Item::Settings => self.settings_open = !self.settings_open,
                        Item::HideToTray => self.hide_to_tray(&ctx),
                        _ => {}
                    }
                }
                if item == Item::Timer && resp.secondary_clicked() {
                    self.settings.delay_secs = 0;
                }
                if item == Item::HideToTray && resp.secondary_clicked() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        });

        if resize {
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(bar_size(&self.settings)));
        }
        self.show_bar_tooltip(&ctx, ui, hovered_tip);
    }

    pub(crate) fn recording_ui(&mut self, ui: &mut egui::Ui) {
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
        let mut hovered_tip = None;
        let clear = self.settings.look.backdrop == platform::Backdrop::Clear && self.glass;
        egui::CentralPanel::default().frame(egui::Frame::NONE).show(ui, |ui| {
            let full = ui.max_rect();
            paint_glass_panel(ui.painter(), full, &self.settings.look, self.glass, RADIUS);
            let bg = ui.interact(full, ui.scope_id().with("rec_bg"), Sense::click_and_drag());
            if bg.drag_started() {
                ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
            }
            let mut y = full.min.y + PAD;
            for (i, item) in REC_ITEMS.into_iter().enumerate() {
                let h = item.height();
                let r = Rect::from_min_size(Pos2::new(full.center().x - ITEM / 2.0, y), Vec2::new(ITEM, h));
                y += h + GAP;
                let p = ui.painter();
                match item {
                    Item::Grip => icons::draw(p, Icon::Grip, Rect::from_center_size(r.center(), Vec2::splat(22.0)), Color32::from_gray(150)),
                    Item::Sep => {
                        p.hline(r.x_range().shrink(4.0), r.center().y, Stroke::new(1.0, Color32::from_white_alpha(38)));
                    }
                    Item::Status => match countdown {
                        Some(n) => {
                            p.text(r.center(), egui::Align2::CENTER_CENTER, n.to_string(), egui::FontId::proportional(26.0), REC_RED);
                        }
                        None => {
                            let blink = paused || (ctx.input(|i| i.time) * 2.0) as i64 % 2 == 0;
                            if blink {
                                let dot = if paused { Color32::from_rgb(240, 180, 40) } else { REC_RED };
                                p.circle_filled(r.center_top() + Vec2::new(0.0, 10.0), 5.5, dot);
                            }
                            let secs = elapsed.as_secs();
                            p.text(r.center_bottom() - Vec2::new(0.0, 10.0), egui::Align2::CENTER_CENTER,
                                format!("{}:{:02}", secs / 60, secs % 60), egui::FontId::monospace(12.5), Color32::WHITE);
                        }
                    },
                    Item::Pause => {
                        let (icon, tip) = if paused { (Icon::Play, "Resume") } else { (Icon::Pause, "Pause") };
                        let resp = icon_button(ui, ui.scope_id().with(("rec", i)), r, icon, None, countdown.is_none(), clear);
                        if resp.hovered() {
                            hovered_tip = Some((tip.to_string(), r.center().y - full.min.y));
                        }
                        toggle_pause |= resp.clicked();
                    }
                    Item::Stop => {
                        let resp = icon_button(ui, ui.scope_id().with(("rec", i)), r, Icon::Stop, Some(REC_RED), true, clear);
                        if resp.hovered() {
                            hovered_tip = Some(("Stop and save".to_string(), r.center().y - full.min.y));
                        }
                        stop |= resp.clicked();
                    }
                    _ => {}
                }
            }
        });
        self.show_bar_tooltip(&ctx, ui, hovered_tip);

        if toggle_pause {
            if let Phase::Recording { recorder, .. } = &self.phase {
                recorder.set_paused(!paused);
            }
        }
        if stop {
            self.stop_recording(&ctx);
        }
    }

    /// Tooltips can't fit inside a 52 px window, so they get their own little window
    /// beside the bar, shown after a short hover.
    fn show_bar_tooltip(&mut self, ctx: &egui::Context, ui: &egui::Ui, hovered: Option<(String, f32)>) {
        let now = ctx.input(|i| i.time);
        let Some((text, local_y)) = hovered else {
            self.tip = None;
            return;
        };
        let since = match &self.tip {
            Some((t, since, _)) if *t == text => *since,
            _ => {
                self.tip = Some((text.clone(), now, None));
                now
            }
        };
        if now - since < 0.45 {
            ctx.request_repaint_after(Duration::from_millis(80));
            return;
        }
        let Some(outer) = ctx.input(|i| i.viewport().outer_rect) else { return };
        let font = egui::FontId::proportional(13.0);
        let galley = ui.painter().layout_no_wrap(text.clone(), font.clone(), Color32::WHITE);
        let size = galley.size() + Vec2::new(20.0, 12.0);
        let screen_w = ctx.input(|i| i.viewport().monitor_size.map_or(f32::MAX, |m| m.x));
        let x = if outer.max.x + 8.0 + size.x > screen_w { outer.min.x - 8.0 - size.x } else { outer.max.x + 8.0 };
        let pos = Pos2::new(x, outer.min.y + local_y - size.y / 2.0);
        // Exact placement in physical pixels beside the bar (logical coordinates don't map
        // consistently across monitors with different scaling).
        let placed: Option<platform::PxRect> = self.main_hwnd.and_then(platform::window_rect).map(|[l, t, r, b]| {
            let ppp = ctx.input(|i| i.viewport().native_pixels_per_point.unwrap_or(1.0));
            let (w, h) = ((size.x * ppp).round() as i32, (size.y * ppp).round() as i32);
            let gap = (8.0 * ppp) as i32;
            let right_edge = platform::work_area((l + r) / 2, (t + b) / 2).map_or(i32::MAX, |a| a[2]);
            let x = if r + gap + w > right_edge { l - gap - w } else { r + gap };
            let y = t + (local_y * ppp) as i32 - h / 2;
            [x, y, x + w, y + h]
        });

        let builder = egui::ViewportBuilder::default()
            .with_title(TOOLTIP_TITLE)
            .with_position(pos)
            .with_inner_size(size)
            .with_decorations(false)
            .with_transparent(true)
            .with_always_on_top()
            .with_mouse_passthrough(true)
            .with_taskbar(false)
            .with_resizable(false)
            .with_active(false);
        let (look, glass) = (self.settings.look, self.glass);
        ctx.show_viewport_immediate(egui::ViewportId::from_hash_of("snippy_tooltip"), builder, |ui, _| {
            let r = ui.max_rect();
            paint_glass_panel_min(ui.painter(), r, &look, glass, 6.0, 0.6);
            ui.painter().text(r.center(), egui::Align2::CENTER_CENTER, &text, font.clone(), Color32::WHITE);
        });
        if let Some(r) = placed {
            platform::place_window(TOOLTIP_TITLE, r);
        }
        let gen = self.style_gen;
        let backdrop = self.settings.look.backdrop;
        if let Some((_, _, styled)) = &mut self.tip {
            if *styled != Some(gen) && style_child_window(TOOLTIP_TITLE, backdrop).is_some() {
                *styled = Some(gen);
            }
        }
    }
}

const TOOLTIP_TITLE: &str = "Snippy tooltip";

fn on_off(b: bool) -> &'static str {
    if b { "on" } else { "off" }
}

/// Outlines the recorded area with four thin, solid, always-on-top strips placed just
/// *outside* it. They never overlap the area, so they can't end up in the video, and they
/// don't rely on window transparency (which isn't available everywhere).
/// `area` is in virtual-desktop physical pixels; strips are placed in physical pixels too,
/// so they line up on any monitor whatever its display scaling.
pub(crate) fn show_recording_border(ctx: &egui::Context, area: platform::PxRect, paused: bool) {
    let [l, t, r, b] = area;
    let (gap, th) = (1, 3);
    let (l, t, r, b) = (l - gap, t - gap, r + gap, b + gap);
    let strips: [platform::PxRect; 4] = [
        [l - th, t - th, r + th, t], // top
        [l - th, b, r + th, b + th], // bottom
        [l - th, t, l, b],           // left
        [r, t, r + th, b],           // right
    ];
    let ppp = ctx.input(|i| i.viewport().native_pixels_per_point.unwrap_or(1.0));
    let color = if paused { Color32::from_rgb(240, 180, 40) } else { REC_RED };
    for (i, s) in strips.into_iter().enumerate() {
        let title = format!("Snippy recording area {i}");
        // Approximate placement in points; corrected in physical pixels below.
        let builder = egui::ViewportBuilder::default()
            .with_title(&title)
            .with_position(Pos2::new(s[0] as f32, s[1] as f32) / ppp)
            .with_inner_size(Vec2::new((s[2] - s[0]) as f32, (s[3] - s[1]) as f32) / ppp)
            .with_min_inner_size(Vec2::splat(1.0))
            .with_decorations(false)
            .with_always_on_top()
            .with_mouse_passthrough(true)
            .with_taskbar(false)
            .with_resizable(false)
            .with_active(false);
        ctx.show_viewport_immediate(egui::ViewportId::from_hash_of(("snippy_rec_border", i)), builder, |ui, _| {
            let rect = ui.max_rect();
            ui.painter().rect_filled(rect, 0.0, color);
        });
        platform::place_window(&title, s);
    }
}

/// Finds one of our secondary windows by title and applies the glass/rounded/capture-excluded
/// style. Returns whether glass is active, or `None` if the window wasn't found (yet).
pub(crate) fn style_child_window(title: &str, backdrop: platform::Backdrop) -> Option<bool> {
    platform::find_window(title).map(|h| platform::style_window(h, backdrop))
}
