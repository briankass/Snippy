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
    Close,
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
    let mut v = vec![Item::Grip, Item::Mode(Region), Item::Mode(Window), Item::Mode(FullScreen), Item::Sep, Item::RecordToggle];
    if s.record_mode {
        v.extend([Item::Speaker, Item::Mic]);
    }
    v.extend([Item::Timer, Item::Sep, Item::Close]);
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

/// Background shared by the bars, tooltip and flyout: translucent tint over the acrylic
/// glass when available, otherwise a nearly opaque dark panel. Thin warm outline.
pub(crate) fn paint_glass_panel(p: &egui::Painter, rect: Rect, glass: bool, radius: f32) {
    let fill = if glass {
        Color32::from_rgba_unmultiplied(22, 22, 26, 120)
    } else {
        Color32::from_rgba_unmultiplied(32, 32, 36, 248)
    };
    p.rect_filled(rect, radius, fill);
    p.rect_stroke(rect.shrink(0.5), radius, Stroke::new(1.0, ACCENT.gamma_multiply(0.55)), StrokeKind::Inside);
}

/// Paints a square icon button and returns its response.
fn icon_button(ui: &mut egui::Ui, id: egui::Id, rect: Rect, icon: Icon, fill: Option<Color32>, enabled: bool) -> egui::Response {
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
    icons::draw(p, icon, rect.shrink(9.0), icon_color);
    resp
}

impl SnippyApp {
    pub(crate) fn bar_ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let capturing = matches!(self.phase, Phase::Capturing);
        let record = self.settings.record_mode;
        let sel_fill = if record { REC_RED } else { ACCENT };
        let mut hovered_tip: Option<(String, f32)> = None;
        let mut resize = false;

        egui::CentralPanel::default().frame(egui::Frame::NONE).show(ui, |ui| {
            let full = ui.max_rect();
            paint_glass_panel(ui.painter(), full, self.glass, RADIUS);
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
                    Item::Close => (Icon::Close, "Exit Snippy".into(), None),
                    Item::Status | Item::Pause | Item::Stop => continue,
                };

                let enabled = !capturing || matches!(item, Item::Close);
                let resp = icon_button(ui, id, r, icon, fill, enabled);
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
                        Item::Close => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
                        _ => {}
                    }
                }
                if item == Item::Timer && resp.secondary_clicked() {
                    self.settings.delay_secs = 0;
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
        egui::CentralPanel::default().frame(egui::Frame::NONE).show(ui, |ui| {
            let full = ui.max_rect();
            paint_glass_panel(ui.painter(), full, self.glass, RADIUS);
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
                        let resp = icon_button(ui, ui.scope_id().with(("rec", i)), r, icon, None, countdown.is_none());
                        if resp.hovered() {
                            hovered_tip = Some((tip.to_string(), r.center().y - full.min.y));
                        }
                        toggle_pause |= resp.clicked();
                    }
                    Item::Stop => {
                        let resp = icon_button(ui, ui.scope_id().with(("rec", i)), r, Icon::Stop, Some(REC_RED), true);
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
                self.tip = Some((text.clone(), now, false));
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
        let glass = self.glass;
        ctx.show_viewport_immediate(egui::ViewportId::from_hash_of("snippy_tooltip"), builder, |ui, _| {
            let r = ui.max_rect();
            paint_glass_panel(ui.painter(), r, glass, 6.0);
            ui.painter().text(r.center(), egui::Align2::CENTER_CENTER, &text, font.clone(), Color32::WHITE);
        });
        if let Some((_, _, styled)) = &mut self.tip {
            if !*styled {
                *styled = style_child_window(TOOLTIP_TITLE, true).is_some();
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
pub(crate) fn show_recording_border(ctx: &egui::Context, area: Rect, paused: bool) {
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
            .with_resizable(false)
            .with_active(false);
        ctx.show_viewport_immediate(egui::ViewportId::from_hash_of(("snippy_rec_border", i)), builder, |ui, _| {
            let rect = ui.max_rect();
            ui.painter().rect_filled(rect, 0.0, color);
        });
    }
}

/// Finds one of our secondary windows by title and applies the glass/rounded/capture-excluded
/// style. Returns whether glass is active, or `None` if the window wasn't found (yet).
pub(crate) fn style_child_window(title: &str, glass: bool) -> Option<bool> {
    platform::find_window(title).map(|h| platform::style_window(h, glass))
}
