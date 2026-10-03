//! Microphone capture.
//!
//! One of the few places that touches the machine. Everything it produces is handed to
//! `hvtt_core::audio` for conditioning, so the arithmetic stays testable without a microphone.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use parking_lot::Mutex;
use std::sync::Arc;

/// Shared between the audio callback and the UI thread.
#[derive(Default)]
struct Buffer {
    samples: Vec<f32>,
    peak: f32,
    /// Paused by him: whatever the device still delivers is thrown away, never kept.
    paused: bool,
    /// When the device delivered its first sound: everything said before it is lost.
    first_sound: Option<std::time::Instant>,
}

pub struct Recording {
    stream: cpal::Stream,
    buffer: Arc<Mutex<Buffer>>,
    sample_rate: u32,
    channels: u16,
    device_name: String,
}

/// A microphone stream built but not started - the slow part of opening the microphone, done
/// before the keypress so the press only has to start it (`Prepared::start`). Nothing is captured
/// until then. The start of a quick dictation is where words were lost: missing the first 0.12 s
/// took base.en from 6% to 23% of words wrong, and opening the microphone took about 0.17 s
/// (Codex's sixth to eighth reviews ranked this first; an experiment from 2026-10-02, checked by
/// hand with him: the macOS microphone dot, AirPods).
pub struct Prepared {
    stream: cpal::Stream,
    buffer: Arc<Mutex<Buffer>>,
    sample_rate: u32,
    channels: u16,
    device_name: String,
    /// Cleared by the device reporting an error (unplugged, say): then it is not used.
    healthy: Arc<std::sync::atomic::AtomicBool>,
}

impl Prepared {
    /// Build the stream for the default (or preferred) input device, without starting it.
    pub fn new(preferred_device: Option<&str>) -> Result<Self, String> {
        let host = cpal::default_host();

        let device = match preferred_device {
            Some(want) => host
                .input_devices()
                .map_err(|e| format!("could not list input devices: {e}"))?
                .find(|d| device_name_of(d).as_deref() == Some(want))
                .or_else(|| host.default_input_device()),
            None => host.default_input_device(),
        }
        .ok_or_else(|| "No microphone found.".to_string())?;

        let device_name = device_name_of(&device).unwrap_or_else(|| "unknown".into());
        let config = device
            .default_input_config()
            .map_err(|e| format!("microphone has no usable input format: {e}"))?;

        // In cpal 0.18 these are plain integers, and StreamConfig is passed by value, so
        // everything needed is read off the supported config before it is converted.
        let sample_format = config.sample_format();
        let sample_rate = config.sample_rate();
        let channels = config.channels();
        let stream_config: cpal::StreamConfig = config.into();
        let buffer = Arc::new(Mutex::new(Buffer::default()));

        let healthy = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let err_buf = buffer.clone();
        let err_health = healthy.clone();
        let err_fn = move |e| {
            // A device error must not lose what was already captured.
            eprintln!("[hvtt] audio stream error: {e}");
            err_health.store(false, std::sync::atomic::Ordering::SeqCst);
            let _ = &err_buf;
        };

        let cb_buf = buffer.clone();
        let stream = match sample_format {
            cpal::SampleFormat::F32 => device.build_input_stream(
                stream_config,
                move |data: &[f32], _: &_| append(&cb_buf, data),
                err_fn,
                None,
            ),
            cpal::SampleFormat::I16 => device.build_input_stream(
                stream_config,
                move |data: &[i16], _: &_| {
                    let f: Vec<f32> = data.iter().map(|s| *s as f32 / i16::MAX as f32).collect();
                    append(&cb_buf, &f)
                },
                err_fn,
                None,
            ),
            cpal::SampleFormat::U16 => device.build_input_stream(
                stream_config,
                move |data: &[u16], _: &_| {
                    let f: Vec<f32> = data
                        .iter()
                        .map(|s| (*s as f32 / u16::MAX as f32) * 2.0 - 1.0)
                        .collect();
                    append(&cb_buf, &f)
                },
                err_fn,
                None,
            ),
            other => return Err(format!("unsupported microphone sample format: {other:?}")),
        }
        .map_err(|e| format!("could not open the microphone: {e}"))?;

        Ok(Prepared { stream, buffer, sample_rate, channels, device_name, healthy })
    }

    /// Still the device a dictation would use now, and no error from it since it was built.
    pub fn still_current(&self, preferred_device: Option<&str>) -> bool {
        if !self.healthy.load(std::sync::atomic::Ordering::SeqCst) {
            return false;
        }
        match preferred_device {
            Some(want) => self.device_name == want,
            None => cpal::default_host()
                .default_input_device()
                .and_then(|d| device_name_of(&d))
                .is_some_and(|name| name == self.device_name),
        }
    }

    /// Start capturing.
    pub fn start(self) -> Result<Recording, String> {
        let Prepared { stream, buffer, sample_rate, channels, device_name, .. } = self;
        stream
            .play()
            .map_err(|e| format!("could not start the microphone: {e}"))?;
        Ok(Recording { stream, buffer, sample_rate, channels, device_name })
    }
}

impl Recording {
    /// Open the default (or preferred) input device and start capturing at once: built and
    /// started in one go - what a prepared stream saves the keypress from.
    pub fn start(preferred_device: Option<&str>) -> Result<Self, String> {
        Prepared::new(preferred_device)?.start()
    }

    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    /// When the first sound arrived from the device, if any has yet.
    pub fn first_sound(&self) -> Option<std::time::Instant> {
        self.buffer.lock().first_sound
    }

    /// Peak level since the last call, 0.0..=1.0. Drives the listening animation.
    pub fn take_level(&self) -> f32 {
        let mut b = self.buffer.lock();
        let peak = b.peak;
        b.peak = 0.0;
        peak
    }

    /// Pause: the stream is stopped, and anything already in flight from the device is dropped,
    /// so nothing said while paused is ever kept or recognised.
    pub fn pause(&self) {
        self.buffer.lock().paused = true;
        let _ = self.stream.pause();
    }

    pub fn resume(&self) {
        self.buffer.lock().paused = false;
        let _ = self.stream.play();
    }

    /// What was captured from `start` (a position in 16 kHz samples) until now, as 16 kHz mono,
    /// without stopping - for recognising while he talks. Only the part not yet recognised is
    /// copied, so a ten-minute dictation costs no more per pass than a ten-second one.
    pub fn peek_from(&self, start: usize) -> Vec<f32> {
        let first = self.raw_index(start);
        let raw = self.buffer.lock().samples.get(first..).map(<[f32]>::to_vec).unwrap_or_default();
        hvtt_core::audio::condition(&raw, self.channels, self.sample_rate)
    }

    /// How much has been recorded so far, in 16 kHz samples.
    pub fn recorded_len(&self) -> usize {
        let raw = self.buffer.lock().samples.len() as u64;
        let frames = raw / self.channels.max(1) as u64;
        (frames * hvtt_core::audio::WHISPER_SAMPLE_RATE as u64 / self.sample_rate.max(1) as u64) as usize
    }

    /// Where a position in 16 kHz samples falls in the device's own interleaved samples.
    fn raw_index(&self, start: usize) -> usize {
        let rate = hvtt_core::audio::WHISPER_SAMPLE_RATE as u64;
        let frame = start as u64 * self.sample_rate as u64 / rate;
        frame as usize * self.channels as usize
    }

    /// Stop capturing and return the audio from `start` (16 kHz samples) on, as 16 kHz mono,
    /// ready for recognition - the part the live passes have not already recognised.
    pub fn finish_from(self, start: usize) -> Vec<f32> {
        let first = self.raw_index(start);
        // Dropping the stream stops it; do it explicitly so intent is visible.
        drop(self.stream);
        let raw = std::mem::take(&mut self.buffer.lock().samples);
        hvtt_core::audio::condition(raw.get(first..).unwrap_or_default(), self.channels, self.sample_rate)
    }
}

fn append(buffer: &Arc<Mutex<Buffer>>, data: &[f32]) {
    let peak = hvtt_core::audio::peak_level(data);
    let mut b = buffer.lock();
    if b.paused {
        return;
    }
    if b.first_sound.is_none() && !data.is_empty() {
        b.first_sound = Some(std::time::Instant::now());
    }
    b.samples.extend_from_slice(data);
    if peak > b.peak {
        b.peak = peak;
    }
}

/// cpal 0.18 exposes the human-readable name through `description()`.
fn device_name_of(device: &cpal::Device) -> Option<String> {
    device.description().ok().map(|d| d.name().to_string())
}
