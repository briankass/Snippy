//! Tiny diagnostics log: `%LOCALAPPDATA%\Snippy\snippy.log` (Windows) or the temp dir.
//!
//! Records notable internal events only (e.g. "keyboard hook re-installed"), never what
//! the user types. Capped in size: when it grows past the limit it's rotated to
//! `snippy.old.log`.

#![cfg_attr(not(windows), allow(dead_code))] // logged from Windows-only code

use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

const MAX_BYTES: u64 = 256 * 1024;
static LOCK: Mutex<()> = Mutex::new(());

pub fn path() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("Snippy").join("snippy.log")
}

/// Appends a timestamped line. Never panics, never blocks for long.
pub fn log(msg: &str) {
    let Ok(_g) = LOCK.try_lock() else { return };
    let p = path();
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if std::fs::metadata(&p).is_ok_and(|m| m.len() > MAX_BYTES) {
        let _ = std::fs::rename(&p, p.with_file_name("snippy.old.log"));
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
        let _ = writeln!(f, "{} v{}  {msg}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f"), env!("CARGO_PKG_VERSION"));
    }
}
