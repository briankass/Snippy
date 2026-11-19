//! Screen recording.
//!
//! ```text
//!  capture thread ──frames──┐
//!                           ├──▶ encoder thread ──▶ MediaSink (Media Foundation H.264+AAC,
//!  audio thread ───pcm──────┘                         or OpenH264 video-only fallback) ──▶ .mp4
//! ```
//!
//! Video and audio are timestamped from one shared [`Clock`] that stops while paused, so
//! paused time is cut from the video and both streams stay in sync. Frames are dropped
//! (not queued) if encoding falls behind, so the timeline never drifts.

use crate::audio::{self, Track};
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
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use xcap::Monitor;

const TIMESCALE: u32 = 90_000; // standard MP4 video timescale (ticks per second)
const MAX_QUEUED_FRAMES: usize = 3;

/// What to record: a rectangle (in physical pixels, relative to the monitor) of one monitor.
#[derive(Clone, Debug)]
pub struct RecordTarget {
    pub monitor_id: Option<u32>,
    /// `None` = the whole monitor.
    pub region: Option<[u32; 4]>,
}

#[derive(Clone, Debug)]
pub struct RecordOptions {
    pub fps: u32,
    pub countdown: Duration,
    pub system_audio: bool,
    pub microphone: bool,
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
    pub encoder: &'static str,
    pub has_audio: bool,
    /// Non-fatal problems worth telling the user (e.g. "no microphone found").
    pub notes: Vec<String>,
    pub thumbnail: Option<RgbaImage>,
}

/// Anything that can turn timestamped frames + audio into a file.
pub trait MediaSink {
    fn video(&mut self, img: &RgbaImage, start: Duration, duration: Duration) -> Result<(), String>;
    /// Interleaved 16-bit stereo PCM at [`audio::RATE`].
    fn audio(&mut self, pcm: &[i16], start: Duration) -> Result<(), String>;
    fn has_audio(&self) -> bool;
    fn name(&self) -> &'static str;
    fn finish(self: Box<Self>) -> Result<(), String>;
}

// ───────────────────────────── Clock ─────────────────────────────

/// Recording time that only advances while recording and not paused.
#[derive(Default)]
pub struct Clock {
    state: Mutex<ClockState>,
}

#[derive(Default)]
struct ClockState {
    started: Option<Instant>,
    paused_at: Option<Instant>,
    paused_total: Duration,
}

impl Clock {
    fn start(&self) {
        let mut s = self.state.lock().unwrap();
        s.started = Some(Instant::now());
    }
    /// `None` until recording starts.
    pub fn active(&self) -> Option<Duration> {
        let s = self.state.lock().unwrap();
        let start = s.started?;
        let now = s.paused_at.unwrap_or_else(Instant::now);
        Some(now.saturating_duration_since(start).saturating_sub(s.paused_total))
    }
    /// Like [`active`](Self::active) but `None` while paused (used to discard paused audio).
    pub fn running(&self) -> Option<Duration> {
        if self.is_paused() { None } else { self.active() }
    }
    pub fn is_paused(&self) -> bool {
        self.state.lock().unwrap().paused_at.is_some()
    }
    fn set_paused(&self, paused: bool) {
        let mut s = self.state.lock().unwrap();
        match (paused, s.paused_at) {
            (true, None) => s.paused_at = Some(Instant::now()),
            (false, Some(at)) => {
                s.paused_total += at.elapsed();
                s.paused_at = None;
            }
            _ => {}
        }
    }
}

// ───────────────────────────── Recorder ─────────────────────────────

#[derive(Default)]
struct Shared {
    stop: AtomicBool,
    clock: Clock,
    frames: AtomicU64,
    dropped: AtomicU64,
    queued: AtomicUsize,
    thumbnail: Mutex<Option<RgbaImage>>,
}

pub struct Recorder {
    shared: Arc<Shared>,
    handle: Option<JoinHandle<Result<RecordingInfo, String>>>,
    countdown_until: Instant,
}

enum Msg {
    Frame(RgbaImage, Duration),
    Audio(Vec<i16>, Duration),
    End(Duration),
}

impl Recorder {
    /// Starts recording after the countdown. Returns immediately.
    pub fn start(target: RecordTarget, opts: RecordOptions, path: PathBuf) -> Recorder {
        let shared = Arc::new(Shared::default());
        let sh = shared.clone();
        let countdown = opts.countdown;
        let handle = std::thread::Builder::new()
            .name("snippy-record".into())
            .spawn(move || run(target, opts, &path, sh))
            .expect("spawn recorder thread");
        Recorder { shared, handle: Some(handle), countdown_until: Instant::now() + countdown }
    }

    /// Seconds left before capture begins (for the "3, 2, 1" countdown).
    pub fn countdown_remaining(&self) -> Option<u64> {
        if self.shared.clock.active().is_some() {
            return None;
        }
        let left = self.countdown_until.saturating_duration_since(Instant::now());
        Some(left.as_secs_f32().ceil().max(1.0) as u64)
    }

    pub fn elapsed(&self) -> Duration {
        self.shared.clock.active().unwrap_or_default()
    }

    pub fn is_paused(&self) -> bool {
        self.shared.clock.is_paused()
    }

    pub fn set_paused(&self, paused: bool) {
        self.shared.clock.set_paused(paused);
    }

    /// True if the worker stopped on its own (i.e. an error happened).
    pub fn has_failed(&self) -> bool {
        self.handle.as_ref().is_some_and(|h| h.is_finished())
    }

    /// Stops, finalises the MP4 and returns info about it. Blocks until the file is written.
    pub fn stop(mut self) -> Result<RecordingInfo, String> {
        self.shared.clock.set_paused(false);
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

fn run(target: RecordTarget, opts: RecordOptions, path: &Path, sh: Arc<Shared>) -> Result<RecordingInfo, String> {
    // Countdown (interruptible).
    let go = Instant::now() + opts.countdown;
    while Instant::now() < go {
        if sh.stop.load(Ordering::Relaxed) {
            return Err("Recording cancelled".into());
        }
        std::thread::sleep(Duration::from_millis(30));
    }

    let fps = opts.fps.max(1);
    let monitor = find_monitor(target.monitor_id)?;
    let (mon_x, mon_y) = (monitor.x().unwrap_or(0), monitor.y().unwrap_or(0));
    let cursor_scale = monitor.scale_factor().unwrap_or(1.0).max(1.0);
    let (mw, mh) = (monitor.width().map_err(|e| e.to_string())?, monitor.height().map_err(|e| e.to_string())?);
    // H.264 4:2:0 needs even dimensions.
    let [rx, ry, rw, rh] = clamp_region(target.region.unwrap_or([0, 0, mw, mh]), mw, mh)?;

    let (tx, rx_msgs) = mpsc::channel::<Msg>();
    let mut notes = Vec::new();

    // Audio: open the devices first so we know whether the file needs an audio track.
    let audio = if opts.system_audio || opts.microphone {
        start_audio(&opts, tx.clone(), sh.clone(), &mut notes)
    } else {
        None
    };
    let want_audio = audio.is_some();

    // Encoder thread (creates the sink so COM/Media Foundation live on that thread).
    let (ready_tx, ready_rx) = mpsc::channel();
    let out = path.to_path_buf();
    let sh_enc = sh.clone();
    let encoder = std::thread::Builder::new()
        .name("snippy-encode".into())
        .spawn(move || encode_loop(rx_msgs, &out, rw, rh, fps, want_audio, &sh_enc, ready_tx))
        .map_err(|e| e.to_string())?;
    let (encoder_name, has_audio) = match ready_rx.recv() {
        Ok(Ok((name, has_audio, note))) => {
            notes.extend(note);
            (name, has_audio)
        }
        Ok(Err(e)) => return Err(e),
        Err(_) => return Err(encoder.join().ok().and_then(|r| r.err()).unwrap_or_else(|| "encoder failed".into())),
    };

    sh.clock.start();
    let capture_res = capture_loop(&monitor, [rx, ry, rw, rh], (mon_x, mon_y), cursor_scale, fps, &tx, &sh);
    sh.stop.store(true, Ordering::Relaxed); // also stops audio if capture failed
    let end = sh.clock.active().unwrap_or_default();
    if let Some(a) = audio {
        let _ = a.join(); // flushes its last audio before we send End
    }
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
        encoder: encoder_name,
        has_audio,
        notes,
        thumbnail: sh.thumbnail.lock().unwrap().take(),
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

// ───────────────────────────── Audio thread ─────────────────────────────

/// Opens the requested audio devices and spawns the mixer. Returns `None` (with notes) if
/// no device could be opened.
fn start_audio(opts: &RecordOptions, tx: Sender<Msg>, sh: Arc<Shared>, notes: &mut Vec<String>) -> Option<JoinHandle<()>> {
    #[cfg(not(windows))]
    {
        let _ = (opts, tx, sh);
        notes.push("Audio recording is only supported on Windows".into());
        None
    }
    #[cfg(windows)]
    {
        let wanted: Vec<audio::Source> = [(opts.system_audio, audio::Source::System), (opts.microphone, audio::Source::Microphone)]
            .into_iter()
            .filter_map(|(on, s)| on.then_some(s))
            .collect();
        let (ready_tx, ready_rx) = mpsc::channel::<Vec<String>>();
        let handle = std::thread::Builder::new()
            .name("snippy-audio".into())
            .spawn(move || {
                // cpal streams must stay on the thread that created them.
                let mut tracks = Vec::new();
                let mut streams = Vec::new();
                let mut errors = Vec::new();
                for source in wanted {
                    let track = Arc::new(Mutex::new(Track::default()));
                    let clock = sh.clone();
                    let now = Arc::new(move || {
                        clock.clock.running().map(|d| (d.as_secs_f64() * audio::RATE as f64) as u64)
                    });
                    match audio::open(source, track.clone(), now) {
                        Ok(s) => {
                            streams.push(s);
                            tracks.push(track);
                        }
                        Err(e) => errors.push(format!(
                            "{} not recorded: {e}",
                            if source == audio::Source::System { "System audio" } else { "Microphone" }
                        )),
                    }
                }
                let ok = !tracks.is_empty();
                let _ = ready_tx.send(errors);
                if ok {
                    mix_loop(&tracks, &tx, &sh);
                }
                drop(streams);
            })
            .ok()?;
        let errors = ready_rx.recv().unwrap_or_else(|_| vec!["Audio thread failed".into()]);
        let opened = errors.len() < [opts.system_audio, opts.microphone].iter().filter(|b| **b).count();
        notes.extend(errors);
        if opened {
            Some(handle)
        } else {
            let _ = handle.join();
            None
        }
    }
}

/// Every 20 ms, mixes all tracks up to (now − latency) and sends the PCM to the encoder.
#[cfg_attr(not(windows), allow(dead_code))]
fn mix_loop(tracks: &[Arc<Mutex<Track>>], tx: &Sender<Msg>, sh: &Shared) {
    let mut emitted = 0u64;
    loop {
        std::thread::sleep(Duration::from_millis(20));
        let stopping = sh.stop.load(Ordering::Relaxed);
        let Some(now) = sh.clock.active() else {
            if stopping {
                return;
            }
            continue;
        };
        let now_f = (now.as_secs_f64() * audio::RATE as f64) as u64;
        let target = if stopping { now_f } else { now_f.saturating_sub(audio::MIX_LATENCY_FRAMES) };
        if target > emitted {
            let mut guards: Vec<_> = tracks.iter().map(|t| t.lock().unwrap()).collect();
            let mut refs: Vec<&mut Track> = guards.iter_mut().map(|g| &mut **g).collect();
            let pcm = audio::mix(&mut refs, emitted, target);
            let start = Duration::from_secs_f64(emitted as f64 / audio::RATE as f64);
            if tx.send(Msg::Audio(pcm, start)).is_err() {
                return;
            }
            emitted = target;
        }
        if stopping {
            return;
        }
    }
}

// ───────────────────────────── Capture thread ─────────────────────────────

fn capture_loop(
    monitor: &Monitor,
    [x, y, w, h]: [u32; 4],
    mon_origin: (i32, i32),
    cursor_scale: f32,
    fps: u32,
    tx: &Sender<Msg>,
    sh: &Shared,
) -> Result<(), String> {
    let interval = Duration::from_secs_f64(1.0 / fps as f64);
    let whole = x == 0 && y == 0 && monitor.width().ok() == Some(w) && monitor.height().ok() == Some(h);
    let mut next = Instant::now();
    let mut consecutive_errors = 0;

    while !sh.stop.load(Ordering::Relaxed) {
        if sh.clock.is_paused() {
            std::thread::sleep(Duration::from_millis(20));
            next = Instant::now();
            continue;
        }
        let t = sh.clock.active().unwrap_or_default();

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
                {
                    let mut thumb = sh.thumbnail.lock().unwrap();
                    if thumb.is_none() {
                        *thumb = Some(make_thumbnail(&img, 320));
                    }
                }
                if sh.queued.load(Ordering::Relaxed) >= MAX_QUEUED_FRAMES {
                    sh.dropped.fetch_add(1, Ordering::Relaxed); // encoder is behind: drop, don't queue
                } else {
                    sh.queued.fetch_add(1, Ordering::Relaxed);
                    if tx.send(Msg::Frame(img, t)).is_err() {
                        return Err("encoder stopped unexpectedly".into());
                    }
                    sh.frames.fetch_add(1, Ordering::Relaxed);
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

pub fn make_thumbnail(img: &RgbaImage, max_w: u32) -> RgbaImage {
    if img.width() <= max_w {
        return img.clone();
    }
    let h = (img.height() as u64 * max_w as u64 / img.width() as u64).max(1) as u32;
    imageops::thumbnail(img, max_w, h)
}

// ───────────────────────────── Encoder thread ─────────────────────────────

type ReadyMsg = Result<(&'static str, bool, Option<String>), String>;

fn open_sink(path: &Path, w: u32, h: u32, fps: u32, want_audio: bool) -> Result<(Box<dyn MediaSink>, Option<String>), String> {
    #[cfg(windows)]
    let note = match crate::mf::MfSink::new(path, w, h, fps, want_audio) {
        Ok(s) => return Ok((Box::new(s), None)),
        Err(e) => Some(format!(
            "Used the fallback encoder ({e}){}",
            if want_audio { " — audio was not recorded" } else { "" }
        )),
    };
    #[cfg(not(windows))]
    let note = None;
    let _ = want_audio;
    Ok((Box::new(OpenH264Sink::new(path, w, h, fps)?), note))
}

/// Receives frames/audio, computes each frame's duration from the next frame's timestamp,
/// and feeds the sink. Returns the number of video frames written.
#[allow(clippy::too_many_arguments)]
fn encode_loop(
    rx: Receiver<Msg>,
    path: &Path,
    w: u32,
    h: u32,
    fps: u32,
    want_audio: bool,
    sh: &Shared,
    ready: Sender<ReadyMsg>,
) -> Result<u64, String> {
    let mut sink = match open_sink(path, w, h, fps, want_audio) {
        Ok((sink, note)) => {
            let _ = ready.send(Ok((sink.name(), sink.has_audio(), note)));
            sink
        }
        Err(e) => {
            let _ = ready.send(Err(e.clone()));
            return Err(e);
        }
    };
    let min_dur = Duration::from_secs_f64(1.0 / fps as f64);
    let mut pending: Option<(RgbaImage, Duration)> = None;
    let mut written = 0u64;

    for msg in rx {
        match msg {
            Msg::Frame(img, t) => {
                sh.queued.fetch_sub(1, Ordering::Relaxed);
                match pending.take() {
                    Some((prev, pt)) => {
                        let t = t.max(pt + Duration::from_millis(1)); // strictly increasing
                        sink.video(&prev, pt, t - pt)?;
                        written += 1;
                        pending = Some((img, t));
                    }
                    None => pending = Some((img, Duration::ZERO)), // first frame starts at 0
                }
            }
            Msg::Audio(pcm, t) => sink.audio(&pcm, t)?,
            Msg::End(t) => {
                if let Some((prev, pt)) = pending.take() {
                    sink.video(&prev, pt, t.saturating_sub(pt).max(min_dur))?;
                    written += 1;
                }
                break;
            }
        }
    }
    sink.finish()?;
    Ok(written)
}

// ───────────────────────────── OpenH264 fallback sink ─────────────────────────────

/// Software H.264 (Cisco OpenH264) + `mp4` muxer. Video only. Used when Media Foundation
/// isn't available, and on non-Windows platforms.
pub struct OpenH264Sink {
    encoder: Encoder,
    file: Option<BufWriter<File>>,
    muxer: Option<Mp4Writer<BufWriter<File>>>,
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
    width: u32,
    height: u32,
    carry: u64, // ticks of skipped frames, added to the next sample
}

fn to_ticks(d: Duration) -> u64 {
    (d.as_secs_f64() * TIMESCALE as f64).round() as u64
}

impl OpenH264Sink {
    pub fn new(path: &Path, width: u32, height: u32, fps: u32) -> Result<Self, String> {
        // ~0.12 bits per pixel per frame is plenty for screen content; clamp to a sane range.
        let bps = ((width as f64 * height as f64 * fps as f64 * 0.12) as u32).clamp(1_500_000, 20_000_000);
        let config = EncoderConfig::new()
            .usage_type(UsageType::ScreenContentRealTime)
            .rate_control_mode(RateControlMode::Quality)
            .bitrate(BitRate::from_bps(bps))
            .max_frame_rate(FrameRate::from_hz(fps as f32))
            .skip_frames(false)
            .intra_frame_period(IntraFramePeriod::from_num_frames(fps * 2))
            .vui(VuiConfig::bt601());
        let encoder = Encoder::with_api_config(OpenH264API::from_source(), config).map_err(|e| e.to_string())?;
        let file = File::create(path).map_err(|e| format!("can't create {}: {e}", path.display()))?;
        Ok(Self { encoder, file: Some(BufWriter::new(file)), muxer: None, sps: None, pps: None, width, height, carry: 0 })
    }
}

impl MediaSink for OpenH264Sink {
    fn video(&mut self, img: &RgbaImage, start: Duration, duration: Duration) -> Result<(), String> {
        let (w, h) = (self.width, self.height);
        let yuv = YUVBuffer::from_rgba8_source(RgbaSliceU8::new(img.as_raw(), (w as usize, h as usize)));
        let ts = openh264::Timestamp::from_millis(start.as_millis() as u64);
        let bitstream = self.encoder.encode_at(&yuv, ts).map_err(|e| e.to_string())?;

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
                    7 => self.sps = Some(nal.to_vec()),
                    8 => self.pps = Some(nal.to_vec()),
                    t => {
                        is_sync |= t == 5;
                        avcc.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                        avcc.extend_from_slice(nal);
                    }
                }
            }
        }
        let ticks = to_ticks(duration).max(1) + self.carry;
        if avcc.is_empty() {
            self.carry = ticks; // encoder skipped this frame
            return Ok(());
        }
        self.carry = 0;

        if self.muxer.is_none() {
            let (Some(sps), Some(pps)) = (&self.sps, &self.pps) else {
                return Ok(()); // can't start the file before we have SPS/PPS
            };
            let cfg = Mp4Config {
                major_brand: "isom".parse().unwrap(),
                minor_version: 512,
                compatible_brands: ["isom", "iso2", "avc1", "mp41"].iter().map(|b| b.parse().unwrap()).collect(),
                timescale: TIMESCALE,
            };
            let mut m = Mp4Writer::write_start(self.file.take().unwrap(), &cfg).map_err(|e| e.to_string())?;
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
            self.muxer = Some(m);
        }
        let sample = Mp4Sample {
            start_time: to_ticks(start),
            duration: ticks.min(u32::MAX as u64) as u32,
            rendering_offset: 0,
            is_sync,
            bytes: avcc.into(),
        };
        self.muxer.as_mut().unwrap().write_sample(1, &sample).map_err(|e| e.to_string())
    }

    fn audio(&mut self, _pcm: &[i16], _start: Duration) -> Result<(), String> {
        Ok(())
    }

    fn has_audio(&self) -> bool {
        false
    }

    fn name(&self) -> &'static str {
        "OpenH264"
    }

    fn finish(mut self: Box<Self>) -> Result<(), String> {
        if let Some(mut m) = self.muxer.take() {
            m.write_end().map_err(|e| e.to_string())?;
            use std::io::Write;
            m.into_writer().flush().map_err(|e| e.to_string())?;
        }
        Ok(())
    }
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

    /// Feeds synthetic frames through the encoder loop + OpenH264 sink and reads the MP4 back.
    #[test]
    fn encodes_valid_mp4() {
        let dir = std::env::temp_dir().join(format!("snippy-rec-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.mp4");
        let (w, h, fps) = (320u32, 240u32, 30u32);

        let (tx, rx) = mpsc::channel();
        let sh = Shared::default();
        for i in 0..45u32 {
            let img = RgbaImage::from_fn(w, h, |x, y| image::Rgba([((x + i * 4) % 256) as u8, (y % 256) as u8, 128, 255]));
            sh.queued.fetch_add(1, Ordering::Relaxed);
            tx.send(Msg::Frame(img, Duration::from_millis(i as u64 * 33))).unwrap();
        }
        tx.send(Msg::End(Duration::from_millis(1500))).unwrap();
        drop(tx);

        // Force the OpenH264 path so this test means the same thing on every OS.
        let mut sink: Box<dyn MediaSink> = Box::new(OpenH264Sink::new(&path, w, h, fps).unwrap());
        let mut pending: Option<(RgbaImage, Duration)> = None;
        let mut n = 0;
        for msg in rx {
            match msg {
                Msg::Frame(img, t) => {
                    if let Some((p, pt)) = pending.take() {
                        sink.video(&p, pt, t - pt).unwrap();
                        n += 1;
                    }
                    pending = Some((img, t));
                }
                Msg::End(t) => {
                    let (p, pt) = pending.take().unwrap();
                    sink.video(&p, pt, t - pt).unwrap();
                    n += 1;
                }
                Msg::Audio(..) => {}
            }
        }
        sink.finish().unwrap();
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
    fn clock_excludes_paused_time() {
        let c = Clock::default();
        assert!(c.active().is_none());
        c.start();
        std::thread::sleep(Duration::from_millis(50));
        c.set_paused(true);
        let at_pause = c.active().unwrap();
        assert!(c.running().is_none());
        std::thread::sleep(Duration::from_millis(80));
        assert_eq!(c.active().unwrap(), at_pause, "frozen while paused");
        c.set_paused(false);
        std::thread::sleep(Duration::from_millis(30));
        let t = c.active().unwrap();
        assert!(t >= at_pause + Duration::from_millis(25) && t < at_pause + Duration::from_millis(70), "{t:?}");
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

    #[test]
    fn thumbnail_keeps_aspect() {
        let t = make_thumbnail(&RgbaImage::new(1920, 1080), 320);
        assert_eq!((t.width(), t.height()), (320, 180));
    }
}
