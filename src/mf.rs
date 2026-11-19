//! Windows Media Foundation MP4 writer: H.264 video (hardware-accelerated when available)
//! plus optional AAC audio, using the built-in `IMFSinkWriter`.
//!
//! Using the codecs that ship with Windows means no third-party encoder (and no codec
//! licensing questions) for the common case. If Media Foundation is unavailable (e.g.
//! Windows "N" editions without the Media Feature Pack) the recorder falls back to OpenH264.

use crate::record::MediaSink;
use image::RgbaImage;
use std::path::Path;
use std::time::Duration;
use windows::core::HSTRING;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};

pub const AUDIO_RATE: u32 = crate::audio::RATE;
const AUDIO_CHANNELS: u32 = 2;

fn hns(d: Duration) -> i64 {
    (d.as_nanos() / 100) as i64
}

fn pack(hi: u32, lo: u32) -> u64 {
    ((hi as u64) << 32) | lo as u64
}

pub struct MfSink {
    /// `Option` so it can be released *before* MFShutdown/CoUninitialize in `Drop`.
    writer: Option<IMFSinkWriter>,
    com_initialized: bool,
    video: u32,
    audio: Option<u32>,
    width: u32,
    height: u32,
    bgra: Vec<u8>,
    finished: bool,
}

impl MfSink {
    /// Must be called on the thread that will write samples.
    pub fn new(path: &Path, width: u32, height: u32, fps: u32, with_audio: bool) -> Result<Self, String> {
        unsafe { Self::create(path, width, height, fps, with_audio) }.map_err(|e| format!("Media Foundation: {e}"))
    }

    unsafe fn create(path: &Path, width: u32, height: u32, fps: u32, with_audio: bool) -> windows::core::Result<Self> {
        // S_OK / S_FALSE both need a matching CoUninitialize; RPC_E_CHANGED_MODE (thread
        // already single-threaded) must not be balanced.
        let com_initialized = CoInitializeEx(None, COINIT_MULTITHREADED).is_ok();
        if let Err(e) = MFStartup(MF_VERSION, MFSTARTUP_FULL) {
            if com_initialized {
                CoUninitialize();
            }
            return Err(e);
        }
        // From here on, `Drop` (via `guard`) undoes MFStartup/CoInitialize on any error.
        let mut guard = StartupGuard { com_initialized, armed: true };

        let mut attrs: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut attrs, 3)?;
        let attrs = attrs.unwrap();
        attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)?;
        attrs.SetUINT32(&MF_SINK_WRITER_DISABLE_THROTTLING, 1)?;
        attrs.SetGUID(&MF_TRANSCODE_CONTAINERTYPE, &MFTranscodeContainerType_MPEG4)?;

        let _ = std::fs::remove_file(path);
        let url = HSTRING::from(path.as_os_str());
        let writer = MFCreateSinkWriterFromURL(&url, None, &attrs)?;

        // ── video: RGB32 in → H.264 out (the sink writer inserts the colour converter)
        let bitrate = ((width as f64 * height as f64 * fps as f64 * 0.15) as u32).clamp(2_000_000, 40_000_000);
        let out = MFCreateMediaType()?;
        out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
        out.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
        out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        out.SetUINT64(&MF_MT_FRAME_SIZE, pack(width, height))?;
        out.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
        out.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
        let video = writer.AddStream(&out)?;

        let inp = MFCreateMediaType()?;
        inp.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        inp.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)?;
        inp.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        inp.SetUINT64(&MF_MT_FRAME_SIZE, pack(width, height))?;
        inp.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
        inp.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
        // Positive stride = top-down rows (RGB defaults to bottom-up otherwise).
        inp.SetUINT32(&MF_MT_DEFAULT_STRIDE, width * 4)?;
        writer.SetInputMediaType(video, &inp, None)?;

        // ── audio: 16-bit PCM 48 kHz stereo in → AAC 192 kbps out
        let audio = if with_audio {
            let out = MFCreateMediaType()?;
            out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            out.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)?;
            out.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
            out.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, AUDIO_RATE)?;
            out.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, AUDIO_CHANNELS)?;
            out.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, 24_000)?;
            let a = writer.AddStream(&out)?;

            let inp = MFCreateMediaType()?;
            inp.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            inp.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
            inp.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
            inp.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, AUDIO_RATE)?;
            inp.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, AUDIO_CHANNELS)?;
            inp.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, 2 * AUDIO_CHANNELS)?;
            inp.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, AUDIO_RATE * 2 * AUDIO_CHANNELS)?;
            writer.SetInputMediaType(a, &inp, None)?;
            Some(a)
        } else {
            None
        };

        writer.BeginWriting()?;
        guard.armed = false;
        Ok(Self { writer: Some(writer), com_initialized, video, audio, width, height, bgra: Vec::new(), finished: false })
    }

    unsafe fn write(&self, stream: u32, bytes: &[u8], start: Duration, duration: Duration) -> windows::core::Result<()> {
        let buffer = MFCreateMemoryBuffer(bytes.len() as u32)?;
        let mut ptr = std::ptr::null_mut();
        buffer.Lock(&mut ptr, None, None)?;
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
        buffer.Unlock()?;
        buffer.SetCurrentLength(bytes.len() as u32)?;
        let sample = MFCreateSample()?;
        sample.AddBuffer(&buffer)?;
        sample.SetSampleTime(hns(start))?;
        sample.SetSampleDuration(hns(duration).max(1))?;
        self.writer.as_ref().expect("writer alive").WriteSample(stream, &sample)
    }
}

impl MediaSink for MfSink {
    fn video(&mut self, img: &RgbaImage, start: Duration, duration: Duration) -> Result<(), String> {
        debug_assert_eq!((img.width(), img.height()), (self.width, self.height));
        // RGBA → BGRA (MF's RGB32 byte order)
        self.bgra.clear();
        self.bgra.extend_from_slice(img.as_raw());
        for px in self.bgra.chunks_exact_mut(4) {
            px.swap(0, 2);
        }
        unsafe { self.write(self.video, &self.bgra, start, duration) }.map_err(|e| format!("video encode: {e}"))
    }

    fn audio(&mut self, pcm: &[i16], start: Duration) -> Result<(), String> {
        let Some(stream) = self.audio else { return Ok(()) };
        if pcm.is_empty() {
            return Ok(());
        }
        let frames = pcm.len() as u32 / AUDIO_CHANNELS;
        let duration = Duration::from_secs_f64(frames as f64 / AUDIO_RATE as f64);
        // SAFETY: i16 → bytes reinterpretation of an initialised slice (little-endian PCM).
        let bytes = unsafe { std::slice::from_raw_parts(pcm.as_ptr() as *const u8, pcm.len() * 2) };
        unsafe { self.write(stream, bytes, start, duration) }.map_err(|e| format!("audio encode: {e}"))
    }

    fn has_audio(&self) -> bool {
        self.audio.is_some()
    }

    fn name(&self) -> &'static str {
        "Media Foundation"
    }

    fn finish(mut self: Box<Self>) -> Result<(), String> {
        self.finished = true;
        let w = self.writer.as_ref().expect("writer alive");
        unsafe { w.Finalize() }.map_err(|e| format!("finalize: {e}"))
    }
}

impl Drop for MfSink {
    fn drop(&mut self) {
        unsafe {
            if let Some(w) = self.writer.take() {
                if !self.finished {
                    let _ = w.Finalize();
                }
                drop(w); // release the COM object while MF/COM are still running
            }
            let _ = MFShutdown();
            if self.com_initialized {
                CoUninitialize();
            }
        }
    }
}

/// Undoes MFStartup / CoInitializeEx if `MfSink::create` fails part-way.
struct StartupGuard {
    com_initialized: bool,
    armed: bool,
}

impl Drop for StartupGuard {
    fn drop(&mut self) {
        if self.armed {
            unsafe {
                let _ = MFShutdown();
                if self.com_initialized {
                    CoUninitialize();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes 2 s of a moving test pattern + a 440 Hz tone. CI uploads the file (path from
    /// `SNIPPY_MF_TEST_OUT`) so it can be inspected with ffprobe.
    #[test]
    fn writes_h264_aac_mp4() {
        let path = std::env::var_os("SNIPPY_MF_TEST_OUT")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join("snippy-mf-test.mp4"));
        let (w, h, fps) = (320u32, 240u32, 30u32);
        let mut sink: Box<dyn MediaSink> = match MfSink::new(&path, w, h, fps, true) {
            Ok(s) => Box::new(s),
            Err(e) => {
                eprintln!("SKIPPED: Media Foundation unavailable on this machine: {e}");
                return;
            }
        };
        assert!(sink.has_audio());
        // Top half red, bottom half blue, plus a moving white bar (to verify orientation).
        for i in 0..60u32 {
            let img = RgbaImage::from_fn(w, h, |x, y| {
                if x / 8 == i % 40 {
                    image::Rgba([255, 255, 255, 255])
                } else if y < h / 2 {
                    image::Rgba([220, 20, 20, 255])
                } else {
                    image::Rgba([20, 20, 220, 255])
                }
            });
            let t = Duration::from_millis(i as u64 * 1000 / fps as u64);
            sink.video(&img, t, Duration::from_millis(1000 / fps as u64)).unwrap();
        }
        let chunk = AUDIO_RATE as usize / 50; // 20 ms
        for c in 0..100 {
            let pcm: Vec<i16> = (0..chunk)
                .flat_map(|i| {
                    let n = (c * chunk + i) as f32;
                    let v = ((n * 440.0 * std::f32::consts::TAU / AUDIO_RATE as f32).sin() * 8000.0) as i16;
                    [v, v]
                })
                .collect();
            sink.audio(&pcm, Duration::from_millis(c as u64 * 20)).unwrap();
        }
        sink.finish().unwrap();
        let size = std::fs::metadata(&path).unwrap().len();
        assert!(size > 10_000, "file too small: {size}");
        eprintln!("wrote {} ({size} bytes)", path.display());
    }
}
