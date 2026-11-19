//! OS integration: glass backdrop, rounded corners, capture exclusion, work area, known
//! folders. Everything degrades gracefully: on non-Windows platforms (or old Windows
//! versions) these are no-ops and the UI falls back to an opaque look.

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

#[cfg(windows)]
mod imp {
    use super::*;
    use windows::core::{HSTRING, PCWSTR};
    use windows::Win32::Foundation::{HWND, POINT};
    use windows::Win32::Graphics::Dwm::{
        DwmExtendFrameIntoClientArea, DwmSetWindowAttribute, DWMSBT_TRANSIENTWINDOW, DWMWA_SYSTEMBACKDROP_TYPE,
        DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
    };
    use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST};
    use windows::Win32::System::Com::CoTaskMemFree;
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

    /// Hides the window from screenshots/recordings, makes it dark + rounded, and (if
    /// `glass`) gives it the Windows 11 acrylic backdrop. Returns whether glass is active.
    pub fn style_window(h: Hwnd, glass: bool) -> bool {
        let h = hwnd(h);
        unsafe {
            // Windows 10 2004+: invisible to screen capture (so the bar isn't in videos).
            let _ = SetWindowDisplayAffinity(h, WDA_EXCLUDEFROMCAPTURE);
            let _ = set_attr(h, DWMWA_USE_IMMERSIVE_DARK_MODE, &1u32);
            let _ = set_attr(h, DWMWA_WINDOW_CORNER_PREFERENCE, &DWMWCP_ROUND);
            if !glass {
                return false;
            }
            // Acrylic ("transient window") backdrop: Windows 11 22H2+. Older versions reject
            // the attribute and we fall back to an opaque look.
            if !set_attr(h, DWMWA_SYSTEMBACKDROP_TYPE, &DWMSBT_TRANSIENTWINDOW) {
                return false;
            }
            let margins = MARGINS { cxLeftWidth: -1, cxRightWidth: -1, cyTopHeight: -1, cyBottomHeight: -1 };
            DwmExtendFrameIntoClientArea(h, &margins).is_ok()
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
}

#[cfg(not(windows))]
mod imp {
    use super::*;
    pub fn find_window(_title: &str) -> Option<Hwnd> {
        None
    }
    pub fn style_window(_h: Hwnd, _glass: bool) -> bool {
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
}

pub use imp::{find_window, known_folder, style_window, work_area};

/// The native handle of eframe's main window.
pub fn main_hwnd(cc: &eframe::CreationContext<'_>) -> Option<Hwnd> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match cc.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(h) => Some(h.hwnd.get()),
        _ => None,
    }
}

/// `<known folder>/<sub>`, created if needed; falls back to the temp dir.
pub fn output_dir(f: Folder, sub: &str) -> PathBuf {
    let dir = known_folder(f).filter(|d| d.is_dir()).unwrap_or_else(std::env::temp_dir).join(sub);
    let _ = std::fs::create_dir_all(&dir);
    dir
}
