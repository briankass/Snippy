//! Text recognition ("copy text from screen") using the OCR engine built into Windows
//! (`Windows.Media.Ocr`): offline, nothing to install, uses the languages of the user's
//! Windows profile.

use crate::redact::Word;
use image::RgbaImage;

/// Recognises text in `img`. Lines are joined with newlines. Blocking: call it off the
/// UI thread.
pub fn recognize(img: &RgbaImage) -> Result<String, String> {
    let lines = recognize_words(img)?;
    Ok(lines
        .iter()
        .map(|l| l.iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join(" "))
        .collect::<Vec<_>>()
        .join("\n"))
}

/// Recognises text in `img` as lines of words, each with its box in `img`'s pixels.
/// Blocking: call it off the UI thread.
#[cfg(windows)]
pub fn recognize_words(img: &RgbaImage) -> Result<Vec<Vec<Word>>, String> {
    use windows::Graphics::Imaging::{BitmapPixelFormat, SoftwareBitmap};
    use windows::Media::Ocr::OcrEngine;
    use windows::Storage::Streams::DataWriter;
    use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};

    let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
    let result = (|| -> windows::core::Result<Result<Vec<Vec<Word>>, String>> {
        let Ok(engine) = OcrEngine::TryCreateFromUserProfileLanguages() else {
            return Ok(Err(
                "Windows has no text-recognition language installed. Add one in Settings → Time & language → Language".into(),
            ));
        };
        let prepared = prepare(img, OcrEngine::MaxImageDimension().unwrap_or(2600));
        let (w, h) = (prepared.width(), prepared.height());
        let (sx, sy) = (img.width() as f32 / w as f32, img.height() as f32 / h as f32);
        // RGBA → BGRA, which is what SoftwareBitmap's Bgra8 expects.
        let mut bgra = prepared.into_raw();
        for px in bgra.chunks_exact_mut(4) {
            px.swap(0, 2);
        }
        let writer = DataWriter::new()?;
        writer.WriteBytes(&bgra)?;
        let buffer = writer.DetachBuffer()?;
        let bitmap = SoftwareBitmap::CreateCopyFromBuffer(&buffer, BitmapPixelFormat::Bgra8, w as i32, h as i32)?;
        let ocr = engine.RecognizeAsync(&bitmap)?.join()?;
        let lines = ocr.Lines()?;
        let mut out = Vec::new();
        for i in 0..lines.Size()? {
            let words = lines.GetAt(i)?.Words()?;
            let mut line = Vec::new();
            for j in 0..words.Size()? {
                let word = words.GetAt(j)?;
                let r = word.BoundingRect()?;
                line.push(Word {
                    text: word.Text()?.to_string(),
                    rect: [r.X * sx, r.Y * sy, (r.X + r.Width) * sx, (r.Y + r.Height) * sy],
                });
            }
            out.push(line);
        }
        Ok(Ok(out))
    })();
    if com {
        unsafe { CoUninitialize() };
    }
    match result {
        Ok(r) => r,
        Err(e) => Err(format!("text recognition failed: {e}")),
    }
}

/// Scales the image into the range the OCR engine reads best: small text is enlarged
/// (recognition works best when text is at least ~20 px tall), and anything beyond the
/// engine's maximum size is reduced.
#[cfg(windows)]
fn prepare(img: &RgbaImage, max_dim: u32) -> RgbaImage {
    use image::imageops::{resize, FilterType};
    let (w, h) = (img.width(), img.height());
    let largest = w.max(h).max(1);
    if largest > max_dim {
        let s = max_dim as f32 / largest as f32;
        resize(img, ((w as f32 * s) as u32).max(1), ((h as f32 * s) as u32).max(1), FilterType::Triangle)
    } else if largest * 2 <= max_dim && h < 400 {
        // Small snips (a line or two of UI text): double the size for better accuracy.
        resize(img, w * 2, h * 2, FilterType::CatmullRom)
    } else {
        img.clone()
    }
}

#[cfg(not(windows))]
pub fn recognize_words(_img: &RgbaImage) -> Result<Vec<Vec<Word>>, String> {
    Err("Text recognition is only available on Windows".into())
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    /// Recognises a rendered sample ("Hello Snippy 2026"). Skips if the machine has no
    /// OCR language (e.g. some Windows Server images).
    #[test]
    fn recognizes_sample_text() {
        let img = image::load_from_memory(include_bytes!("../assets/test/ocr-sample.png")).unwrap().to_rgba8();
        match recognize(&img) {
            Ok(text) => {
                eprintln!("OCR result: {text:?}");
                let t = text.to_lowercase();
                assert!(t.contains("hello") && t.contains("snippy"), "unexpected OCR result: {text:?}");
            }
            Err(e) if e.contains("no text-recognition language") => eprintln!("SKIPPED: {e}"),
            Err(e) => panic!("{e}"),
        }
    }

    /// Finds the personal details in a rendered sample (an email address and a phone
    /// number) and nothing on the harmless line.
    #[test]
    fn finds_personal_info_in_sample() {
        let img = image::load_from_memory(include_bytes!("../assets/test/redact-sample.png")).unwrap().to_rgba8();
        match recognize_words(&img) {
            Ok(lines) => {
                for l in &lines {
                    eprintln!("OCR line: {:?}", l.iter().map(|w| (&w.text, w.rect.map(|v| v as i32))).collect::<Vec<_>>());
                }
                let hits = crate::redact::find_sensitive(&lines);
                eprintln!("hits: {hits:?}");
                // email on the first line (y 20..55), phone on the second (y 85..120)
                assert!(hits.iter().any(|r| r[1] < 60.0 && r[3] > 30.0), "email not found");
                assert!(hits.iter().any(|r| r[1] < 125.0 && r[3] > 95.0 && r[1] > 60.0), "phone not found");
                assert!(!hits.iter().any(|r| r[1] > 135.0), "harmless line hidden");
            }
            Err(e) if e.contains("no text-recognition language") => eprintln!("SKIPPED: {e}"),
            Err(e) => panic!("{e}"),
        }
    }
}
