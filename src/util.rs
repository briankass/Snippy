use super::*;

// ───────────────────────────── Files, images, misc ─────────────────────────────

pub(crate) fn save_image(img: &RgbaImage, path: &Path) -> Result<PathBuf, String> {
    let mut path = path.to_path_buf();
    let ext = path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase);
    let res = match ext.as_deref() {
        // The JPEG encoder can't take RGBA — flatten to RGB first.
        Some("jpg" | "jpeg") => DynamicImage::ImageRgba8(img.clone()).to_rgb8().save(&path),
        Some("png") => img.save(&path),
        _ => {
            path.set_extension("png");
            img.save(&path)
        }
    };
    res.map(|()| path).map_err(|e| e.to_string())
}

pub(crate) fn open_path(path: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    let mut cmd = std::process::Command::new("explorer");
    #[cfg(target_os = "macos")]
    let mut cmd = std::process::Command::new("open");
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = std::process::Command::new("xdg-open");
    cmd.arg(path).spawn().map(|_| ())
}

pub(crate) fn reveal_in_folder(path: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // explorer needs `/select,"path"` unsplit, so pass it raw.
        std::process::Command::new("explorer")
            .raw_arg(format!("/select,\"{}\"", path.display()))
            .spawn()
            .map(|_| ())
    }
    #[cfg(not(windows))]
    {
        open_path(path.parent().unwrap_or(path))
    }
}

pub(crate) fn move_file(from: &Path, to: &Path) -> std::io::Result<()> {
    if from == to {
        return Ok(());
    }
    // rename fails across drives; fall back to copy + delete.
    std::fs::rename(from, to).or_else(|_| {
        std::fs::copy(from, to)?;
        std::fs::remove_file(from)
    })
}

pub(crate) fn image_to_texture(ctx: &egui::Context, name: &str, img: &RgbaImage) -> egui::TextureHandle {
    let size = [img.width() as usize, img.height() as usize];
    let color = egui::ColorImage::from_rgba_unmultiplied(size, img.as_raw());
    ctx.load_texture(name, color, egui::TextureOptions::LINEAR)
}

pub(crate) fn unit_uv() -> Rect {
    Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0))
}

/// Truncate by characters, not bytes (slicing `&s[..28]` panics on multi-byte UTF-8).
pub(crate) fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        s.chars().take(max.saturating_sub(1)).chain(std::iter::once('…')).collect()
    }
}

/// Local date-time for file names, e.g. `2026-11-19 17-40-12`.
pub(crate) fn timestamp() -> String {
    chrono::Local::now().format("%Y-%m-%d %H-%M-%S").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: u32, h: u32) -> RgbaImage {
        RgbaImage::from_pixel(w, h, Rgba([255, 255, 255, 255]))
    }

    #[test]
    fn truncate_is_utf8_safe() {
        let t = "😀".repeat(40);
        assert_eq!(truncate_chars(&t, 10).chars().count(), 10);
    }

    #[test]
    fn jpeg_save_works_from_rgba() {
        let dir = std::env::temp_dir().join(format!("snippy-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = save_image(&solid(8, 8), &dir.join("a.jpg")).unwrap();
        assert!(p.exists());
        let p = save_image(&solid(8, 8), &dir.join("noext")).unwrap();
        assert_eq!(p.extension().unwrap(), "png");
        let _ = std::fs::remove_dir_all(dir);
    }
}
