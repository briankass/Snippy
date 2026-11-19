//! Screen recording: capture thread → bounded queue → H.264 (OpenH264) → MP4.
//!
//! * Frames are timestamped with the real capture time, so the MP4 is variable-frame-rate
//!   and stays in sync even if capture or encoding can't keep up (frames get dropped, not delayed).
//! * Pausing simply stops the "active" clock, so paused time is cut out of the video.

use image::{imageops, RgbaImage};
use mp4::{AvcConfig, MediaConfig, Mp4Config, Mp4Sample, Mp4Writer, TrackConfig, TrackType};
use openh264::encoder::{
    BitRate, Encoder, EncoderConfig, FrameRate, IntraFramePeriod, RateControlMode, UsageType, VuiConfig,
};
use openh264::formats::{RgbaSliceU8, YUVBuffer};
use openh264::OpenH264API;
use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use xcap::Monitor;

const TIMESCALE: u32 = 90_000; // standard MP4 video timescale (ticks per second)

/// What to record: a rectangle (in physical pixels, relative to the monitor) of one monitor.
#[derive(Clone, Debug)]
pub struct RecordTarget {
    pub monitor_id: Option<u32>,
    /// `None` = the whole monitor.
    pub region: Option<[u32; 4]>,
}

#[derive(Debug, Clone)]
pub struct RecordingInfo {
    pub path: PathBuf,
    pub width: u32,
    pub height: u32,
    pub frames: u64,
    pub dropped: u64,
    pub duration: Duration,
    pub bytes: u64,
}

/// State shared between the UI and the worker threads.
#[derive(Default)]
struct Shared {
    stop: AtomicBool,
    paused: AtomicBool,
    started: AtomicBool,
    active_ms: AtomicU64,
    frames: AtomicU64,
    dropped: AtomicU64,
}

pub struct Recorder {
    shared: Arc<Shared>,
    handle: Option<JoinHandle<Result<RecordingInfo, String>>>,
    countdown_until: Instant,
}

enum Msg {
    Frame(RgbaImage, Duration),
    End(Duration),
}

impl Recorder {
    /// Starts recording after `countdown`. Returns immediately.
    pub fn start(target: RecordTarget, fps: u32, countdown: Duration, path: PathBuf) -> Recorder {
        let shared = Arc::new(Shared::default());
        let sh = shared.clone();
        let out = path;
        let handle = std::thread::Builder::new()
            .name("snippy-record".into())
            .spawn(move || run(target, fps.max(1), countdown, &out, &sh))
            .expect("spawn recorder thread");
        Recorder { shared, handle: Some(handle), countdown_until: Instant::now() + countdown }
    }

    /// Seconds left before capture begins (for the "3, 2, 1" countdown).
    pub fn countdown_remaining(&self) -> Option<u64> {
        if self.shared.started.load(Ordering::Relaxed) {
            return None;
        }
        let left = self.countdown_until.saturating_duration_since(Instant::now());
        Some(left.as_secs_f32().ceil() as u64)
    }

    pub fn elapsed(&self) -> Duration {
        Duration::from_millis(self.shared.active_ms.load(Ordering::Relaxed))
    }

    pub fn is_paused(&self) -> bool {
        self.shared.paused.load(Ordering::Relaxed)
    }

    pub fn set_paused(&self, paused: bool) {
        self.shared.paused.store(paused, Ordering::Relaxed);
    }

    /// True if the worker stopped on its own (i.e. an error happened).
    pub fn has_failed(&self) -> bool {
        self.handle.as_ref().is_some_and(|h| h.is_finished())
    }

    /// Stops, finalises the MP4 and returns info about it. Blocks until the file is written.
    pub fn stop(mut self) -> Result<RecordingInfo, String> {
        self.shared.stop.store(true, Ordering::Relaxed);
        match self.handle.take().map(JoinHandle::join) {
            Some(Ok(res)) => res,
            _ => Err("recording thread crashed".into()),
        }
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
    }
}

fn find_monitor(id: Option<u32>) -> Result<Monitor, String> {
    let monitors = Monitor::all().map_err(|e| e.to_string())?;
    let by_id = id.and_then(|id| monitors.iter().position(|m| m.id().ok() == Some(id)));
    let idx = by_id
        .or_else(|| monitors.iter().position(|m| m.is_primary().unwrap_or(false)))
        .unwrap_or(0);
    monitors.into_iter().nth(idx).ok_or_else(|| "no monitor found".into())
}

fn run(target: RecordTarget, fps: u32, countdown: Duration, path: &Path, sh: &Shared) -> Result<RecordingInfo, String> {
    // Countdown (interruptible).
    let go = Instant::now() + countdown;
    while Instant::now() < go {
        if sh.stop.load(Ordering::Relaxed) {
            return Err("Recording cancelled".into());
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    sh.started.store(true, Ordering::Relaxed);

    let monitor = find_monitor(target.monitor_id)?;
    let (mon_x, mon_y) = (monitor.x().unwrap_or(0), monitor.y().unwrap_or(0));
    let cursor_scale = monitor.scale_factor().unwrap_or(1.0).max(1.0);
    let (mw, mh) = (monitor.width().map_err(|e| e.to_string())?, monitor.height().map_err(|e| e.to_string())?);

    // H.264 4:2:0 needs even dimensions.
    let [rx, ry, rw, rh] = clamp_region(target.region.unwrap_or([0, 0, mw, mh]), mw, mh)?;

    // Encoder runs on its own thread; a short queue lets capture continue while a frame encodes.
    let (tx, rx_frames) = mpsc::sync_channel::<Msg>(3);
    let out = path.to_path_buf();
    let encoder = std::thread::Builder::new()
        .name("snippy-encode".into())
        .spawn(move || encode_loop(rx_frames, &out, rw, rh, fps))
        .map_err(|e| e.to_string())?;

    let capture_res = capture_loop(&monitor, [rx, ry, rw, rh], (mon_x, mon_y), cursor_scale, fps, &tx, sh);
    let end = Duration::from_millis(sh.active_ms.load(Ordering::Relaxed));
    let _ = tx.send(Msg::End(end));
    drop(tx);

    let enc_res = encoder.join().map_err(|_| "encoder thread crashed".to_string())?;
    capture_res?;
    let frames = enc_res?;
    if frames == 0 {
        let _ = std::fs::remove_file(path);
        return Err("No frames were captured".into());
    }
    Ok(RecordingInfo {
        path: path.to_path_buf(),
        width: rw,
        height: rh,
        frames,
        dropped: sh.dropped.load(Ordering::Relaxed),
        duration: end,
        bytes: std::fs::metadata(path).map(|m| m.len()).unwrap_or(0),
    })
}

fn clamp_region([x, y, w, h]: [u32; 4], mw: u32, mh: u32) -> Result<[u32; 4], String> {
    let x = x.min(mw);
    let y = y.min(mh);
    let w = (w.min(mw - x)) & !1;
    let h = (h.min(mh - y)) & !1;
    if w < 16 || h < 16 {
        return Err("Recording area is too small".into());
    }
    Ok([x, y, w, h])
}

fn capture_loop(
    monitor: &Monitor,
    [x, y, w, h]: [u32; 4],
    mon_origin: (i32, i32),
    cursor_scale: f32,
    fps: u32,
    tx: &SyncSender<Msg>,
    sh: &Shared,
) -> Result<(), String> {
    let interval = Duration::from_secs_f64(1.0 / fps as f64);
    let whole = x == 0 && y == 0 && monitor.width().ok() == Some(w) && monitor.height().ok() == Some(h);
    let mut active = Duration::ZERO;
    let mut last = Instant::now();
    let mut next = Instant::now();
    let mut consecutive_errors = 0;

    while !sh.stop.load(Ordering::Relaxed) {
        let now = Instant::now();
        if !sh.paused.load(Ordering::Relaxed) {
            active += now - last;
        }
        last = now;
        sh.active_ms.store(active.as_millis() as u64, Ordering::Relaxed);

        if sh.paused.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(20));
            next = Instant::now();
            continue;
        }

        let shot = if whole { monitor.capture_image() } else { monitor.capture_region(x, y, w, h) };
        match shot {
            Ok(mut img) => {
                consecutive_errors = 0;
                if img.width() != w || img.height() != h {
                    img = imageops::crop_imm(&img, 0, 0, w.min(img.width()), h.min(img.height())).to_image();
                    if img.width() != w || img.height() != h {
                        img = imageops::resize(&img, w, h, imageops::FilterType::Triangle);
                    }
                }
                if let Some((cx, cy)) = cursor_position() {
                    let (px, py) = (cx - mon_origin.0 - x as i32, cy - mon_origin.1 - y as i32);
                    draw_cursor(&mut img, px as f32, py as f32, cursor_scale);
                }
                match tx.try_send(Msg::Frame(img, active)) {
                    Ok(()) => {
                        sh.frames.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(TrySendError::Full(_)) => {
                        sh.dropped.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(TrySendError::Disconnected(_)) => return Err("encoder stopped unexpectedly".into()),
                }
            }
            Err(e) => {
                consecutive_errors += 1;
                if consecutive_errors > 30 {
                    return Err(format!("screen capture failed: {e}"));
                }
            }
        }

        next += interval;
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else {
            next = now; // running behind: don't try to "catch up" with a burst
        }
    }
    Ok(())
}

/// Encodes frames as they arrive and writes the MP4. Returns the number of frames written.
fn encode_loop(rx: Receiver<Msg>, path: &Path, w: u32, h: u32, fps: u32) -> Result<u64, String> {
    // ~0.12 bits per pixel per frame is plenty for screen content; clamp to a sane range.
    let bps = ((w as f64 * h as f64 * fps as f64 * 0.12) as u32).clamp(1_500_000, 20_000_000);
    let config = EncoderConfig::new()
        .usage_type(UsageType::ScreenContentRealTime)
        .rate_control_mode(RateControlMode::Quality)
        .bitrate(BitRate::from_bps(bps))
        .max_frame_rate(FrameRate::from_hz(fps as f32))
        .skip_frames(false)
        .intra_frame_period(IntraFramePeriod::from_num_frames(fps * 2))
        .vui(VuiConfig::bt601());
    let mut encoder = Encoder::with_api_config(OpenH264API::from_source(), config).map_err(|e| e.to_string())?;

    let file = File::create(path).map_err(|e| format!("can't create {}: {e}", path.display()))?;
    let mut muxer: Option<Mp4Writer<BufWriter<File>>> = None;
    let mut file = Some(BufWriter::new(file));
    let (mut sps, mut pps) = (None::<Vec<u8>>, None::<Vec<u8>>);
    let mut pending: Option<(Vec<u8>, u64, bool)> = None; // (avcc bytes, start tick, is_sync)
    let mut written = 0u64;
    let to_ticks = |d: Duration| (d.as_secs_f64() * TIMESCALE as f64).round() as u64;
    let min_dur = (TIMESCALE / fps.max(1)).max(1) as u64;

    let flush = |muxer: &mut Mp4Writer<BufWriter<File>>, sample: (Vec<u8>, u64, bool), end: u64| -> Result<(), String> {
        let (bytes, start, is_sync) = sample;
        let duration = end.saturating_sub(start).max(1).min(u32::MAX as u64) as u32;
        let s = Mp4Sample { start_time: start, duration, rendering_offset: 0, is_sync, bytes: bytes.into() };
        muxer.write_sample(1, &s).map_err(|e| e.to_string())
    };

    for msg in rx {
        match msg {
            Msg::Frame(img, t) => {
                let yuv = YUVBuffer::from_rgba8_source(RgbaSliceU8::new(img.as_raw(), (w as usize, h as usize)));
                let ts = openh264::Timestamp::from_millis(t.as_millis() as u64);
                let bitstream = encoder.encode_at(&yuv, ts).map_err(|e| e.to_string())?;

                // Convert Annex-B NAL units to MP4's length-prefixed (AVCC) format.
                let mut avcc = Vec::new();
                let mut is_sync = false;
                for l in 0..bitstream.num_layers() {
                    let layer = bitstream.layer(l).unwrap();
                    for n in 0..layer.nal_count() {
                        let nal = strip_start_code(layer.nal_unit(n).unwrap());
                        if nal.is_empty() {
                            continue;
                        }
                        match nal[0] & 0x1f {
                            7 => sps = Some(nal.to_vec()),
                            8 => pps = Some(nal.to_vec()),
                            t => {
                                if t == 5 {
                                    is_sync = true;
                                }
                                avcc.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                                avcc.extend_from_slice(nal);
                            }
                        }
                    }
                }
                if avcc.is_empty() {
                    continue; // encoder skipped this frame
                }

                if muxer.is_none() {
                    let (Some(sps), Some(pps)) = (&sps, &pps) else {
                        continue; // can't start the file before we have SPS/PPS
                    };
                    let cfg = Mp4Config {
                        major_brand: "isom".parse().unwrap(),
                        minor_version: 512,
                        compatible_brands: ["isom", "iso2", "avc1", "mp41"].iter().map(|b| b.parse().unwrap()).collect(),
                        timescale: TIMESCALE,
                    };
                    let mut m = Mp4Writer::write_start(file.take().unwrap(), &cfg).map_err(|e| e.to_string())?;
                    m.add_track(&TrackConfig {
                        track_type: TrackType::Video,
                        timescale: TIMESCALE,
                        language: "und".into(),
                        media_conf: MediaConfig::AvcConfig(AvcConfig {
                            width: w as u16,
                            height: h as u16,
                            seq_param_set: sps.clone(),
                            pic_param_set: pps.clone(),
                        }),
                    })
                    .map_err(|e| e.to_string())?;
                    muxer = Some(m);
                }

                let m = muxer.as_mut().unwrap();
                let start = to_ticks(t);
                if let Some(prev) = pending.take() {
                    // Keep timestamps strictly increasing.
                    let start = start.max(prev.1 + 1);
                    flush(m, prev, start)?;
                    pending = Some((avcc, start, is_sync));
                } else {
                    pending = Some((avcc, 0, true)); // first sample starts at 0
                }
                written += 1;
            }
            Msg::End(t) => {
                if let (Some(m), Some(prev)) = (muxer.as_mut(), pending.take()) {
                    let end = to_ticks(t).max(prev.1 + min_dur);
                    flush(m, prev, end)?;
                }
                break;
            }
        }
    }

    if let Some(mut m) = muxer {
        m.write_end().map_err(|e| e.to_string())?;
        use std::io::Write;
        m.into_writer().flush().map_err(|e| e.to_string())?;
    }
    Ok(written)
}

fn strip_start_code(nal: &[u8]) -> &[u8] {
    if nal.starts_with(&[0, 0, 0, 1]) {
        &nal[4..]
    } else if nal.starts_with(&[0, 0, 1]) {
        &nal[3..]
    } else {
        nal
    }
}

// ───────────── mouse cursor (screen capture APIs don't include it) ─────────────

#[cfg(windows)]
fn cursor_position() -> Option<(i32, i32)> {
    use windows_sys::Win32::Foundation::POINT;
    use windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos;
    let mut p = POINT { x: 0, y: 0 };
    // SAFETY: GetCursorPos writes into the POINT we pass.
    (unsafe { GetCursorPos(&mut p) } != 0).then_some((p.x, p.y))
}

#[cfg(not(windows))]
fn cursor_position() -> Option<(i32, i32)> {
    None
}

/// Draws a classic arrow pointer (white with black outline) with its tip at (x, y).
pub fn draw_cursor(img: &mut RgbaImage, x: f32, y: f32, scale: f32) {
    const SHAPE: [(f32, f32); 7] = [(0.0, 0.0), (0.0, 17.0), (4.5, 13.0), (7.5, 20.0), (10.5, 19.0), (7.5, 12.0), (13.0, 12.0)];
    let pts: Vec<(f32, f32)> = SHAPE.iter().map(|&(px, py)| (x + px * scale, y + py * scale)).collect();
    let outline = 1.3 * scale;
    let (w, h) = (img.width() as i32, img.height() as i32);
    let x0 = (x - 2.0).floor().max(0.0) as i32;
    let y0 = (y - 2.0).floor().max(0.0) as i32;
    let x1 = ((x + 15.0 * scale).ceil() as i32).min(w);
    let y1 = ((y + 22.0 * scale).ceil() as i32).min(h);
    for py in y0..y1 {
        for px in x0..x1 {
            let p = (px as f32 + 0.5, py as f32 + 0.5);
            if !point_in_polygon(p, &pts) {
                continue;
            }
            let d = (0..pts.len())
                .map(|i| seg_dist(p, pts[i], pts[(i + 1) % pts.len()]))
                .fold(f32::MAX, f32::min);
            let c = if d < outline { 0 } else { 255 };
            img.put_pixel(px as u32, py as u32, image::Rgba([c, c, c, 255]));
        }
    }
}

fn point_in_polygon((x, y): (f32, f32), poly: &[(f32, f32)]) -> bool {
    let mut inside = false;
    let mut j = poly.len() - 1;
    for i in 0..poly.len() {
        let ((xi, yi), (xj, yj)) = (poly[i], poly[j]);
        if (yi > y) != (yj > y) && x < (xj - xi) * (y - yi) / (yj - yi) + xi {
            inside = !inside;
        }
        j = i;
    }
    inside
}

fn seg_dist(p: (f32, f32), a: (f32, f32), b: (f32, f32)) -> f32 {
    let (abx, aby) = (b.0 - a.0, b.1 - a.1);
    let len2 = abx * abx + aby * aby;
    let t = if len2 > 0.0 { (((p.0 - a.0) * abx + (p.1 - a.1) * aby) / len2).clamp(0.0, 1.0) } else { 0.0 };
    let (dx, dy) = (p.0 - (a.0 + abx * t), p.1 - (a.1 + aby * t));
    (dx * dx + dy * dy).sqrt()
}

/// Default location for a new recording: `~/Videos/Snippy/Recording <date>.mp4`.
pub fn default_recording_path() -> PathBuf {
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(PathBuf::from);
    let dir = home
        .map(|h| h.join("Videos"))
        .filter(|d| d.is_dir())
        .unwrap_or_else(std::env::temp_dir)
        .join("Snippy");
    let _ = std::fs::create_dir_all(&dir);
    let stamp = chrono::Local::now().format("%Y-%m-%d %H-%M-%S");
    dir.join(format!("Recording {stamp}.mp4"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encodes synthetic frames through the real encoder + muxer and reads the MP4 back.
    #[test]
    fn encodes_valid_mp4() {
        let dir = std::env::temp_dir().join(format!("snippy-rec-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.mp4");
        let (w, h, fps) = (320u32, 240u32, 30u32);

        let (tx, rx) = mpsc::sync_channel(100);
        for i in 0..45u32 {
            let img = RgbaImage::from_fn(w, h, |x, y| image::Rgba([((x + i * 4) % 256) as u8, (y % 256) as u8, 128, 255]));
            tx.send(Msg::Frame(img, Duration::from_millis(i as u64 * 33))).unwrap();
        }
        tx.send(Msg::End(Duration::from_millis(1500))).unwrap();
        drop(tx);
        let n = encode_loop(rx, &path, w, h, fps).unwrap();
        assert_eq!(n, 45);

        let f = File::open(&path).unwrap();
        let size = f.metadata().unwrap().len();
        let mp4 = mp4::Mp4Reader::read_header(std::io::BufReader::new(f), size).unwrap();
        let track = mp4.tracks().get(&1).unwrap();
        assert_eq!((track.width(), track.height()), (w as u16, h as u16));
        assert_eq!(track.sample_count(), 45);
        let secs = mp4.duration().as_secs_f32();
        assert!((1.4..1.6).contains(&secs), "duration {secs}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn region_is_clamped_and_even() {
        assert_eq!(clamp_region([10, 10, 101, 51], 1920, 1080).unwrap(), [10, 10, 100, 50]);
        assert_eq!(clamp_region([1900, 0, 500, 500], 1920, 1080).unwrap(), [1900, 0, 20, 500]);
        assert!(clamp_region([0, 0, 5, 5], 1920, 1080).is_err());
    }

    #[test]
    fn cursor_draws_inside_bounds() {
        let mut img = RgbaImage::new(50, 50);
        draw_cursor(&mut img, 45.0, 45.0, 1.0); // partially off-image: must not panic
        draw_cursor(&mut img, 5.0, 5.0, 1.0);
        assert_eq!(img.get_pixel(8, 15)[3], 255);
    }
}
