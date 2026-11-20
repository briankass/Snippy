//! Diagnostics log: `%LOCALAPPDATA%\Snippy\snippy.log` (Windows) or the temp dir.
//!
//! Records Snippy's own events (shortcut handling, captures, flyouts, hook recovery) and,
//! for the two global shortcuts only (Print Screen, and S while Win is held), how each
//! press was handled. Never records other typing.
//!
//! Logging is asynchronous: callers just queue a line; a background thread writes it.
//! The file is capped: past the limit it's rotated to `snippy.old.log`.

use std::io::Write;
use std::path::PathBuf;
use std::sync::mpsc::{self, SyncSender};
use std::sync::OnceLock;

const MAX_BYTES: u64 = 512 * 1024;
static TX: OnceLock<SyncSender<String>> = OnceLock::new();

pub fn path() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("Snippy").join("snippy.log")
}

fn writer() -> &'static SyncSender<String> {
    TX.get_or_init(|| {
        let (tx, rx) = mpsc::sync_channel::<String>(1024);
        std::thread::Builder::new()
            .name("snippy-log".into())
            .spawn(move || {
                let p = path();
                if let Some(dir) = p.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                for line in rx {
                    if std::fs::metadata(&p).is_ok_and(|m| m.len() > MAX_BYTES) {
                        let _ = std::fs::rename(&p, p.with_file_name("snippy.old.log"));
                    }
                    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
                        let _ = f.write_all(line.as_bytes());
                    }
                }
            })
            .ok();
        tx
    })
}

/// Queues a timestamped line. Never blocks; drops the line if the queue is full.
pub fn log(msg: &str) {
    let line = format!("{}  {msg}\n", chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f"));
    let _ = writer().try_send(line);
}

/// Windows Snipping Tool detection (to catch presses that slip past Snippy).
#[cfg(windows)]
pub mod snipping_tool {
    use windows::core::BOOL;
    use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM};
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
    };
    use windows::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP, VK_ESCAPE,
    };
    use windows::Win32::UI::WindowsAndMessaging::{EnumWindows, GetForegroundWindow, GetWindowThreadProcessId, IsWindowVisible};

    /// Process names of the Windows Snipping Tool and its screen-capture overlay.
    const NAMES: [&str; 2] = ["snippingtool.exe", "screenclippinghost.exe"];

    /// Running Snipping Tool processes: (pid, exe name).
    pub fn processes() -> Vec<(u32, String)> {
        let mut out = Vec::new();
        unsafe {
            let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return out };
            let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
            let mut ok = Process32FirstW(snap, &mut e).is_ok();
            while ok {
                let len = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
                let name = String::from_utf16_lossy(&e.szExeFile[..len]).to_ascii_lowercase();
                if NAMES.contains(&name.as_str()) {
                    out.push((e.th32ProcessID, name));
                }
                ok = Process32NextW(snap, &mut e).is_ok();
            }
            let _ = CloseHandle(snap);
        }
        out
    }

    unsafe extern "system" fn collect_visible(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let st = &mut *(lparam.0 as *mut (Vec<u32>, Vec<isize>));
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if st.0.contains(&pid) && IsWindowVisible(hwnd).as_bool() {
            st.1.push(hwnd.0 as isize);
        }
        BOOL(1)
    }

    /// Visible top-level windows of the given processes.
    pub fn visible_windows_of(pids: &[u32]) -> Vec<isize> {
        if pids.is_empty() {
            return Vec::new();
        }
        let mut st: (Vec<u32>, Vec<isize>) = (pids.to_vec(), Vec::new());
        unsafe {
            let _ = EnumWindows(Some(collect_visible), LPARAM(&mut st as *mut _ as isize));
        }
        st.1
    }

    /// Process owning the foreground window.
    pub fn foreground_pid() -> u32 {
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(GetForegroundWindow(), Some(&mut pid)) };
        pid
    }

    pub fn terminate(pid: u32) -> bool {
        unsafe {
            let Ok(h) = OpenProcess(PROCESS_TERMINATE, false, pid) else { return false };
            let ok = TerminateProcess(h, 0).is_ok();
            let _ = CloseHandle(h);
            ok
        }
    }

    /// Presses Esc (cancels the Snipping Tool's capture overlay when it's in front).
    pub fn send_escape() {
        let key = |flags: KEYBD_EVENT_FLAGS| INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: VK_ESCAPE, wScan: 0, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
        };
        unsafe {
            SendInput(&[key(KEYBD_EVENT_FLAGS(0)), key(KEYEVENTF_KEYUP)], std::mem::size_of::<INPUT>() as i32);
        }
    }
}
