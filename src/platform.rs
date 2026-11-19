//! OS integration: glass backdrops, rounded corners, capture exclusion, work area, known
//! folders, "start with Windows". Everything degrades gracefully: on non-Windows platforms
//! (or old Windows versions) these are no-ops and the UI falls back to an opaque look.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// A native window handle, as an integer so it can cross platform boundaries.
pub type Hwnd = isize;

#[derive(Clone, Copy, Debug)]
pub enum Folder {
    Pictures,
    Videos,
}

/// Physical-pixel rectangle `[left, top, right, bottom]`.
pub type PxRect = [i32; 4];

/// How the window background is rendered by Windows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Backdrop {
    /// Blur-behind via `SetWindowCompositionAttribute`. Stays on when the window isn't
    /// focused, which a floating toolbar almost never is. Windows 10 and 11.
    Blur,
    /// Acrylic (blur + noise) via the same API. Can make dragging feel sluggish.
    Acrylic,
    /// Windows 11 22H2+ system backdrop. Official API, but Windows only shows it while the
    /// window is focused; otherwise it's a solid colour.
    System,
    /// No glass: a solid dark panel.
    Off,
}

impl Backdrop {
    pub const ALL: [Backdrop; 4] = [Backdrop::Blur, Backdrop::Acrylic, Backdrop::System, Backdrop::Off];

    pub fn label(self) -> &'static str {
        match self {
            Backdrop::Blur => "Frosted blur (recommended)",
            Backdrop::Acrylic => "Acrylic",
            Backdrop::System => "Windows 11 backdrop",
            Backdrop::Off => "Off (solid)",
        }
    }

    pub fn help(self) -> &'static str {
        match self {
            Backdrop::Blur => "Blurs whatever is behind Snippy, always, even when it isn't the active window.",
            Backdrop::Acrylic => "Blur with a subtle grain. On some PCs dragging the bar can feel slow.",
            Backdrop::System => "Windows 11's own acrylic. Windows only shows it while Snippy is focused, so the bar usually looks solid.",
            Backdrop::Off => "A solid dark panel. Use this if glass looks wrong on your PC.",
        }
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use windows::core::{s, w, BOOL, HSTRING, PCWSTR};
    use windows::Win32::Foundation::{ERROR_SUCCESS, HWND, POINT};
    use windows::Win32::Graphics::Dwm::{
        DwmExtendFrameIntoClientArea, DwmSetWindowAttribute, DWMSBT_NONE, DWMSBT_TRANSIENTWINDOW, DWMWA_SYSTEMBACKDROP_TYPE,
        DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
    };
    use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST};
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
    use windows::Win32::System::Registry::{
        RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ,
    };
    use windows::Win32::UI::Controls::MARGINS;
    use windows::Win32::UI::Shell::{FOLDERID_Pictures, FOLDERID_Videos, SHGetKnownFolderPath, KF_FLAG_DEFAULT};
    use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, SetWindowDisplayAffinity, WDA_EXCLUDEFROMCAPTURE};

    fn hwnd(h: Hwnd) -> HWND {
        HWND(h as *mut _)
    }

    pub fn find_window(title: &str) -> Option<Hwnd> {
        let t = HSTRING::from(title);
        unsafe { FindWindowW(PCWSTR::null(), &t) }.ok().map(|h| h.0 as Hwnd).filter(|&h| h != 0)
    }

    unsafe fn set_attr<T>(h: HWND, attr: windows::Win32::Graphics::Dwm::DWMWINDOWATTRIBUTE, value: &T) -> bool {
        DwmSetWindowAttribute(h, attr, value as *const T as *const _, std::mem::size_of::<T>() as u32).is_ok()
    }

    // ── SetWindowCompositionAttribute: undocumented but stable since Windows 10; loaded at
    //    runtime because it isn't in every import library.
    #[repr(C)]
    struct AccentPolicy {
        state: u32,
        flags: u32,
        gradient: u32,
        animation: u32,
    }
    #[repr(C)]
    struct CompositionData {
        attrib: u32,
        data: *mut std::ffi::c_void,
        size: usize,
    }
    type SetWcaFn = unsafe extern "system" fn(HWND, *mut CompositionData) -> BOOL;
    const WCA_ACCENT_POLICY: u32 = 19;
    const ACCENT_DISABLED: u32 = 0;
    const ACCENT_ENABLE_BLURBEHIND: u32 = 3;
    const ACCENT_ENABLE_ACRYLICBLURBEHIND: u32 = 4;

    unsafe fn set_accent(h: HWND, state: u32, gradient_abgr: u32) -> bool {
        let Ok(user32) = GetModuleHandleW(w!("user32.dll")) else { return false };
        let Some(f) = GetProcAddress(user32, s!("SetWindowCompositionAttribute")) else { return false };
        let f: SetWcaFn = std::mem::transmute(f);
        let mut policy = AccentPolicy { state, flags: 2, gradient: gradient_abgr, animation: 0 };
        let mut data = CompositionData {
            attrib: WCA_ACCENT_POLICY,
            data: &mut policy as *mut _ as *mut _,
            size: std::mem::size_of::<AccentPolicy>(),
        };
        f(h, &mut data).as_bool()
    }

    /// Hides the window from screenshots/recordings, makes it dark + rounded, and applies
    /// `backdrop`. Returns whether a glass backdrop is active.
    pub fn style_window(h: Hwnd, backdrop: Backdrop) -> bool {
        let h = hwnd(h);
        unsafe {
            // Windows 10 2004+: invisible to screen capture (so the bar isn't in videos).
            let _ = SetWindowDisplayAffinity(h, WDA_EXCLUDEFROMCAPTURE);
            let _ = set_attr(h, DWMWA_USE_IMMERSIVE_DARK_MODE, &1u32);
            let _ = set_attr(h, DWMWA_WINDOW_CORNER_PREFERENCE, &DWMWCP_ROUND);

            // Reset whatever was applied before (style can change at runtime).
            let _ = set_attr(h, DWMWA_SYSTEMBACKDROP_TYPE, &DWMSBT_NONE);
            let _ = set_accent(h, ACCENT_DISABLED, 0);
            let zero = MARGINS::default();
            let _ = DwmExtendFrameIntoClientArea(h, &zero);

            match backdrop {
                Backdrop::Off => false,
                Backdrop::Blur => set_accent(h, ACCENT_ENABLE_BLURBEHIND, 0),
                // The gradient needs non-zero alpha or some builds draw nothing; our own
                // painted tint goes on top anyway.
                Backdrop::Acrylic => set_accent(h, ACCENT_ENABLE_ACRYLICBLURBEHIND, 0x0100_0000),
                Backdrop::System => {
                    if !set_attr(h, DWMWA_SYSTEMBACKDROP_TYPE, &DWMSBT_TRANSIENTWINDOW) {
                        return false;
                    }
                    let all = MARGINS { cxLeftWidth: -1, cxRightWidth: -1, cyTopHeight: -1, cyBottomHeight: -1 };
                    DwmExtendFrameIntoClientArea(h, &all).is_ok()
                }
            }
        }
    }

    /// The usable area (excluding the taskbar) of the monitor containing a physical point.
    pub fn work_area(x: i32, y: i32) -> Option<PxRect> {
        unsafe {
            let mon = MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST);
            let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
            GetMonitorInfoW(mon, &mut info).as_bool().then(|| {
                let r = info.rcWork;
                [r.left, r.top, r.right, r.bottom]
            })
        }
    }

    pub fn known_folder(f: Folder) -> Option<PathBuf> {
        let id = match f {
            Folder::Pictures => FOLDERID_Pictures,
            Folder::Videos => FOLDERID_Videos,
        };
        unsafe {
            let p = SHGetKnownFolderPath(&id, KF_FLAG_DEFAULT, None).ok()?;
            let s = p.to_string().ok();
            CoTaskMemFree(Some(p.0 as *const _));
            s.map(PathBuf::from)
        }
    }

    // ── Start with Windows: HKCU\...\Run\Snippy = "<path to exe>"
    const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
    const RUN_VALUE: PCWSTR = w!("Snippy");

    fn run_command() -> Result<String, String> {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        Ok(format!("\"{}\"", exe.display()))
    }

    fn read_run_value() -> Option<String> {
        unsafe {
            let mut buf = [0u16; 1024];
            let mut len = (buf.len() * 2) as u32;
            let r = RegGetValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE, RRF_RT_REG_SZ, None, Some(buf.as_mut_ptr() as *mut _), Some(&mut len));
            (r == ERROR_SUCCESS).then(|| String::from_utf16_lossy(&buf[..(len as usize / 2).saturating_sub(1)]))
        }
    }

    pub fn startup_enabled() -> bool {
        read_run_value().is_some()
    }

    pub fn set_startup(on: bool) -> Result<(), String> {
        unsafe {
            if on {
                let cmd: Vec<u16> = run_command()?.encode_utf16().chain(Some(0)).collect();
                let r = RegSetKeyValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE, REG_SZ.0, Some(cmd.as_ptr() as *const _), (cmd.len() * 2) as u32);
                if r != ERROR_SUCCESS {
                    return Err(format!("couldn't write the startup entry ({})", r.0));
                }
            } else {
                let r = RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE);
                if r != ERROR_SUCCESS && startup_enabled() {
                    return Err(format!("couldn't remove the startup entry ({})", r.0));
                }
            }
        }
        Ok(())
    }

    /// If startup is enabled but points at an old location (exe was moved), fix it.
    pub fn refresh_startup_path() {
        if let (Some(cur), Ok(want)) = (read_run_value(), run_command()) {
            if !cur.eq_ignore_ascii_case(&want) {
                let _ = set_startup(true);
            }
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::*;
    pub fn find_window(_title: &str) -> Option<Hwnd> {
        None
    }
    pub fn style_window(_h: Hwnd, _backdrop: Backdrop) -> bool {
        false
    }
    pub fn work_area(_x: i32, _y: i32) -> Option<PxRect> {
        None
    }
    pub fn known_folder(f: Folder) -> Option<PathBuf> {
        let home = PathBuf::from(std::env::var_os("HOME")?);
        Some(home.join(match f {
            Folder::Pictures => "Pictures",
            Folder::Videos => "Videos",
        }))
    }
    pub fn startup_enabled() -> bool {
        false
    }
    pub fn set_startup(_on: bool) -> Result<(), String> {
        Err("Starting with the OS is only supported on Windows".into())
    }
    pub fn refresh_startup_path() {}
}

pub use imp::{find_window, known_folder, refresh_startup_path, set_startup, startup_enabled, style_window, work_area};

/// The native handle of eframe's main window.
pub fn main_hwnd(cc: &eframe::CreationContext<'_>) -> Option<Hwnd> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match cc.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(h) => Some(h.hwnd.get()),
        _ => None,
    }
}

/// Default save folders: `Pictures\Screenshots` and `Videos\Snippy`.
pub fn default_dir(f: Folder) -> PathBuf {
    let sub = match f {
        Folder::Pictures => "Screenshots",
        Folder::Videos => "Snippy",
    };
    known_folder(f).filter(|d| d.is_dir()).unwrap_or_else(std::env::temp_dir).join(sub)
}
