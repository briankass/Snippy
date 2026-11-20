//! Global shortcuts: Print Screen and Win+Shift+S open Snippy instead of the Windows
//! Snipping Tool.
//!
//! Windows reserves Win+Shift+S for itself (RegisterHotKey can't take it), so this uses a
//! low-level keyboard hook that sees keys before the shell does and swallows the ones we
//! handle. The hook runs on its own thread with its own message loop, so a busy UI never
//! delays typing anywhere else.
//!
//! Limitation (by Windows design): low-level hooks don't see keys while a program running
//! as administrator has focus.

#![cfg_attr(not(windows), allow(dead_code))] // the hook itself is Windows-only

use crate::AppEvent;
use eframe::egui;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Mutex, OnceLock};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hotkey {
    PrintScreen,
    WinShiftS,
}

static PRINT_SCREEN: AtomicBool = AtomicBool::new(false);
static WIN_SHIFT_S: AtomicBool = AtomicBool::new(false);
static SINK: OnceLock<Mutex<(Sender<AppEvent>, egui::Context)>> = OnceLock::new();

/// Enables/disables each shortcut (cheap; the hook stays installed).
pub fn configure(print_screen: bool, win_shift_s: bool) {
    PRINT_SCREEN.store(print_screen, Ordering::Relaxed);
    WIN_SHIFT_S.store(win_shift_s, Ordering::Relaxed);
}

fn fire(h: Hotkey) {
    if let Some(m) = SINK.get() {
        if let Ok(g) = m.lock() {
            let _ = g.0.send(AppEvent::Hotkey(h));
            g.1.request_repaint();
        }
    }
}

#[cfg(windows)]
pub fn install(ctx: &egui::Context, tx: Sender<AppEvent>) {
    if SINK.set(Mutex::new((tx, ctx.clone()))).is_err() {
        return; // already installed
    }
    std::thread::Builder::new()
        .name("snippy-hotkeys".into())
        .spawn(|| unsafe { imp::run() })
        .ok();
}

#[cfg(not(windows))]
pub fn install(ctx: &egui::Context, tx: Sender<AppEvent>) {
    let _ = SINK.set(Mutex::new((tx, ctx.clone())));
}

#[cfg(windows)]
mod imp {
    use super::*;
    use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP,
        VIRTUAL_KEY, VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT, VK_SNAPSHOT,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, DispatchMessageW, GetMessageW, SetWindowsHookExW, TranslateMessage, HC_ACTION, KBDLLHOOKSTRUCT,
        LLKHF_INJECTED, MSG, WH_KEYBOARD_LL, WM_KEYDOWN, WM_SYSKEYDOWN,
    };

    /// True while we're swallowing an `S` press (so its key-up is swallowed too).
    static SWALLOWING_S: AtomicBool = AtomicBool::new(false);
    static SWALLOWING_PRTSC: AtomicBool = AtomicBool::new(false);

    fn held(vk: VIRTUAL_KEY) -> bool {
        unsafe { (GetAsyncKeyState(vk.0 as i32) as u16) & 0x8000 != 0 }
    }

    /// Releasing Win after a swallowed key would open the Start menu (the shell thinks Win
    /// was tapped alone). A harmless dummy key press in between prevents that.
    fn suppress_start_menu() {
        const VK_DUMMY: VIRTUAL_KEY = VIRTUAL_KEY(0xFF);
        let key = |flags: KEYBD_EVENT_FLAGS| INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: VK_DUMMY, wScan: 0, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
        };
        unsafe {
            SendInput(&[key(KEYBD_EVENT_FLAGS(0)), key(KEYEVENTF_KEYUP)], std::mem::size_of::<INPUT>() as i32);
        }
    }

    unsafe extern "system" fn hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code == HC_ACTION as i32 {
            let kb = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
            let down = wparam.0 as u32 == WM_KEYDOWN || wparam.0 as u32 == WM_SYSKEYDOWN;
            let injected = kb.flags.0 & LLKHF_INJECTED.0 != 0;
            if !injected {
                // Print Screen (plain — Alt+PrtSc etc. keep their Windows behaviour)
                if kb.vkCode == VK_SNAPSHOT.0 as u32 && PRINT_SCREEN.load(Ordering::Relaxed) {
                    if down && !held(VK_MENU) && !held(VK_CONTROL) && !held(VK_SHIFT) {
                        SWALLOWING_PRTSC.store(true, Ordering::Relaxed);
                        fire(Hotkey::PrintScreen);
                        return LRESULT(1);
                    }
                    if !down && SWALLOWING_PRTSC.swap(false, Ordering::Relaxed) {
                        return LRESULT(1);
                    }
                }
                // Win+Shift+S
                if kb.vkCode == b'S' as u32 && WIN_SHIFT_S.load(Ordering::Relaxed) {
                    if down && (held(VK_LWIN) || held(VK_RWIN)) && held(VK_SHIFT) && !held(VK_CONTROL) && !held(VK_MENU) {
                        if !SWALLOWING_S.swap(true, Ordering::Relaxed) {
                            fire(Hotkey::WinShiftS); // once per press, not per auto-repeat
                        }
                        suppress_start_menu();
                        return LRESULT(1);
                    }
                    if !down && SWALLOWING_S.swap(false, Ordering::Relaxed) {
                        return LRESULT(1);
                    }
                }
            }
        }
        CallNextHookEx(None, code, wparam, lparam)
    }

    pub unsafe fn run() {
        let module = GetModuleHandleW(None).ok();
        let Ok(_hook) = SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook), module.map(Into::into), 0) else {
            return;
        };
        // Low-level hooks are called through this thread's message loop.
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
