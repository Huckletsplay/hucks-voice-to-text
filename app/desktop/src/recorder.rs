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
}

pub struct Recording {
    stream: cpal::Stream,
    buffer: Arc<Mutex<Buffer>>,
    sample_rate: u32,
    channels: u16,
    device_name: String,
}

impl Recording {
    /// Open the default input device and start capturing immediately.
    ///
    /// The hotkey budget is 150 ms, so nothing slow belongs on this path.
    pub fn start(preferred_device: Option<&str>) -> Result<Self, String> {
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

        let err_buf = buffer.clone();
        let err_fn = move |e| {
            // A device error must not lose what was already captured.
            eprintln!("[hvtt] audio stream error: {e}");
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

        stream
            .play()
            .map_err(|e| format!("could not start the microphone: {e}"))?;

        Ok(Recording { stream, buffer, sample_rate, channels, device_name })
    }

    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    /// Peak level since the last call, 0.0..=1.0. Drives the listening animation.
    pub fn take_level(&self) -> f32 {
        let mut b = self.buffer.lock();
        let peak = b.peak;
        b.peak = 0.0;
        peak
    }

    /// Stop capturing and return 16 kHz mono audio, ready for recognition.
    pub fn finish(self) -> Vec<f32> {
        // Dropping the stream stops it; do it explicitly so intent is visible.
        drop(self.stream);
        let raw = std::mem::take(&mut self.buffer.lock().samples);
        hvtt_core::audio::condition(&raw, self.channels, self.sample_rate)
    }
}

fn append(buffer: &Arc<Mutex<Buffer>>, data: &[f32]) {
    let peak = hvtt_core::audio::peak_level(data);
    let mut b = buffer.lock();
    b.samples.extend_from_slice(data);
    if peak > b.peak {
        b.peak = peak;
    }
}

/// cpal 0.18 exposes the human-readable name through `description()`.
fn device_name_of(device: &cpal::Device) -> Option<String> {
    device.description().ok().map(|d| d.name().to_string())
}
