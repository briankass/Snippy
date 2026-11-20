//! Global shortcuts: Print Screen and Win+Shift+S open Snippy instead of the Windows
//! Snipping Tool.
//!
//! Windows reserves Win+Shift+S for itself (RegisterHotKey can't take it), so this uses a
//! low-level keyboard hook that sees keys before the shell does and swallows the ones we
//! handle.
//!
//! Low-level hooks are fragile: Windows *silently removes* a hook whose callback doesn't
//! return quickly (LowLevelHooksTimeout), and it deprioritises background apps with no
//! visible window — exactly Snippy's state when it's hidden in the tray. So:
//! * the callback never blocks: it only `try_send`s into a bounded queue; a separate
//!   notifier thread wakes the UI;
//! * the hook thread runs at the highest priority and the process opts out of power
//!   throttling;
//! * a watchdog re-installs the hook every few seconds, so even if Windows drops it,
//!   shortcuts come back almost immediately.
//!
//! Limitation (by Windows design): low-level hooks don't see keys while a program running
//! as administrator has focus.

#![cfg_attr(not(windows), allow(dead_code))] // the hook itself is Windows-only

use crate::AppEvent;
use eframe::egui;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender, SyncSender};
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hotkey {
    PrintScreen,
    WinShiftS,
}

static PRINT_SCREEN: AtomicBool = AtomicBool::new(false);
static WIN_SHIFT_S: AtomicBool = AtomicBool::new(false);
/// Filled by the hook callback (never blocks: bounded + `try_send`).
static QUEUE: OnceLock<SyncSender<Hotkey>> = OnceLock::new();

/// Enables/disables each shortcut (cheap; the hook stays installed).
pub fn configure(print_screen: bool, win_shift_s: bool) {
    PRINT_SCREEN.store(print_screen, Ordering::Relaxed);
    WIN_SHIFT_S.store(win_shift_s, Ordering::Relaxed);
}

/// Called from the hook callback: must return immediately.
fn fire(h: Hotkey) {
    if let Some(q) = QUEUE.get() {
        let _ = q.try_send(h);
    }
}

/// Starts the notifier thread that turns queued hotkeys into app events + UI wake-ups.
/// Returns false if already started.
fn start_notifier(ctx: &egui::Context, tx: Sender<AppEvent>) -> bool {
    let (qtx, qrx) = mpsc::sync_channel::<Hotkey>(16);
    if QUEUE.set(qtx).is_err() {
        return false;
    }
    let ctx = ctx.clone();
    std::thread::Builder::new()
        .name("snippy-hotkey-notify".into())
        .spawn(move || {
            for h in qrx {
                if tx.send(AppEvent::Hotkey(h)).is_err() {
                    break;
                }
                ctx.request_repaint();
            }
        })
        .ok();
    true
}

#[cfg(windows)]
pub fn install(ctx: &egui::Context, tx: Sender<AppEvent>) {
    if !start_notifier(ctx, tx) {
        return; // already installed
    }
    imp::opt_out_of_power_throttling();
    std::thread::Builder::new()
        .name("snippy-hotkeys".into())
        .spawn(|| unsafe { imp::run() })
        .ok();
}

#[cfg(not(windows))]
pub fn install(ctx: &egui::Context, tx: Sender<AppEvent>) {
    start_notifier(ctx, tx);
}

#[cfg(windows)]
mod imp {
    use super::*;
    use windows::Win32::Foundation::{HINSTANCE, LPARAM, LRESULT, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::Threading::{
        GetCurrentProcess, GetCurrentThread, ProcessPowerThrottling, SetProcessInformation, SetThreadPriority,
        PROCESS_POWER_THROTTLING_CURRENT_VERSION, PROCESS_POWER_THROTTLING_EXECUTION_SPEED, PROCESS_POWER_THROTTLING_STATE,
        THREAD_PRIORITY_TIME_CRITICAL,
    };
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP,
        VIRTUAL_KEY, VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT, VK_SNAPSHOT,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, DispatchMessageW, GetMessageW, SetTimer, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx,
        HC_ACTION, HHOOK, KBDLLHOOKSTRUCT, LLKHF_INJECTED, MSG, WH_KEYBOARD_LL, WM_KEYDOWN, WM_SYSKEYDOWN, WM_TIMER,
    };

    /// How often the hook is re-installed, in case Windows silently removed it.
    const REINSTALL_MS: u32 = 10_000;

    /// True while we're swallowing an `S` press (so its key-up is swallowed too).
    static SWALLOWING_S: AtomicBool = AtomicBool::new(false);
    static SWALLOWING_PRTSC: AtomicBool = AtomicBool::new(false);

    /// Hidden in the tray, Snippy has no visible window and Windows 11 may throttle it,
    /// which can make the hook miss its deadline. Opt out of that.
    pub fn opt_out_of_power_throttling() {
        let state = PROCESS_POWER_THROTTLING_STATE {
            Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
            StateMask: 0, // 0 = "don't throttle execution speed"
        };
        unsafe {
            let _ = SetProcessInformation(
                GetCurrentProcess(),
                ProcessPowerThrottling,
                &state as *const _ as *const _,
                std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
            );
        }
    }

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

    /// The hook callback. Keep it tiny: no locks, no allocation, no blocking calls.
    unsafe extern "system" fn hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code == HC_ACTION as i32 {
            let kb = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
            let down = wparam.0 as u32 == WM_KEYDOWN || wparam.0 as u32 == WM_SYSKEYDOWN;
            let injected = kb.flags.0 & LLKHF_INJECTED.0 != 0;
            if !injected {
                // Print Screen (plain — Alt+PrtSc etc. keep their Windows behaviour)
                if kb.vkCode == VK_SNAPSHOT.0 as u32 && PRINT_SCREEN.load(Ordering::Relaxed) {
                    if down && !held(VK_MENU) && !held(VK_CONTROL) && !held(VK_SHIFT) {
                        if !SWALLOWING_PRTSC.swap(true, Ordering::Relaxed) {
                            fire(Hotkey::PrintScreen); // once per press, not per auto-repeat
                        }
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
                            fire(Hotkey::WinShiftS);
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

    unsafe fn install_hook(module: Option<HINSTANCE>) -> Option<HHOOK> {
        SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook), module, 0).ok()
    }

    pub unsafe fn run() {
        // Answer key events promptly even when the system is busy.
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL);
        let module: Option<HINSTANCE> = GetModuleHandleW(None).ok().map(Into::into);
        let mut current = install_hook(module);
        // A thread timer (no window): WM_TIMER arrives through this thread's message loop.
        SetTimer(None, 0, REINSTALL_MS, None);

        // Low-level hooks are called through this thread's message loop.
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if msg.message == WM_TIMER {
                // Install the new hook *before* removing the old one so no key slips
                // through in between (the newest hook runs first and swallows ours).
                if let Some(fresh) = install_hook(module) {
                    if let Some(old) = current.replace(fresh) {
                        let _ = UnhookWindowsHookEx(old);
                    }
                }
                continue;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
