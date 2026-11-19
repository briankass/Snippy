//! Self-update from GitHub Releases.
//!
//! 1. `check()` asks the GitHub API for the latest release and compares versions.
//! 2. `download()` saves the new `snippy.exe` next to the running one (`snippy.new.exe`).
//! 3. `install_and_restart()` renames the running exe to `snippy.old.exe` (Windows allows
//!    renaming a running executable, just not overwriting it), moves the new one into place
//!    and launches it. The next start deletes `snippy.old.exe`.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// The GitHub repository releases are published to.
pub const REPO: &str = "briankass/Snippy";
const ASSET: &str = "snippy.exe";
const USER_AGENT: &str = "Snippy-updater";

pub type Version = (u32, u32, u32);

#[derive(Clone, Debug)]
pub struct Release {
    pub version: Version,
    pub tag: String,
    pub exe_url: String,
    pub size: u64,
    pub page: String,
}

pub fn current_version() -> Version {
    parse_version(env!("CARGO_PKG_VERSION")).unwrap_or_default()
}

/// Parses `v1.2.3` / `1.2.3` (ignores anything after a `-` or `+`).
pub fn parse_version(s: &str) -> Option<Version> {
    let core = s.trim().trim_start_matches(['v', 'V']).split(['-', '+']).next()?;
    let mut it = core.split('.').map(|p| p.parse::<u32>());
    let v = (it.next()?.ok()?, it.next().unwrap_or(Ok(0)).ok()?, it.next().unwrap_or(Ok(0)).ok()?);
    Some(v)
}

pub fn version_string(v: Version) -> String {
    format!("{}.{}.{}", v.0, v.1, v.2)
}

fn agent() -> ureq::Agent {
    let tls = ureq::tls::TlsConfig::builder()
        // Use the OS certificate store (works behind corporate TLS-inspecting proxies too).
        .root_certs(ureq::tls::RootCerts::PlatformVerifier)
        .build();
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(300)))
        .timeout_connect(Some(Duration::from_secs(15)))
        .tls_config(tls)
        .build()
        .into()
}

/// Returns the latest release if it's newer than the running version.
pub fn check() -> Result<Option<Release>, String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let mut resp = agent()
        .get(&url)
        .header("User-Agent", USER_AGENT)
        .header("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| format!("couldn't reach GitHub: {e}"))?;
    let json: serde_json::Value = resp.body_mut().read_json().map_err(|e| format!("unexpected response: {e}"))?;
    let release = parse_release(&json)?;
    Ok((release.version > current_version()).then_some(release))
}

fn parse_release(json: &serde_json::Value) -> Result<Release, String> {
    let tag = json["tag_name"].as_str().ok_or("release has no tag")?.to_string();
    let version = parse_version(&tag).ok_or_else(|| format!("unrecognised version tag '{tag}'"))?;
    let asset = json["assets"]
        .as_array()
        .and_then(|a| a.iter().find(|x| x["name"].as_str().is_some_and(|n| n.eq_ignore_ascii_case(ASSET))))
        .ok_or_else(|| format!("release {tag} has no {ASSET}"))?;
    Ok(Release {
        version,
        tag,
        exe_url: asset["browser_download_url"].as_str().ok_or("asset has no download URL")?.to_string(),
        size: asset["size"].as_u64().unwrap_or(0),
        page: json["html_url"].as_str().unwrap_or_default().to_string(),
    })
}

fn sibling(exe: &Path, suffix: &str) -> PathBuf {
    let stem = exe.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "snippy".into());
    exe.with_file_name(format!("{stem}.{suffix}.exe"))
}

/// Downloads the release's exe next to the running one, reporting bytes received.
pub fn download(rel: &Release, progress: &AtomicU64) -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let dest = sibling(&exe, "new");
    let resp = agent()
        .get(&rel.exe_url)
        .header("User-Agent", USER_AGENT)
        .call()
        .map_err(|e| format!("download failed: {e}"))?;
    let mut reader = resp.into_body().into_with_config().limit(512 * 1024 * 1024).reader();
    let mut file = std::fs::File::create(&dest)
        .map_err(|e| format!("can't write next to snippy.exe ({}): {e}", dest.display()))?;
    let mut buf = vec![0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let n = reader.read(&mut buf).map_err(|e| format!("download interrupted: {e}"))?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        total += n as u64;
        progress.store(total, Ordering::Relaxed);
    }
    file.flush().map_err(|e| e.to_string())?;
    drop(file);
    verify_exe(&dest, rel.size).inspect_err(|_| {
        let _ = std::fs::remove_file(&dest);
    })?;
    Ok(dest)
}

/// Sanity checks: expected size and a Windows executable header.
fn verify_exe(path: &Path, expected_size: u64) -> Result<(), String> {
    let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
    if expected_size > 0 && meta.len() != expected_size {
        return Err(format!("download incomplete ({} of {expected_size} bytes)", meta.len()));
    }
    let mut head = [0u8; 2];
    std::fs::File::open(path).and_then(|mut f| f.read_exact(&mut head)).map_err(|e| e.to_string())?;
    if &head != b"MZ" {
        return Err("downloaded file isn't a Windows program".into());
    }
    Ok(())
}

/// Swaps `new_exe` into `current`'s place, keeping the old one as `*.old.exe`.
fn swap_in(current: &Path, new_exe: &Path) -> Result<(), String> {
    let old = sibling(current, "old");
    let _ = std::fs::remove_file(&old);
    std::fs::rename(current, &old).map_err(|e| format!("can't replace snippy.exe: {e}"))?;
    if let Err(e) = std::fs::rename(new_exe, current) {
        let _ = std::fs::rename(&old, current); // roll back
        return Err(format!("can't install the update: {e}"));
    }
    Ok(())
}

/// Installs the downloaded exe and starts it. The caller should then exit.
pub fn install_and_restart(new_exe: &Path) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    swap_in(&exe, new_exe)?;
    std::process::Command::new(&exe).spawn().map_err(|e| format!("installed, but couldn't restart: {e}"))?;
    Ok(())
}

/// Removes leftovers of a previous update (retrying while the old process exits).
pub fn cleanup_old() {
    let Ok(exe) = std::env::current_exe() else { return };
    std::thread::spawn(move || {
        for f in [sibling(&exe, "old"), sibling(&exe, "new")] {
            for _ in 0..20 {
                if !f.exists() || std::fs::remove_file(&f).is_ok() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    });
}

pub fn releases_page() -> String {
    format!("https://github.com/{REPO}/releases")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_parse_and_compare() {
        assert_eq!(parse_version("v0.4.0"), Some((0, 4, 0)));
        assert_eq!(parse_version("1.2"), Some((1, 2, 0)));
        assert_eq!(parse_version("v2.0.1-beta.1"), Some((2, 0, 1)));
        assert_eq!(parse_version("latest"), None);
        assert!(parse_version("v0.10.0").unwrap() > parse_version("v0.9.9").unwrap());
        assert_eq!(current_version(), parse_version(env!("CARGO_PKG_VERSION")).unwrap());
    }

    #[test]
    fn parses_github_release_json() {
        let json = serde_json::json!({
            "tag_name": "v9.1.0",
            "html_url": "https://github.com/x/y/releases/tag/v9.1.0",
            "assets": [
                {"name": "notes.txt", "browser_download_url": "https://e/notes", "size": 1},
                {"name": "snippy.exe", "browser_download_url": "https://e/snippy.exe", "size": 123}
            ]
        });
        let r = parse_release(&json).unwrap();
        assert_eq!((r.version, r.size, r.exe_url.as_str()), ((9, 1, 0), 123, "https://e/snippy.exe"));
        assert!(parse_release(&serde_json::json!({"tag_name": "v1.0.0", "assets": []})).is_err());
    }

    #[test]
    fn swap_replaces_and_keeps_backup() {
        let dir = std::env::temp_dir().join(format!("snippy-upd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cur = dir.join("snippy.exe");
        let new = dir.join("snippy.new.exe");
        std::fs::write(&cur, b"MZold").unwrap();
        std::fs::write(&new, b"MZnew").unwrap();
        verify_exe(&new, 5).unwrap();
        assert!(verify_exe(&new, 99).is_err());
        swap_in(&cur, &new).unwrap();
        assert_eq!(std::fs::read(&cur).unwrap(), b"MZnew");
        assert_eq!(std::fs::read(dir.join("snippy.old.exe")).unwrap(), b"MZold");
        assert!(!new.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Talks to the real GitHub API. Run with `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn live_check() {
        let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
        let mut resp = agent().get(&url).header("User-Agent", USER_AGENT).call().unwrap();
        let json: serde_json::Value = resp.body_mut().read_json().unwrap();
        let r = parse_release(&json).unwrap();
        eprintln!("latest: {} ({} bytes) {}", r.tag, r.size, r.exe_url);
        assert!(r.size > 1_000_000);
    }
}
