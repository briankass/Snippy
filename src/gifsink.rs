//! Animated GIF output for screen recordings.
//!
//! Screen recordings are mostly still, so only the part of each frame that changed is
//! stored (with its own 256-colour palette), and unchanged frames just lengthen the
//! previous one. Frames are scaled down to at most `MAX_SIDE` pixels.

use crate::record::MediaSink;
use gif::{DisposalMethod, Encoder, Frame, Repeat};
use image::{imageops, RgbaImage};
use std::fs::File;
use std::io::BufWriter;
use std::collections::VecDeque;
use std::path::Path;
use std::thread::JoinHandle;
use std::time::Duration;

/// Longest side of the GIF, in pixels (bigger GIFs get huge and slow to share).
pub const MAX_SIDE: u32 = 960;
/// GIF frame rate cap.
pub const MAX_FPS: u32 = 15;

pub struct GifSink {
    encoder: Encoder<BufWriter<File>>,
    width: u32,
    height: u32,
    /// The image as it stands after all frames so far (output size).
    canvas: Option<RgbaImage>,
    /// Changed areas being compressed on other threads (in order), each with how long it
    /// stays up. The last one's time still grows while nothing changes.
    queue: VecDeque<(JoinHandle<Frame<'static>>, Duration)>,
    /// Time written so far, and the exact time it should be (for rounding to 1/100 s).
    written_cs: u64,
    total: Duration,
}

impl GifSink {
    pub fn new(path: &Path, src_w: u32, src_h: u32) -> Result<Self, String> {
        let (width, height) = output_size(src_w, src_h);
        let file = File::create(path).map_err(|e| format!("can't create {}: {e}", path.display()))?;
        let mut encoder = Encoder::new(BufWriter::new(file), width as u16, height as u16, &[]).map_err(|e| e.to_string())?;
        encoder.set_repeat(Repeat::Infinite).map_err(|e| e.to_string())?;
        Ok(Self { encoder, width, height, canvas: None, queue: VecDeque::new(), written_cs: 0, total: Duration::ZERO })
    }

    /// Writes queued frames until at most `keep` are left, each with its delay rounded so
    /// the running total stays exact.
    fn write_until(&mut self, keep: usize) -> Result<(), String> {
        while self.queue.len() > keep {
            let (job, dur) = self.queue.pop_front().expect("non-empty");
            let mut frame = job.join().map_err(|_| "GIF frame encoder crashed".to_string())?;
            self.total += dur;
            let target_cs = (self.total.as_secs_f64() * 100.0).round() as u64;
            let delay = target_cs.saturating_sub(self.written_cs).clamp(2, u16::MAX as u64);
            self.written_cs += delay;
            frame.delay = delay as u16;
            self.encoder.write_lzw_pre_encoded_frame(&frame).map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}

/// Size of the GIF for a `w`×`h` recording.
pub fn output_size(w: u32, h: u32) -> (u32, u32) {
    let longest = w.max(h).max(1);
    if longest <= MAX_SIDE {
        return (w.max(1), h.max(1));
    }
    let s = MAX_SIDE as f64 / longest as f64;
    (((w as f64 * s).round() as u32).max(1), ((h as f64 * s).round() as u32).max(1))
}

/// Smallest rectangle `[x, y, w, h]` containing every pixel that differs, or `None`.
fn changed_area(a: &RgbaImage, b: &RgbaImage) -> Option<[u32; 4]> {
    let (w, h) = (a.width(), a.height());
    let (ra, rb) = (a.as_raw(), b.as_raw());
    let stride = w as usize * 4;
    let rows: Vec<u32> = (0..h).filter(|&y| ra[y as usize * stride..][..stride] != rb[y as usize * stride..][..stride]).collect();
    let (&y0, &y1) = (rows.first()?, rows.last()?);
    let (mut x0, mut x1) = (w, 0);
    for &y in &rows {
        let off = y as usize * stride;
        let (la, lb) = (&ra[off..off + stride], &rb[off..off + stride]);
        if let Some(first) = (0..w as usize).find(|&x| la[x * 4..x * 4 + 4] != lb[x * 4..x * 4 + 4]) {
            x0 = x0.min(first as u32);
        }
        if let Some(last) = (0..w as usize).rev().find(|&x| la[x * 4..x * 4 + 4] != lb[x * 4..x * 4 + 4]) {
            x1 = x1.max(last as u32);
        }
    }
    Some([x0, y0, x1 - x0 + 1, y1 - y0 + 1])
}

impl MediaSink for GifSink {
    fn video(&mut self, img: &RgbaImage, _start: Duration, duration: Duration) -> Result<(), String> {
        let img = if img.width() != self.width || img.height() != self.height {
            imageops::resize(img, self.width, self.height, imageops::FilterType::Triangle)
        } else {
            img.clone()
        };
        let area = match &self.canvas {
            None => Some([0, 0, self.width, self.height]),
            Some(prev) => changed_area(prev, &img),
        };
        let Some([x, y, w, h]) = area else {
            // Nothing changed: the previous frame just stays up longer.
            if let Some((_, d)) = self.queue.back_mut() {
                *d += duration;
            }
            return Ok(());
        };
        let mut part = imageops::crop_imm(&img, x, y, w, h).to_image().into_raw();
        // Choosing colours and compressing is the slow part: do it on another thread, a
        // few frames at a time, and write the results in order.
        let job = std::thread::spawn(move || {
            // Opaque pixels only (the screen has no transparency).
            for px in part.chunks_exact_mut(4) {
                px[3] = 255;
            }
            let mut frame = Frame::from_rgba_speed(w as u16, h as u16, &mut part, 10);
            frame.left = x as u16;
            frame.top = y as u16;
            frame.dispose = DisposalMethod::Keep;
            frame.make_lzw_pre_encoded();
            frame
        });
        self.queue.push_back((job, duration));
        self.canvas = Some(img);
        // The last frame's time isn't final yet; older ones are. Keep a few in flight.
        let in_flight = std::thread::available_parallelism().map_or(2, |n| n.get().saturating_sub(1)).clamp(1, 4);
        self.write_until(in_flight)
    }

    fn audio(&mut self, _pcm: &[i16], _start: Duration) -> Result<(), String> {
        Ok(())
    }

    fn has_audio(&self) -> bool {
        false
    }

    fn name(&self) -> &'static str {
        "GIF"
    }

    fn finish(mut self: Box<Self>) -> Result<(), String> {
        self.write_until(0)?;
        drop(self.encoder); // writes the trailer
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;

    #[test]
    fn big_recordings_are_scaled_down() {
        assert_eq!(output_size(3840, 2160), (960, 540));
        assert_eq!(output_size(800, 600), (800, 600));
        assert_eq!(output_size(1000, 4000), (240, 960));
    }

    #[test]
    fn changed_area_is_tight() {
        let a = RgbaImage::from_pixel(100, 80, Rgba([10, 20, 30, 255]));
        let mut b = a.clone();
        assert_eq!(changed_area(&a, &b), None);
        b.put_pixel(12, 40, Rgba([0, 0, 0, 255]));
        b.put_pixel(30, 44, Rgba([0, 0, 0, 255]));
        assert_eq!(changed_area(&a, &b), Some([12, 40, 19, 5]));
    }

    /// A short animation: a box moving across a still background, plus still frames.
    /// Decodes back with the right size, frame count (still frames merged) and timing.
    #[test]
    fn writes_a_valid_compact_gif() {
        let dir = std::env::temp_dir().join(format!("snippy-gif-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.gif");
        let (w, h) = (1920, 1080); // scaled to 960×540
        let mut sink: Box<dyn MediaSink> = Box::new(GifSink::new(&path, w, h).unwrap());
        let bg = RgbaImage::from_fn(w, h, |x, y| Rgba([(x / 8) as u8, (y / 6) as u8, 120, 255]));
        let step = Duration::from_millis(100);
        for i in 0..10u32 {
            let mut f = bg.clone();
            if i < 6 {
                for y in 400..520 {
                    for x in (100 + i * 120)..(220 + i * 120) {
                        f.put_pixel(x, y, Rgba([250, 60, 40, 255]));
                    }
                }
            } // frames 6..9 are identical (box gone, still)
            sink.video(&f, step * i, step).unwrap();
        }
        sink.finish().unwrap();

        let mut opts = gif::DecodeOptions::new();
        opts.set_color_output(gif::ColorOutput::RGBA);
        let mut dec = opts.read_info(File::open(&path).unwrap()).unwrap();
        assert_eq!((dec.width(), dec.height()), (960, 540));
        let mut frames = 0;
        let mut total_cs = 0u32;
        let mut first_full = false;
        while let Some(f) = dec.read_next_frame().unwrap() {
            if frames == 0 {
                first_full = f.width == 960 && f.height == 540;
            } else {
                assert!(f.width < 960 || f.height < 540, "later frames store only what changed");
            }
            frames += 1;
            total_cs += f.delay as u32;
        }
        assert!(first_full, "first frame is the whole picture");
        assert_eq!(frames, 7, "6 moving frames + 1 for the still end");
        assert_eq!(total_cs, 100, "1 second in total");
        let bytes = std::fs::metadata(&path).unwrap().len();
        assert!(bytes < 400_000, "{bytes} bytes");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Worst case speed: every frame changes completely. `cargo test --release gif_speed -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn gif_speed() {
        let path = std::env::temp_dir().join("snippy-gif-speed.gif");
        let mut sink: Box<dyn MediaSink> = Box::new(GifSink::new(&path, 1920, 1080).unwrap());
        let t = std::time::Instant::now();
        for i in 0..15u32 {
            let f = RgbaImage::from_fn(1920, 1080, |x, y| Rgba([((x + i * 7) / 5) as u8, ((y * 3 + i) / 7) as u8, ((x ^ y) % 255) as u8, 255]));
            sink.video(&f, Duration::ZERO, Duration::from_millis(66)).unwrap();
        }
        sink.finish().unwrap();
        println!("15 full frames: {:?} per frame", t.elapsed() / 15);
    }
}
