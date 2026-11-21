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

/// Records crashes: the panic message and location are written to the log *synchronously*
/// (the process may be about to exit), then the default handler runs.
pub fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        write_now(&format!("CRASH (thread '{}'): {info}", thread.name().unwrap_or("?")));
        default(info);
    }));
    #[cfg(windows)]
    native::install();
}

/// Appends a line to the log immediately (for crashes, when the writer thread may never run).
fn write_now(msg: &str) {
    let line = format!("{}  {msg}\n", chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f"));
    let p = path();
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
        let _ = f.write_all(line.as_bytes());
        let _ = f.flush();
    }
}

/// Native crashes (access violations in drivers or system DLLs, stack overflows) bypass
/// Rust's panic hook; record the exception code and the module it happened in.
#[cfg(windows)]
mod native {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::System::Diagnostics::Debug::{AddVectoredExceptionHandler, SetUnhandledExceptionFilter, EXCEPTION_POINTERS};
    use windows::Win32::System::LibraryLoader::{
        GetModuleFileNameW, GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
    };

    const STACK_OVERFLOW: u32 = 0xC000_00FD;

    unsafe fn describe(info: *const EXCEPTION_POINTERS) -> String {
        let Some(rec) = info.as_ref().and_then(|i| i.ExceptionRecord.as_ref()) else { return "unknown exception".into() };
        let code = rec.ExceptionCode.0 as u32;
        let addr = rec.ExceptionAddress as usize;
        let mut module = HMODULE::default();
        let mut name = String::from("?");
        if GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            PCWSTR(addr as *const u16),
            &mut module,
        )
        .is_ok()
        {
            let mut buf = [0u16; 260];
            let n = GetModuleFileNameW(Some(module), &mut buf) as usize;
            name = String::from_utf16_lossy(&buf[..n]);
            name = name.rsplit('\\').next().unwrap_or(&name).to_string();
        }
        let what = match code {
            0xC000_0005 => " (access violation)",
            STACK_OVERFLOW => " (stack overflow)",
            0xC000_0409 => " (fail-fast)",
            0xC000_001D => " (illegal instruction)",
            0x8000_0003 => " (breakpoint)",
            _ => "",
        };
        let thread = std::thread::current();
        format!(
            "CRASH (native, thread '{}'): exception 0x{code:08X}{what} in {name}+0x{:X}",
            thread.name().unwrap_or("?"),
            addr.wrapping_sub(module.0 as usize)
        )
    }

    unsafe extern "system" fn unhandled(info: *const EXCEPTION_POINTERS) -> i32 {
        super::write_now(&describe(info));
        0 // EXCEPTION_CONTINUE_SEARCH: let Windows end the process as usual
    }

    /// Stack overflows never reach the unhandled-exception filter (Rust's own handler
    /// aborts first), so catch those here.
    unsafe extern "system" fn vectored(info: *mut EXCEPTION_POINTERS) -> i32 {
        if let Some(rec) = info.as_ref().and_then(|i| i.ExceptionRecord.as_ref()) {
            if rec.ExceptionCode.0 as u32 == STACK_OVERFLOW {
                super::write_now(&describe(info));
            }
        }
        0 // EXCEPTION_CONTINUE_SEARCH
    }

    pub fn install() {
        unsafe {
            SetUnhandledExceptionFilter(Some(unhandled));
            AddVectoredExceptionHandler(1, Some(vectored));
        }
    }
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
    use windows::Win32::UI::WindowsAndMessaging::{EnumWindows, GetWindowThreadProcessId, IsWindowVisible};

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

    /// A visible top-level window of another program (for working out what pops up after
    /// a shortcut). Titles are deliberately not recorded (they can be private).
    #[derive(Clone, Debug)]
    pub struct WinInfo {
        pub hwnd: isize,
        pub pid: u32,
        pub class: String,
        pub rect: [i32; 4],
        pub topmost: bool,
    }

    unsafe extern "system" fn collect_all(hwnd: HWND, lparam: LPARAM) -> BOOL {
        use windows::Win32::Foundation::RECT;
        use windows::Win32::UI::WindowsAndMessaging::{GetClassNameW, GetWindowLongW, GetWindowRect, GWL_EXSTYLE, WS_EX_TOPMOST};
        let out = &mut *(lparam.0 as *mut Vec<WinInfo>);
        if !IsWindowVisible(hwnd).as_bool() {
            return BOOL(1);
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == std::process::id() {
            return BOOL(1);
        }
        let mut buf = [0u16; 128];
        let n = GetClassNameW(hwnd, &mut buf) as usize;
        let mut r = RECT::default();
        let _ = GetWindowRect(hwnd, &mut r);
        let topmost = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TOPMOST.0 != 0;
        out.push(WinInfo { hwnd: hwnd.0 as isize, pid, class: String::from_utf16_lossy(&buf[..n]), rect: [r.left, r.top, r.right, r.bottom], topmost });
        BOOL(1)
    }

    /// All visible top-level windows of other programs.
    pub fn visible_windows() -> Vec<WinInfo> {
        let mut v: Vec<WinInfo> = Vec::new();
        unsafe {
            let _ = EnumWindows(Some(collect_all), LPARAM(&mut v as *mut _ as isize));
        }
        v
    }

    /// Process id → executable name (lower case), for every running process.
    pub fn process_names() -> std::collections::HashMap<u32, String> {
        let mut out = std::collections::HashMap::new();
        unsafe {
            let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return out };
            let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
            let mut ok = Process32FirstW(snap, &mut e).is_ok();
            while ok {
                let len = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
                out.insert(e.th32ProcessID, String::from_utf16_lossy(&e.szExeFile[..len]).to_ascii_lowercase());
                ok = Process32NextW(snap, &mut e).is_ok();
            }
            let _ = CloseHandle(snap);
        }
        out
    }

    /// One line describing a window: program, window class, size and position, topmost.
    pub fn describe(w: &WinInfo, names: &std::collections::HashMap<u32, String>) -> String {
        let [l, t, r, b] = w.rect;
        format!(
            "{} (pid {}), class \"{}\", {}x{} at {},{}{}",
            names.get(&w.pid).map_or("?", |s| s.as_str()),
            w.pid,
            w.class,
            r - l,
            b - t,
            l,
            t,
            if w.topmost { ", topmost" } else { "" }
        )
    }

    pub fn terminate(pid: u32) -> bool {
        unsafe {
            let Ok(h) = OpenProcess(PROCESS_TERMINATE, false, pid) else { return false };
            let ok = TerminateProcess(h, 0).is_ok();
            let _ = CloseHandle(h);
            ok
        }
    }
}
