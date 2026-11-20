//! System tray icon with a quick-actions menu (Windows).
//!
//! Left-click shows/hides the floating bar; right-click opens the menu.

#![cfg_attr(not(windows), allow(dead_code))] // the tray itself is Windows-only

use crate::AppEvent;
use eframe::egui;
use std::sync::mpsc::Sender;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayCmd {
    SnipRegion,
    SnipWindow,
    SnipScreen,
    CopyText,
    PickColor,
    Record,
    History,
    ToggleBar,
    Settings,
    Exit,
}

/// Decodes the embedded tray-icon artwork.
pub fn icon_rgba(png: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    let img = image::load_from_memory(png).ok()?.to_rgba8();
    let (w, h) = img.dimensions();
    Some((img.into_raw(), w, h))
}

#[cfg(windows)]
pub use imp::Tray;

#[cfg(windows)]
mod imp {
    use super::*;
    use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

    pub struct Tray {
        _icon: TrayIcon,
    }

    /// (id, label, separator before it)
    const ITEMS: [(&str, &str, bool); 10] = [
        ("snip_region", "Snip region", false),
        ("snip_window", "Snip window", false),
        ("snip_screen", "Snip full screen", false),
        ("copy_text", "Copy text from screen", true),
        ("pick_color", "Pick a colour", false),
        ("record", "Start recording…", true),
        ("history", "Recent captures", true),
        ("toggle_bar", "Show / hide the bar", false),
        ("settings", "Settings", false),
        ("exit", "Exit Snippy", true),
    ];

    fn cmd(id: &str) -> Option<TrayCmd> {
        Some(match id {
            "snip_region" => TrayCmd::SnipRegion,
            "snip_window" => TrayCmd::SnipWindow,
            "snip_screen" => TrayCmd::SnipScreen,
            "copy_text" => TrayCmd::CopyText,
            "pick_color" => TrayCmd::PickColor,
            "history" => TrayCmd::History,
            "record" => TrayCmd::Record,
            "toggle_bar" => TrayCmd::ToggleBar,
            "settings" => TrayCmd::Settings,
            "exit" => TrayCmd::Exit,
            _ => return None,
        })
    }

    impl Tray {
        /// Must be called on the UI (event-loop) thread.
        pub fn new(ctx: &egui::Context, tx: Sender<AppEvent>) -> Result<Self, String> {
            let menu = Menu::new();
            for (id, label, sep) in ITEMS.iter() {
                if *sep {
                    menu.append(&PredefinedMenuItem::separator()).map_err(|e| e.to_string())?;
                }
                menu.append(&MenuItem::with_id(*id, *label, true, None)).map_err(|e| e.to_string())?;
            }
            let (rgba, w, h) = icon_rgba(crate::TRAY_PNG).ok_or("bad tray icon")?;
            let icon = Icon::from_rgba(rgba, w, h).map_err(|e| e.to_string())?;
            let tray = TrayIconBuilder::new()
                .with_icon(icon)
                .with_tooltip("Snippy")
                .with_menu(Box::new(menu))
                .with_menu_on_left_click(false)
                .build()
                .map_err(|e| e.to_string())?;

            // Event handlers are global; forward into the app's channel and wake the UI.
            let (tx2, ctx2) = (tx.clone(), ctx.clone());
            MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
                if let Some(c) = cmd(e.id.as_ref()) {
                    let _ = tx2.send(AppEvent::Tray(c));
                    ctx2.request_repaint();
                }
            }));
            let ctx3 = ctx.clone();
            TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
                if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = e {
                    let _ = tx.send(AppEvent::Tray(TrayCmd::ToggleBar));
                    ctx3.request_repaint();
                }
            }));
            Ok(Self { _icon: tray })
        }
    }

    impl Drop for Tray {
        fn drop(&mut self) {
            MenuEvent::set_event_handler(None::<fn(MenuEvent)>);
            TrayIconEvent::set_event_handler(None::<fn(TrayIconEvent)>);
        }
    }
}

#[cfg(not(windows))]
pub struct Tray;

#[cfg(not(windows))]
impl Tray {
    pub fn new(_ctx: &egui::Context, _tx: Sender<AppEvent>) -> Result<Self, String> {
        Err("The tray icon is only available on Windows".into())
    }
}
