//! Vector icons, drawn with egui's painter on a 24×24 design grid.
//!
//! Drawing icons as shapes (instead of emoji/font glyphs) means they never show up as
//! "tofu" boxes, look identical on every machine, and stay sharp at any display scaling.

use eframe::egui::{Color32, Painter, Pos2, Rect, Shape, Stroke, Vec2};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Icon {
    Region,
    Window,
    Screen,
    Video,
    Timer,
    Speaker,
    SpeakerOff,
    Mic,
    MicOff,
    Close,
    Pause,
    Play,
    Stop,
    Grip,
    Edit,
    Copy,
    Save,
    Folder,
    Trash,
    Gear,
    Refresh,
    Brush,
    Sun,
    Grid,
    Power,
    Pointer,
    Tray,
    Keyboard,
    HideToTray,
}

/// Draws `icon` centred in `rect` (scaled to fit), in `color`.
pub fn draw(p: &Painter, icon: Icon, rect: Rect, color: Color32) {
    let size = rect.width().min(rect.height());
    let s = size / 24.0;
    let o = rect.center() - Vec2::splat(size / 2.0);
    let pt = |x: f32, y: f32| o + Vec2::new(x, y) * s;
    let st = Stroke::new(1.7 * s.max(0.75), color);
    let line = |pts: &[(f32, f32)]| {
        p.add(Shape::line(pts.iter().map(|&(x, y)| pt(x, y)).collect(), st));
    };
    let closed = |pts: &[(f32, f32)]| {
        p.add(Shape::closed_line(pts.iter().map(|&(x, y)| pt(x, y)).collect(), st));
    };
    let rrect = |x0: f32, y0: f32, x1: f32, y1: f32, r: f32| {
        p.rect_stroke(Rect::from_min_max(pt(x0, y0), pt(x1, y1)), r * s, st, eframe::egui::StrokeKind::Middle);
    };
    let arc = |cx: f32, cy: f32, r: f32, a0: f32, a1: f32| {
        let n = 16;
        let pts: Vec<Pos2> = (0..=n)
            .map(|i| {
                let a = (a0 + (a1 - a0) * i as f32 / n as f32).to_radians();
                pt(cx + r * a.cos(), cy + r * a.sin())
            })
            .collect();
        p.add(Shape::line(pts, st));
    };
    let fill_poly = |pts: &[(f32, f32)]| {
        p.add(Shape::convex_polygon(pts.iter().map(|&(x, y)| pt(x, y)).collect(), color, Stroke::NONE));
    };

    match icon {
        Icon::Region => {
            let pts: Vec<Pos2> = [(4.0, 5.0), (20.0, 5.0), (20.0, 19.0), (4.0, 19.0), (4.0, 5.0)]
                .iter()
                .map(|&(x, y)| pt(x, y))
                .collect();
            p.extend(Shape::dashed_line(&pts, st, 2.6 * s, 2.0 * s));
        }
        Icon::Window => {
            rrect(3.0, 5.0, 21.0, 19.0, 2.5);
            line(&[(3.0, 9.0), (21.0, 9.0)]);
        }
        Icon::Screen => {
            line(&[(4.0, 9.0), (4.0, 4.0), (9.0, 4.0)]);
            line(&[(15.0, 4.0), (20.0, 4.0), (20.0, 9.0)]);
            line(&[(20.0, 15.0), (20.0, 20.0), (15.0, 20.0)]);
            line(&[(9.0, 20.0), (4.0, 20.0), (4.0, 15.0)]);
        }
        Icon::Video => {
            rrect(2.5, 6.5, 15.0, 17.5, 2.5);
            closed(&[(15.0, 10.5), (21.5, 7.0), (21.5, 17.0), (15.0, 13.5)]);
        }
        Icon::Timer => {
            let c = (12.0, 13.5);
            arc(c.0, c.1, 7.5, 0.0, 360.0);
            line(&[(12.0, 13.5), (12.0, 9.5)]);
            line(&[(9.5, 3.0), (14.5, 3.0)]);
            line(&[(12.0, 3.0), (12.0, 6.0)]);
        }
        Icon::Speaker | Icon::SpeakerOff => {
            closed(&[(3.5, 9.5), (7.5, 9.5), (12.5, 5.0), (12.5, 19.0), (7.5, 14.5), (3.5, 14.5)]);
            if icon == Icon::Speaker {
                arc(12.5, 12.0, 4.0, -45.0, 45.0);
                arc(12.5, 12.0, 7.5, -45.0, 45.0);
            } else {
                line(&[(16.0, 9.0), (21.5, 15.0)]);
                line(&[(21.5, 9.0), (16.0, 15.0)]);
            }
        }
        Icon::Mic | Icon::MicOff => {
            rrect(9.0, 3.0, 15.0, 14.5, 3.0);
            arc(12.0, 11.5, 6.0, 0.0, 180.0);
            line(&[(12.0, 17.5), (12.0, 21.0)]);
            line(&[(8.5, 21.0), (15.5, 21.0)]);
            if icon == Icon::MicOff {
                line(&[(4.0, 3.5), (20.0, 20.5)]);
            }
        }
        Icon::Close => {
            line(&[(6.5, 6.5), (17.5, 17.5)]);
            line(&[(17.5, 6.5), (6.5, 17.5)]);
        }
        Icon::Pause => {
            for x in [6.5, 13.5] {
                p.rect_filled(Rect::from_min_max(pt(x, 5.0), pt(x + 4.0, 19.0)), 1.2 * s, color);
            }
        }
        Icon::Play => fill_poly(&[(7.5, 4.5), (19.5, 12.0), (7.5, 19.5)]),
        Icon::Stop => {
            p.rect_filled(Rect::from_min_max(pt(6.0, 6.0), pt(18.0, 18.0)), 2.5 * s, color);
        }
        Icon::Grip => {
            for x in [8.0, 12.0, 16.0] {
                for y in [10.0, 14.0] {
                    p.circle_filled(pt(x, y), 1.15 * s, color);
                }
            }
        }
        Icon::Edit => {
            closed(&[(15.0, 4.5), (19.5, 9.0), (9.0, 19.5), (4.5, 19.5), (4.5, 15.0)]);
            line(&[(12.5, 7.0), (17.0, 11.5)]);
        }
        Icon::Copy => {
            rrect(8.5, 8.5, 19.5, 19.5, 2.0);
            line(&[(5.0, 15.5), (5.0, 6.5), (6.5, 5.0), (15.5, 5.0)]);
        }
        Icon::Save => {
            closed(&[(4.0, 4.0), (16.5, 4.0), (20.0, 7.5), (20.0, 20.0), (4.0, 20.0)]);
            line(&[(8.0, 4.0), (8.0, 9.0), (15.0, 9.0), (15.0, 4.0)]);
            rrect(7.5, 13.0, 16.5, 20.0, 1.0);
        }
        Icon::Folder => {
            closed(&[(3.0, 6.0), (9.0, 6.0), (11.0, 8.5), (21.0, 8.5), (21.0, 19.0), (3.0, 19.0)]);
        }
        Icon::Gear => {
            for k in 0..8 {
                let a = (k as f32 * 45.0).to_radians();
                line(&[(12.0 + 6.5 * a.cos(), 12.0 + 6.5 * a.sin()), (12.0 + 9.5 * a.cos(), 12.0 + 9.5 * a.sin())]);
            }
            arc(12.0, 12.0, 6.5, 0.0, 360.0);
            arc(12.0, 12.0, 2.8, 0.0, 360.0);
        }
        Icon::Refresh => {
            arc(12.0, 12.0, 7.5, -60.0, 250.0);
            line(&[(15.6, 2.8), (15.8, 5.6), (13.0, 6.2)]);
        }
        Icon::Brush => {
            line(&[(20.0, 4.0), (11.0, 13.0)]);
            closed(&[(11.0, 13.0), (8.0, 13.5), (5.5, 16.5), (4.0, 20.0), (7.5, 18.5), (10.5, 16.0), (11.0, 13.0)]);
        }
        Icon::Sun => {
            arc(12.0, 12.0, 4.0, 0.0, 360.0);
            for k in 0..8 {
                let a = (k as f32 * 45.0).to_radians();
                line(&[(12.0 + 6.5 * a.cos(), 12.0 + 6.5 * a.sin()), (12.0 + 8.5 * a.cos(), 12.0 + 8.5 * a.sin())]);
            }
        }
        Icon::Grid => {
            rrect(4.0, 4.0, 10.5, 10.5, 2.0);
            rrect(13.5, 4.0, 20.0, 10.5, 2.0);
            rrect(4.0, 13.5, 10.5, 20.0, 2.0);
            rrect(13.5, 13.5, 20.0, 20.0, 2.0);
        }
        Icon::Power => {
            arc(12.0, 13.0, 7.5, -55.0, 235.0);
            line(&[(12.0, 3.0), (12.0, 11.0)]);
        }
        Icon::Pointer => {
            closed(&[(6.0, 3.5), (6.0, 19.0), (10.0, 15.5), (12.8, 21.0), (15.0, 20.0), (12.3, 14.5), (17.5, 14.5)]);
        }
        Icon::Tray => {
            // taskbar with a small "up" chevron
            line(&[(3.0, 18.0), (21.0, 18.0)]);
            line(&[(8.0, 11.0), (12.0, 7.0), (16.0, 11.0)]);
            for x in [6.0, 12.0, 18.0] {
                p.circle_filled(pt(x, 14.6), 1.2 * s, color);
            }
        }
        Icon::HideToTray => {
            // arrow dropping into a tray
            line(&[(12.0, 3.5), (12.0, 13.5)]);
            line(&[(7.5, 9.5), (12.0, 14.0), (16.5, 9.5)]);
            line(&[(4.0, 14.5), (4.0, 19.5), (20.0, 19.5), (20.0, 14.5)]);
        }
        Icon::Keyboard => {
            rrect(2.5, 6.0, 21.5, 18.0, 2.5);
            for x in [6.0, 9.5, 13.0, 16.5] {
                p.circle_filled(pt(x, 10.0), 0.95 * s, color);
            }
            line(&[(7.0, 14.0), (17.0, 14.0)]);
        }
        Icon::Trash => {
            line(&[(4.0, 6.5), (20.0, 6.5)]);
            line(&[(9.5, 6.5), (9.5, 4.0), (14.5, 4.0), (14.5, 6.5)]);
            closed(&[(6.0, 6.5), (7.0, 20.0), (17.0, 20.0), (18.0, 6.5)]);
        }
    }
}
