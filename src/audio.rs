//! Audio for screen recordings: capture (system "loopback" audio and/or microphone),
//! conversion to 48 kHz stereo, and a clock-aligned mixer.
//!
//! The tricky part is timing. WASAPI loopback delivers *nothing* while nothing is playing,
//! and every device has its own jittery callback rhythm. So each incoming chunk is placed
//! on the recording timeline by its arrival time (a [`Track`]), and the mixer reads from
//! all tracks a fixed latency behind "now". Gaps become silence, jitter is absorbed, and
//! audio stays in sync with the video, which uses the same clock.

use std::collections::VecDeque;

pub const RATE: u32 = 48_000;
/// How far behind "now" the mixer reads; absorbs callback jitter.
pub const MIX_LATENCY_FRAMES: u64 = RATE as u64 / 10; // 100 ms
/// A chunk arriving later than this after the previous one ended is treated as a gap (silence).
const GAP_TOLERANCE: u64 = RATE as u64 / 20; // 50 ms
/// If a track runs this far ahead of the clock (device clock drift), drop audio to resync.
const MAX_AHEAD: u64 = RATE as u64 / 4; // 250 ms

/// Linear resampler + down/up-mixer to interleaved stereo at [`RATE`].
pub struct Resampler {
    channels: usize,
    step: f64, // input frames per output frame
    pos: f64,  // fractional read position relative to `prev`
    prev: [f32; 2],
}

impl Resampler {
    pub fn new(in_rate: u32, channels: u16) -> Self {
        Self { channels: channels.max(1) as usize, step: in_rate as f64 / RATE as f64, pos: 0.0, prev: [0.0; 2] }
    }

    /// Appends converted stereo samples for `input` (interleaved, `channels` per frame) to `out`.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let frames = input.len() / self.channels;
        let frame = |i: usize| -> [f32; 2] {
            let f = &input[i * self.channels..];
            if self.channels == 1 { [f[0], f[0]] } else { [f[0], f[1]] }
        };
        if (self.step - 1.0).abs() < 1e-9 {
            for i in 0..frames {
                out.extend_from_slice(&frame(i));
            }
            if frames > 0 {
                self.prev = frame(frames - 1);
            }
            return;
        }
        // Position p in [0, frames]: p == 0 is `prev`, p == k is input frame k-1.
        while self.pos < frames as f64 {
            let i = self.pos.floor() as usize;
            let t = (self.pos - i as f64) as f32;
            let a = if i == 0 { self.prev } else { frame(i - 1) };
            let b = frame(i);
            out.push(a[0] + (b[0] - a[0]) * t);
            out.push(a[1] + (b[1] - a[1]) * t);
            self.pos += self.step;
        }
        self.pos -= frames as f64;
        if frames > 0 {
            self.prev = frame(frames - 1);
        }
    }
}

/// One audio source's samples, positioned on the recording timeline (in frames at [`RATE`]).
#[derive(Default)]
pub struct Track {
    /// Timeline frame index of `buf[0..2]`.
    base: u64,
    /// Interleaved stereo samples.
    buf: VecDeque<f32>,
    started: bool,
}

impl Track {
    fn end(&self) -> u64 {
        self.base + (self.buf.len() / 2) as u64
    }

    /// Adds a chunk of stereo samples that *finished* arriving at timeline frame `arrival`.
    pub fn push(&mut self, arrival: u64, stereo: &[f32]) {
        let n = (stereo.len() / 2) as u64;
        let start = arrival.saturating_sub(n);
        if !self.started {
            self.started = true;
            self.base = start;
            self.buf.clear();
        } else if start > self.end() + GAP_TOLERANCE {
            // Gap (e.g. loopback went quiet): fill with silence up to where this chunk belongs.
            let missing = (start - self.end()) as usize;
            self.buf.extend(std::iter::repeat_n(0.0, missing * 2));
        } else if self.end() > arrival + MAX_AHEAD {
            // Device clock running fast relative to ours: skip this chunk to pull back in sync.
            return;
        }
        self.buf.extend(stereo.iter().copied());
    }

    /// Mixes timeline frames `[from, from + out.len()/2)` into `out` (adding), consuming
    /// them. Anything older than `from` is discarded; missing samples are silence.
    pub fn mix_into(&mut self, from: u64, out: &mut [f32]) {
        let to = from + (out.len() / 2) as u64;
        if self.base < from {
            let drop = ((from - self.base) as usize * 2).min(self.buf.len());
            self.buf.drain(..drop);
            self.base += (drop / 2) as u64;
            if self.buf.is_empty() {
                self.base = from;
            }
        }
        let start = self.base; // >= from here
        if start < to {
            let offset = (start - from) as usize;
            let avail = (self.buf.len() / 2).min((to - start) as usize);
            for (i, s) in self.buf.drain(..avail * 2).enumerate() {
                out[offset * 2 + i] += s;
            }
            self.base += avail as u64;
        }
    }
}

/// Mixes tracks into 16-bit PCM for timeline frames `[from, to)`.
pub fn mix(tracks: &mut [&mut Track], from: u64, to: u64) -> Vec<i16> {
    let mut acc = vec![0f32; ((to - from) * 2) as usize];
    for t in tracks.iter_mut() {
        t.mix_into(from, &mut acc);
    }
    acc.iter().map(|s| (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16).collect()
}

// ───────────────────────────── Capture (Windows / WASAPI) ─────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Source {
    /// Whatever is playing through the default speakers/headphones.
    System,
    Microphone,
}

#[cfg(windows)]
pub use capture::open;

#[cfg(windows)]
mod capture {
    use super::{Resampler, Source, Track, RATE};
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use std::sync::{Arc, Mutex};

    /// Opens a capture stream. `now_frame` returns the current recording timeline position
    /// (or `None` while paused / not started, in which case audio is discarded).
    pub fn open(
        source: Source,
        track: Arc<Mutex<Track>>,
        now_frame: Arc<dyn Fn() -> Option<u64> + Send + Sync>,
    ) -> Result<cpal::Stream, String> {
        let host = cpal::default_host();
        let (device, config) = match source {
            // WASAPI: an *input* stream on an *output* device records it in loopback mode.
            Source::System => {
                let d = host.default_output_device().ok_or("no speakers/headphones found")?;
                let c = d.default_output_config().map_err(|e| e.to_string())?;
                (d, c)
            }
            Source::Microphone => {
                let d = host.default_input_device().ok_or("no microphone found")?;
                let c = d.default_input_config().map_err(|e| e.to_string())?;
                (d, c)
            }
        };
        let format = config.sample_format();
        let mut resampler = Resampler::new(config.sample_rate(), config.channels());
        let mut scratch: Vec<f32> = Vec::new();
        let mut stereo: Vec<f32> = Vec::new();
        let _ = RATE;

        let stream = device
            .build_input_stream_raw(
                config.config(),
                format,
                move |data: &cpal::Data, _: &cpal::InputCallbackInfo| {
                    let Some(now) = now_frame() else { return };
                    scratch.clear();
                    match format {
                        cpal::SampleFormat::F32 => scratch.extend_from_slice(data.as_slice::<f32>().unwrap_or(&[])),
                        cpal::SampleFormat::I16 => scratch
                            .extend(data.as_slice::<i16>().unwrap_or(&[]).iter().map(|&s| s as f32 / 32768.0)),
                        cpal::SampleFormat::I32 => scratch
                            .extend(data.as_slice::<i32>().unwrap_or(&[]).iter().map(|&s| s as f32 / 2_147_483_648.0)),
                        cpal::SampleFormat::U16 => scratch.extend(
                            data.as_slice::<u16>().unwrap_or(&[]).iter().map(|&s| (s as f32 - 32768.0) / 32768.0),
                        ),
                        _ => return,
                    }
                    stereo.clear();
                    resampler.process(&scratch, &mut stereo);
                    if let Ok(mut t) = track.lock() {
                        t.push(now, &stereo);
                    }
                },
                |e| eprintln!("audio stream error: {e}"),
                None,
            )
            .map_err(|e| e.to_string())?;
        stream.play().map_err(|e| e.to_string())?;
        Ok(stream)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(frames: usize, v: f32) -> Vec<f32> {
        vec![v; frames * 2]
    }

    #[test]
    fn resampler_rates_and_channels() {
        let mut r = Resampler::new(44_100, 1);
        let mut out = Vec::new();
        for _ in 0..10 {
            r.process(&vec![0.5; 4410], &mut out); // 1 s of mono 44.1k in 10 chunks
        }
        let frames = out.len() / 2;
        assert!((47_990..=48_010).contains(&frames), "{frames}");
        assert!(out.iter().skip(4).all(|&s| (s - 0.5).abs() < 1e-6));

        let mut r = Resampler::new(48_000, 6); // 5.1 → stereo, same rate
        let mut out = Vec::new();
        r.process(&[0.1, 0.2, 0.3, 0.4, 0.5, 0.6], &mut out);
        assert_eq!(out, vec![0.1, 0.2]);
    }

    #[test]
    fn track_fills_gaps_with_silence() {
        let mut t = Track::default();
        t.push(4800, &tone(4800, 0.5)); // frames 0..4800
        t.push(48_000, &tone(4800, 0.25)); // 43200..48000, after a long quiet gap
        let mut out = vec![0.0; 48_000 * 2];
        t.mix_into(0, &mut out);
        assert_eq!(out[0], 0.5);
        assert_eq!(out[4799 * 2], 0.5);
        assert_eq!(out[20_000 * 2], 0.0); // the gap
        assert_eq!(out[45_000 * 2], 0.25);
    }

    #[test]
    fn track_absorbs_jitter_without_gaps() {
        // 10 ms chunks arriving up to 15 ms late should still be contiguous.
        let mut t = Track::default();
        let jitter = [0u64, 300, 720, 100, 0, 500];
        for (i, j) in jitter.iter().enumerate() {
            let arrival = (i as u64 + 1) * 480 + j;
            t.push(arrival, &tone(480, 1.0));
        }
        let mut out = vec![0.0; 480 * 6 * 2];
        t.mix_into(0, &mut out);
        assert!(out.iter().all(|&s| s == 1.0), "no silence holes from jitter");
    }

    #[test]
    fn mixing_sums_and_clamps() {
        let mut a = Track::default();
        let mut b = Track::default();
        a.push(100, &tone(100, 0.75));
        b.push(100, &tone(100, 0.75));
        let pcm = mix(&mut [&mut a, &mut b], 0, 100);
        assert_eq!(pcm.len(), 200);
        assert_eq!(pcm[0], i16::MAX); // 1.5 clamped
        // Consumed: next window is silent.
        let pcm = mix(&mut [&mut a, &mut b], 100, 200);
        assert!(pcm.iter().all(|&s| s == 0));
    }

    #[test]
    fn late_reads_skip_old_audio() {
        let mut t = Track::default();
        t.push(1000, &tone(1000, 0.5));
        let mut out = vec![0.0; 200];
        t.mix_into(900, &mut out); // first 900 frames are already in the past
        assert!(out.iter().all(|&s| s == 0.5));
    }
}
