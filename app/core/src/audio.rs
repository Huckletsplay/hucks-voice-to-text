//! Audio conditioning: whatever the microphone gives us, into what Whisper wants.
//!
//! Whisper wants 16 kHz mono `f32`. Real input devices hand back 44.1 or 48 kHz, often stereo.
//! This is pure arithmetic with no device handles in it, which is why it lives in core and can
//! be tested without a microphone.

/// Whisper's fixed input rate.
pub const WHISPER_SAMPLE_RATE: u32 = 16_000;

/// Average interleaved channels down to one.
pub fn to_mono(samples: &[f32], channels: u16) -> Vec<f32> {
    if channels <= 1 {
        return samples.to_vec();
    }
    let ch = channels as usize;
    samples
        .chunks_exact(ch)
        .map(|frame| frame.iter().sum::<f32>() / ch as f32)
        .collect()
}

/// Linear resampling.
///
/// Good enough for speech at these rates, and cheap — which matters because the hotkey budget
/// in `docs/user-experience.md` is 150 ms and this sits on that path.
pub fn resample_linear(input: &[f32], from_hz: u32, to_hz: u32) -> Vec<f32> {
    if from_hz == to_hz || input.is_empty() {
        return input.to_vec();
    }
    let ratio = to_hz as f64 / from_hz as f64;
    let out_len = ((input.len() as f64) * ratio).round() as usize;
    let mut out = Vec::with_capacity(out_len);

    for i in 0..out_len {
        let src = i as f64 / ratio;
        let lo = src.floor() as usize;
        let hi = (lo + 1).min(input.len() - 1);
        let frac = (src - lo as f64) as f32;
        let lo_v = input[lo.min(input.len() - 1)];
        out.push(lo_v + (input[hi] - lo_v) * frac);
    }
    out
}

/// Microphone input, conditioned for recognition.
pub fn condition(samples: &[f32], channels: u16, from_hz: u32) -> Vec<f32> {
    let mono = to_mono(samples, channels);
    resample_linear(&mono, from_hz, WHISPER_SAMPLE_RATE)
}

/// Peak amplitude, 0.0..=1.0. Drives the listening animation on the H.
pub fn peak_level(samples: &[f32]) -> f32 {
    samples.iter().fold(0.0f32, |acc, s| acc.max(s.abs())).min(1.0)
}

/// How long a buffer of conditioned audio runs for.
pub fn duration_secs(sample_count: usize, sample_rate: u32) -> f32 {
    if sample_rate == 0 {
        return 0.0;
    }
    sample_count as f32 / sample_rate as f32
}

/// Whisper refuses very short clips; below this a recording is a mis-press, not speech.
pub const MIN_USEFUL_SECS: f32 = 0.25;

pub fn is_long_enough(sample_count: usize, sample_rate: u32) -> bool {
    duration_secs(sample_count, sample_rate) >= MIN_USEFUL_SECS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stereo_averages_down_to_mono() {
        // L/R pairs: (1.0, 0.0) -> 0.5, (0.5, 0.5) -> 0.5
        let stereo = [1.0, 0.0, 0.5, 0.5];
        assert_eq!(to_mono(&stereo, 2), vec![0.5, 0.5]);
    }

    #[test]
    fn mono_input_passes_through_untouched() {
        let mono = [0.1, -0.2, 0.3];
        assert_eq!(to_mono(&mono, 1), mono.to_vec());
    }

    #[test]
    fn downsampling_48k_to_16k_gives_a_third_of_the_samples() {
        let input: Vec<f32> = (0..4800).map(|i| (i as f32 / 4800.0)).collect();
        let out = resample_linear(&input, 48_000, 16_000);
        assert_eq!(out.len(), 1600);
    }

    #[test]
    fn resampling_to_the_same_rate_changes_nothing() {
        let input = vec![0.1, 0.2, 0.3];
        assert_eq!(resample_linear(&input, 16_000, 16_000), input);
    }

    #[test]
    fn resampling_preserves_the_shape_of_a_ramp() {
        let input: Vec<f32> = (0..100).map(|i| i as f32).collect();
        let out = resample_linear(&input, 100, 50);
        // A rising ramp must still rise, and stay within the original range.
        assert!(out.windows(2).all(|w| w[1] >= w[0]), "ramp should stay monotonic");
        assert!(out.iter().all(|v| *v >= 0.0 && *v <= 99.0));
    }

    #[test]
    fn empty_input_is_handled_everywhere() {
        assert!(resample_linear(&[], 48_000, 16_000).is_empty());
        assert!(to_mono(&[], 2).is_empty());
        assert_eq!(peak_level(&[]), 0.0);
    }

    #[test]
    fn conditioning_stereo_48k_yields_mono_16k() {
        // 4800 stereo frames = 9600 samples, 0.1s at 48k -> 1600 samples at 16k
        let stereo: Vec<f32> = vec![0.5; 9600];
        let out = condition(&stereo, 2, 48_000);
        assert_eq!(out.len(), 1600);
        assert!((duration_secs(out.len(), WHISPER_SAMPLE_RATE) - 0.1).abs() < 0.001);
    }

    #[test]
    fn peak_level_finds_the_loudest_sample_and_is_clamped() {
        assert_eq!(peak_level(&[0.1, -0.7, 0.3]), 0.7);
        assert_eq!(peak_level(&[2.5]), 1.0, "clipping must not exceed 1.0");
    }

    #[test]
    fn a_tap_of_the_hotkey_is_rejected_as_too_short() {
        assert!(!is_long_enough(1000, WHISPER_SAMPLE_RATE)); // 0.0625s
        assert!(is_long_enough(16_000, WHISPER_SAMPLE_RATE)); // 1s
    }
}
