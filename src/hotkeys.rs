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
//! * raw keyboard input (a separate channel Windows never times out) watches for our keys:
//!   if raw input sees Print Screen / Win+Shift+S but the hook didn't, the hook was dropped
//!   and is re-installed on the spot (logged to the diagnostics log);
//! * a watchdog also re-installs the hook every couple of seconds.
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

/// Called from the hook callback: must return immediately. Returns whether it was queued.
fn fire(h: Hotkey) -> bool {
    QUEUE.get().is_some_and(|q| q.try_send(h).is_ok())
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
                crate::diag::log(&format!("shortcut: {h:?} -> waking Snippy"));
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
    imp::start_diagnostics();
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
    use std::sync::atomic::{AtomicIsize, AtomicU32};
    use windows::core::w;
    use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::System::SystemInformation::GetTickCount;
    use windows::Win32::UI::Input::{
        GetRawInputData, RegisterRawInputDevices, HRAWINPUT, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER, RIDEV_INPUTSINK,
        RID_INPUT, RIM_TYPEKEYBOARD,
    };
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
        CallNextHookEx, CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, RegisterClassW, SetTimer,
        SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, HC_ACTION, HHOOK, HWND_MESSAGE, KBDLLHOOKSTRUCT,
        LLKHF_INJECTED, MSG, RI_KEY_BREAK, WH_KEYBOARD_LL, WINDOW_EX_STYLE, WINDOW_STYLE, WM_INPUT, WM_KEYDOWN,
        WM_SYSKEYDOWN, WM_TIMER, WNDCLASSW,
    };

    /// How often the hook is re-installed anyway, in case Windows silently removed it.
    const REINSTALL_MS: u32 = 2_000;
    /// If raw input sees one of our keys and the hook hasn't seen it within this long, the
    /// hook was dropped.
    const MISS_WINDOW_MS: u32 = 400;

    /// The installed hook (an `HHOOK`), owned by the hook thread.
    static CURRENT: AtomicIsize = AtomicIsize::new(0);
    /// `GetTickCount` when the hook last saw one of our keys go down.
    static HOOK_SAW: AtomicU32 = AtomicU32::new(0);
    static MODULE: AtomicIsize = AtomicIsize::new(0);

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
            let is_prtsc = kb.vkCode == VK_SNAPSHOT.0 as u32;
            let win = held(VK_LWIN) || held(VK_RWIN);
            // Only the two shortcuts are ever looked at (and S only while Win is held).
            let relevant = is_prtsc || (kb.vkCode == b'S' as u32 && win);
            if !relevant {
                return CallNextHookEx(None, code, wparam, lparam);
            }
            let m = mods();
            let mut swallow = false;
            let mut fired = None;
            if !injected {
                if down {
                    HOOK_SAW.store(GetTickCount(), Ordering::Relaxed);
                }
                if is_prtsc && PRINT_SCREEN.load(Ordering::Relaxed) {
                    // Plain Print Screen (Alt+PrtSc etc. keep their Windows behaviour).
                    if down && m & (M_ALT | M_CTRL | M_SHIFT) == 0 {
                        if !SWALLOWING_PRTSC.swap(true, Ordering::Relaxed) {
                            fired = Some(fire(Hotkey::PrintScreen)); // once per press
                        }
                        swallow = true;
                    } else if !down && SWALLOWING_PRTSC.swap(false, Ordering::Relaxed) {
                        swallow = true;
                    }
                } else if !is_prtsc && WIN_SHIFT_S.load(Ordering::Relaxed) {
                    if down && m & M_SHIFT != 0 && m & (M_CTRL | M_ALT) == 0 {
                        if !SWALLOWING_S.swap(true, Ordering::Relaxed) {
                            fired = Some(fire(Hotkey::WinShiftS));
                        }
                        suppress_start_menu();
                        swallow = true;
                    } else if !down && SWALLOWING_S.swap(false, Ordering::Relaxed) {
                        swallow = true;
                    }
                }
            }
            record(KeyRecord { source: Source::Hook, prtsc: is_prtsc, down, mods: m, injected, swallowed: swallow, fired });
            if swallow {
                return LRESULT(1);
            }
        }
        CallNextHookEx(None, code, wparam, lparam)
    }

    const M_SHIFT: u8 = 1;
    const M_CTRL: u8 = 2;
    const M_ALT: u8 = 4;
    const M_WIN: u8 = 8;

    fn mods() -> u8 {
        let mut m = 0;
        if held(VK_SHIFT) {
            m |= M_SHIFT;
        }
        if held(VK_CONTROL) {
            m |= M_CTRL;
        }
        if held(VK_MENU) {
            m |= M_ALT;
        }
        if held(VK_LWIN) || held(VK_RWIN) {
            m |= M_WIN;
        }
        m
    }

    // ── Diagnostics: the hook and the raw-input watcher queue small records (never
    //    blocking); a background thread writes them and watches for the Snipping Tool.

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Source {
        Hook,
        Raw,
    }

    #[derive(Clone, Copy)]
    struct KeyRecord {
        source: Source,
        prtsc: bool,
        down: bool,
        mods: u8,
        injected: bool,
        swallowed: bool,
        /// `Some(true)` = handed to Snippy; `Some(false)` = queue full; `None` = not fired.
        fired: Option<bool>,
    }

    static RECORDS: OnceLock<SyncSender<KeyRecord>> = OnceLock::new();

    fn record(r: KeyRecord) {
        if let Some(q) = RECORDS.get() {
            let _ = q.try_send(r);
        }
    }

    fn mods_text(m: u8) -> String {
        let mut v = Vec::new();
        for (bit, name) in [(M_WIN, "Win"), (M_CTRL, "Ctrl"), (M_ALT, "Alt"), (M_SHIFT, "Shift")] {
            if m & bit != 0 {
                v.push(name);
            }
        }
        if v.is_empty() { "none".into() } else { v.join("+") }
    }

    /// Starts the thread that logs key records and checks whether the Windows Snipping
    /// Tool shows up after one of our shortcuts.
    pub fn start_diagnostics() {
        let (tx, rx) = mpsc::sync_channel::<KeyRecord>(64);
        if RECORDS.set(tx).is_err() {
            return;
        }
        std::thread::Builder::new()
            .name("snippy-key-diag".into())
            .spawn(move || {
                let mut last_check = std::time::Instant::now() - std::time::Duration::from_secs(10);
                for r in rx {
                    let key = if r.prtsc { "Print Screen" } else { "S (Win held)" };
                    let line = match r.source {
                        Source::Hook => format!(
                            "key: hook  {key} {}  mods={}{}  -> {}{}",
                            if r.down { "down" } else { "up" },
                            mods_text(r.mods),
                            if r.injected { " (injected)" } else { "" },
                            if r.swallowed { "swallowed" } else { "PASSED THROUGH to Windows" },
                            match r.fired {
                                Some(true) => ", sent to Snippy",
                                Some(false) => ", NOT sent (queue full)",
                                None => "",
                            }
                        ),
                        Source::Raw => format!("key: raw   {key} down  mods={}", mods_text(r.mods)),
                    };
                    crate::diag::log(&line);
                    // On a press: is the Snipping Tool running, and does it pop up?
                    if r.down && r.source == Source::Raw && last_check.elapsed().as_millis() > 1500 {
                        last_check = std::time::Instant::now();
                        let before_pids = crate::diag::snipping_tool::pids().len();
                        let before = crate::diag::snipping_tool::visible_windows();
                        crate::diag::log(&format!(
                            "snipping tool: {} process(es) running, {} visible window(s) at the press",
                            before_pids, before
                        ));
                        std::thread::spawn(move || {
                            // (elapsed ms, wait before this check)
                            for (ms, wait) in [(250u64, 250u64), (750, 500), (2000, 1250)] {
                                std::thread::sleep(std::time::Duration::from_millis(wait));
                                let now = crate::diag::snipping_tool::visible_windows();
                                if now > before {
                                    crate::diag::log(&format!(
                                        "snipping tool: WINDOW APPEARED ~{ms} ms after {key} ({before} -> {now} visible)"
                                    ));
                                    return;
                                }
                            }
                        });
                    }
                }
            })
            .ok();
    }

    fn module() -> Option<HINSTANCE> {
        let m = MODULE.load(Ordering::Relaxed);
        (m != 0).then_some(HINSTANCE(m as *mut _))
    }

    /// Installs a fresh hook *before* removing the old one, so no key slips through in
    /// between (the newest hook runs first and swallows ours).
    unsafe fn reinstall() -> bool {
        match SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook), module(), 0) {
            Ok(fresh) => {
                let old = CURRENT.swap(fresh.0 as isize, Ordering::Relaxed);
                if old != 0 {
                    let _ = UnhookWindowsHookEx(HHOOK(old as *mut _));
                }
                true
            }
            Err(e) => {
                crate::diag::log(&format!("keyboard hook: install failed: {e}"));
                false
            }
        }
    }

    /// Is `vk` (going down now) one of the shortcuts we're supposed to catch?
    fn is_our_shortcut(vk: u16) -> bool {
        (vk == VK_SNAPSHOT.0 && PRINT_SCREEN.load(Ordering::Relaxed) && !held(VK_MENU) && !held(VK_CONTROL) && !held(VK_SHIFT))
            || (vk == b'S' as u16
                && WIN_SHIFT_S.load(Ordering::Relaxed)
                && (held(VK_LWIN) || held(VK_RWIN))
                && held(VK_SHIFT)
                && !held(VK_CONTROL)
                && !held(VK_MENU))
    }

    /// Raw input watcher: sees every key regardless of hooks. If one of our shortcuts went
    /// by and the hook didn't see it, Windows dropped the hook: put it back immediately.
    unsafe extern "system" fn watcher_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if msg == WM_INPUT {
            let mut raw = RAWINPUT::default();
            let mut size = std::mem::size_of::<RAWINPUT>() as u32;
            let got = GetRawInputData(
                HRAWINPUT(lparam.0 as *mut _),
                RID_INPUT,
                Some(&mut raw as *mut _ as *mut _),
                &mut size,
                std::mem::size_of::<RAWINPUTHEADER>() as u32,
            );
            if got != u32::MAX && raw.header.dwType == RIM_TYPEKEYBOARD.0 {
                let kb = raw.data.keyboard;
                let key_down = (kb.Flags as u32 & RI_KEY_BREAK) == 0;
                let win = held(VK_LWIN) || held(VK_RWIN);
                if key_down && (kb.VKey == VK_SNAPSHOT.0 || (kb.VKey == b'S' as u16 && win)) {
                    record(KeyRecord {
                        source: Source::Raw,
                        prtsc: kb.VKey == VK_SNAPSHOT.0,
                        down: true,
                        mods: mods(),
                        injected: false,
                        swallowed: false,
                        fired: None,
                    });
                }
                if key_down && is_our_shortcut(kb.VKey) {
                    let since = GetTickCount().wrapping_sub(HOOK_SAW.load(Ordering::Relaxed));
                    if since > MISS_WINDOW_MS {
                        let name = if kb.VKey == VK_SNAPSHOT.0 { "Print Screen" } else { "Win+Shift+S" };
                        let ok = reinstall();
                        crate::diag::log(&format!(
                            "keyboard hook missed {name} (Windows had dropped it); re-installed: {}",
                            if ok { "ok" } else { "FAILED" }
                        ));
                    }
                }
            }
        }
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }

    /// A hidden message-only window registered for raw keyboard input.
    unsafe fn create_watcher() {
        let class = w!("SnippyHotkeyWatcher");
        let wc = WNDCLASSW { lpfnWndProc: Some(watcher_proc), hInstance: module().unwrap_or_default(), lpszClassName: class, ..Default::default() };
        RegisterClassW(&wc);
        let Ok(hwnd) = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            w!(""),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            module(),
            None,
        ) else {
            crate::diag::log("keyboard hook: couldn't create the raw-input watcher window");
            return;
        };
        let dev = RAWINPUTDEVICE { usUsagePage: 0x01, usUsage: 0x06, dwFlags: RIDEV_INPUTSINK, hwndTarget: hwnd };
        if let Err(e) = RegisterRawInputDevices(&[dev], std::mem::size_of::<RAWINPUTDEVICE>() as u32) {
            crate::diag::log(&format!("keyboard hook: raw input registration failed: {e}"));
        }
    }

    pub unsafe fn run() {
        // Answer key events promptly even when the system is busy.
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL);
        if let Ok(m) = GetModuleHandleW(None) {
            MODULE.store(m.0 as isize, Ordering::Relaxed);
        }
        if reinstall() {
            crate::diag::log("keyboard hook: installed");
        }
        create_watcher();
        // A thread timer (no window): WM_TIMER arrives through this thread's message loop.
        SetTimer(None, 0, REINSTALL_MS, None);

        // Low-level hooks are called through this thread's message loop.
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if msg.message == WM_TIMER && msg.hwnd.0.is_null() {
                reinstall();
                continue;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
