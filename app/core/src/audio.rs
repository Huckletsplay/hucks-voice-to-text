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

/// The filter keeps everything below this share of the lower rate (7.5 kHz of Whisper's 16).
const CUTOFF: f64 = 0.47;
/// How many of the filter's ripples it spans either side: sharper with more, slower too.
const ZERO_CROSSINGS: f64 = 16.0;
/// Kaiser window shape: about 80 dB between what is kept and what is removed.
const KAISER_BETA: f64 = 8.0;
/// Positions between two input samples the filter is worked out for.
const PHASES: usize = 256;

/// Resampling, filtered first (a Kaiser-windowed sinc).
///
/// Whisper hears 0-8 kHz. A microphone at 48 kHz also records 8-24 kHz - its own hiss, the top of
/// every "s" and "f" - and the linear interpolation used until 0.1.7, which at exactly this ratio
/// is taking every third sample, folded all of it back down on top of the voice. Removed first,
/// it never reaches Whisper. About a millisecond per second of audio.
pub fn resample(input: &[f32], from_hz: u32, to_hz: u32) -> Vec<f32> {
    if from_hz == to_hz || input.is_empty() || from_hz == 0 || to_hz == 0 {
        return input.to_vec();
    }
    let step = from_hz as f64 / to_hz as f64;
    // Cycles per input sample; when upsampling, the input's own limit is the one that matters.
    let fc = CUTOFF * from_hz.min(to_hz) as f64 / from_hz as f64;
    let reach = ZERO_CROSSINGS / (2.0 * fc);
    let half = reach.ceil() as usize;
    let i0_beta = bessel_i0(KAISER_BETA);
    // Only the positions this pair of rates actually lands on are worked out: one at 48 kHz.
    let mut rows: Vec<Option<Vec<f32>>> = vec![None; PHASES];
    let row = |phase: usize| -> Vec<f32> {
        let frac = phase as f64 / PHASES as f64;
        let mut taps: Vec<f64> = (0..2 * half)
            .map(|k| {
                let t = k as f64 - (half as f64 - 1.0) - frac;
                if t.abs() >= reach {
                    return 0.0;
                }
                let x = 2.0 * fc * t;
                let sinc = if x.abs() < 1e-12 {
                    1.0
                } else {
                    (std::f64::consts::PI * x).sin() / (std::f64::consts::PI * x)
                };
                let r = t / reach;
                sinc * bessel_i0(KAISER_BETA * (1.0 - r * r).sqrt()) / i0_beta
            })
            .collect();
        // Unity gain, so a filtered voice is exactly as loud as it was.
        let sum: f64 = taps.iter().sum();
        taps.iter_mut().for_each(|t| *t /= sum);
        taps.into_iter().map(|t| t as f32).collect()
    };

    let out_len = (input.len() as f64 / step).round() as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let center = i as f64 * step;
        let mut base = center.floor() as isize;
        let mut phase = ((center - base as f64) * PHASES as f64).round() as usize;
        if phase == PHASES {
            phase = 0;
            base += 1;
        }
        let taps = rows[phase].get_or_insert_with(|| row(phase));
        // Taps that fall before the start or after the end meet silence.
        let first = base - half as isize + 1;
        let lo = (-first).max(0) as usize;
        let hi = taps.len().min((input.len() as isize - first).max(0) as usize);
        let mut acc = 0.0f32;
        for k in lo..hi {
            acc += input[(first + k as isize) as usize] * taps[k];
        }
        out.push(acc);
    }
    out
}

/// The modified Bessel function of the first kind, order zero, for the Kaiser window.
fn bessel_i0(x: f64) -> f64 {
    let (mut sum, mut term, mut k) = (1.0, 1.0, 1.0);
    while term > 1e-12 * sum {
        term *= (x / (2.0 * k)) * (x / (2.0 * k));
        sum += term;
        k += 1.0;
    }
    sum
}

/// Microphone input, conditioned for recognition.
pub fn condition(samples: &[f32], channels: u16, from_hz: u32) -> Vec<f32> {
    let mono = to_mono(samples, channels);
    resample(&mono, from_hz, WHISPER_SAMPLE_RATE)
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

// ---------------------------------------------------------------------------- live recognition
//
// While he talks, finished stretches of speech are recognised in the background so the box fills
// in and the stop press only has the last few seconds left to do. A stretch ends at a pause, so
// no word is ever cut in half; if he never pauses, it ends at the quietest moment instead.

/// Loudness is judged in windows this long (20 ms at 16 kHz).
const WINDOW: usize = 320;
/// A pause this long between words is a place to cut.
const PAUSE_WINDOWS: usize = 15; // 300 ms
/// Quieter than this, a window is silence, whatever the microphone's gain.
const SILENCE_FLOOR: f32 = 0.004;
/// Below this nothing is sound at all - about -74 dB, digital silence and the faintest hiss. Quiet
/// speech is told from noise by how far it rises above the room, not by loudness (Codex's sixth
/// review: a phrase at 0.3% volume, which Whisper heard, was thrown away as silence).
const NOTHING_AT_ALL: f32 = 0.0002;

/// The room: the level of the quietest tenth of the windows - the gaps between words, a fan,
/// the microphone's own hiss.
fn room_level(rms: &[f32]) -> f32 {
    let mut sorted = rms.to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));
    sorted.get(sorted.len() / 10).copied().unwrap_or(0.0)
}

fn window_rms(samples: &[f32]) -> Vec<f32> {
    samples
        .chunks(WINDOW)
        .map(|w| (w.iter().map(|s| s * s).sum::<f32>() / w.len() as f32).sqrt())
        .collect()
}

/// Where to end the next finished stretch of `samples` (16 kHz), or `None` to wait for more.
///
/// A stretch is at least `min_secs` long. The cut goes in the middle of the last pause of
/// 300 ms or more; with no pause by `max_secs`, at the quietest moment after `min_secs`.
pub fn commit_point(samples: &[f32], min_secs: f32, max_secs: f32) -> Option<usize> {
    let min = (min_secs * WHISPER_SAMPLE_RATE as f32) as usize / WINDOW;
    let max = (max_secs * WHISPER_SAMPLE_RATE as f32) as usize / WINDOW;
    let rms = window_rms(samples);
    if rms.len() <= min + PAUSE_WINDOWS {
        return None;
    }
    // Quiet relative to how loudly he is speaking, so a hot or a distant microphone both work.
    let mut sorted = rms.clone();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let loud = sorted[sorted.len() * 9 / 10];
    let quiet = (loud * 0.15).max(SILENCE_FLOOR);

    // The last pause long enough to cut in, not counting one still running at the very end:
    // he may only be drawing breath, and the next pass will see it either way.
    let mut best = None;
    let mut run = 0;
    for (i, level) in rms.iter().enumerate() {
        if *level < quiet {
            run += 1;
        } else {
            if run >= PAUSE_WINDOWS && i - run / 2 > min {
                best = Some((i - run / 2) * WINDOW);
            }
            run = 0;
        }
    }
    if best.is_some() {
        return best;
    }
    if rms.len() >= max {
        let (at, _) = rms[min..max]
            .iter()
            .enumerate()
            .min_by(|a, b| a.1.total_cmp(b.1))?;
        return Some((min + at) * WINDOW);
    }
    None
}

// ---------------------------------------------------------------------------- speech or not
//
// Two judges. A trained voice detector (whisper.cpp's Silero model, set by the program with
// `use_detector`) tells speech from noise where loudness cannot: quiet speech over a fan, a soft
// last sound with no gap before it (Codex's sixth and seventh reviews, 2026-10-01/02 - two rounds
// of loudness rules each failed a new case). Loudness is still the first word on "is there speech"
// and the whole answer when no detector is set (tests, a missing model file).

/// Where speech is in 16 kHz audio, as (start, end) in seconds; `None` when it cannot say.
pub type Detector = dyn Fn(&[f32]) -> Option<Vec<(f32, f32)>> + Send + Sync;

static DETECTOR: std::sync::OnceLock<Box<Detector>> = std::sync::OnceLock::new();

/// Set the voice detector, once, for the life of the program.
pub fn use_detector(detector: Box<Detector>) -> bool {
    DETECTOR.set(detector).is_ok()
}

/// Whether a voice detector has been set.
pub fn detector_in_use() -> bool {
    DETECTOR.get().is_some()
}

fn detected(samples: &[f32]) -> Option<Vec<(f32, f32)>> {
    DETECTOR.get().and_then(|d| d(samples))
}

/// Is there speech in some audio?
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Heard {
    /// Loud and clear, or the voice detector found it.
    Speech,
    /// Neither judge hears speech, but it is not silent either: Whisper decides, and what it is
    /// unsure of is dropped (`engine::confident`) - it invents words for noise ("Thank you.").
    Unsure,
    /// Nothing at all: never sent to Whisper.
    Silence,
}

pub fn hear_speech(samples: &[f32]) -> Heard {
    let rms = window_rms(samples);
    if rms.iter().any(|r| *r >= SILENCE_FLOOR * 2.0) {
        return Heard::Speech;
    }
    let above = (room_level(&rms) * 4.0).max(NOTHING_AT_ALL);
    if rms.iter().filter(|r| **r >= above).count() >= 5 {
        return Heard::Speech;
    }
    if detected(samples).is_some_and(|spans| !spans.is_empty()) {
        return Heard::Speech;
    }
    if rms.iter().all(|r| *r < NOTHING_AT_ALL) {
        return Heard::Silence;
    }
    Heard::Unsure
}

/// Is there certainly speech in this audio? (`hear_speech` for the three answers.)
pub fn has_speech(samples: &[f32]) -> bool {
    hear_speech(samples) == Heard::Speech
}

/// Quiet by loudness alone, for when no voice detector is set: back down to the room, and never
/// above 2% of his loudest - with no gap earlier in the audio, the "room" may be his own soft
/// last sound (Codex's seventh review), so it is better to wait a little longer than to cut it.
fn quiet_level(rms: &[f32]) -> Option<f32> {
    let mut sorted = rms.to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let loud = *sorted.get(sorted.len() * 9 / 10)?;
    Some((room_level(rms) * 3.0).min(loud * 0.02).max(NOTHING_AT_ALL))
}

/// Has he stopped talking? True when the last `quiet_secs` of `samples` (16 kHz) hold no speech.
///
/// The stop press waits for this before the microphone closes: pressed while the last word was
/// still being said, the end of it used to be cut off. Measured 2026-10-01 on quick phrases:
/// missing the last 0.2 s took base.en from 6% of words wrong to 12%, the last 0.3 s to 21%.
pub fn voice_has_stopped(samples: &[f32], quiet_secs: f32, careful: bool) -> bool {
    stopped_given(detected(samples).as_deref(), samples, quiet_secs, careful)
}

/// `voice_has_stopped`, with the detector's answer given. The detector saying "no speech lately"
/// is only a candidate: the sound itself must agree (conservatively) - a quiet voice the detector
/// missed is not an ending (Codex's eighth review). In a noisy room this waits the stop's full
/// hangover; that is the price of the last sound.
///
/// `careful`: the room measured before he spoke is not enough on its own - the careful loudness
/// rule must agree as well, so a last word fainter than the room's hiss, which the detector missed,
/// is not cut (Codex's eleventh review). On a hissy headset that is the stop's full 1.2 s.
pub fn stopped_given(spans: Option<&[(f32, f32)]>, samples: &[f32], quiet_secs: f32, careful: bool) -> bool {
    let total = samples.len() as f32 / WHISPER_SAMPLE_RATE as f32;
    if total < quiet_secs {
        return false;
    }
    let rms = window_rms(samples);
    let need = ((quiet_secs * WHISPER_SAMPLE_RATE as f32) as usize).div_ceil(WINDOW).max(1);
    // With the detector, the sound has only to be back down to the room: the detector is what
    // hears a soft last sound. Without it, loudness must also be conservative (`quiet_level`).
    // Measured on his headset microphone 2026-10-02: its hiss sits 20 dB under his voice, so "2%
    // of his loudest" was below the hiss itself and every stop waited the full 1.2 s.
    // The room is measured only where the detector is sure there is no speech: before the last
    // stretch of speech began (the silence before he spoke, or a pause). Never from the ending
    // being judged - a soft last syllable the detector missed would become "the room" (Codex's
    // tenth review). With no such silence, loudness stays careful (`quiet_level`).
    let quiet = match spans {
        Some(spans) if !careful => {
            room_before(spans, &rms).map(|room| (room * 3.0).max(NOTHING_AT_ALL)).or_else(|| quiet_level(&rms))
        }
        Some(_) => quiet_level(&rms),
        None => quiet_level(&rms),
    };
    let quiet_by_sound =
        quiet.is_some_and(|quiet| rms.len() >= need && rms[rms.len() - need..].iter().all(|r| *r < quiet));
    let quiet_by_detector = spans.is_none_or(|spans| spans.last().is_none_or(|&(_, end)| total - end >= quiet_secs));
    quiet_by_detector && quiet_by_sound
}

/// The room's level, from windows the detector places outside speech and before the last stretch
/// of it began - at least 0.1 s of them - or `None`.
fn room_before(spans: &[(f32, f32)], rms: &[f32]) -> Option<f32> {
    let &(last_start, _) = spans.last()?;
    let secs = |i: usize| (i * WINDOW) as f32 / WHISPER_SAMPLE_RATE as f32;
    let mut outside: Vec<f32> = rms
        .iter()
        .enumerate()
        .filter(|&(i, _)| secs(i + 1) <= last_start && !spans.iter().any(|&(a, b)| secs(i + 1) > a && secs(i) < b))
        .map(|(_, r)| *r)
        .collect();
    if outside.len() < 5 {
        return None;
    }
    outside.sort_by(|a, b| a.total_cmp(b));
    Some(outside[outside.len() / 2])
}

/// The middle of the last gap of at least `gap_secs` between two stretches of speech, starting
/// at least `after_secs` in - a place to cut where no word is. `None` if there is none. A gap
/// still running at the very end does not count: he may only be drawing breath.
pub fn last_pause(samples: &[f32], after_secs: f32, gap_secs: f32) -> Option<usize> {
    pause_given(detected(samples).as_deref(), samples, after_secs, gap_secs)
}

/// `last_pause`, with the detector's answer given. A gap the detector reports must also be
/// clearly quieter than the speech either side of it - in a loud room, a missed quiet word looks
/// like a gap to the detector (Codex's eighth review).
pub fn pause_given(spans: Option<&[(f32, f32)]>, samples: &[f32], after_secs: f32, gap_secs: f32) -> Option<usize> {
    let rate = WHISPER_SAMPLE_RATE as f32;
    let rms = window_rms(samples);
    let level = |from: f32, to: f32| {
        let (a, b) = (((from * rate) as usize / WINDOW).min(rms.len()), ((to * rate) as usize / WINDOW).min(rms.len()));
        (b > a).then(|| rms[a..b].iter().sum::<f32>() / (b - a) as f32)
    };
    if let Some(spans) = spans {
        let speech = spans.iter().filter_map(|&(a, b)| level(a, b)).fold(0.0f32, f32::max);
        return spans
            .windows(2)
            .filter(|w| w[0].1 >= after_secs && w[1].0 - w[0].1 >= gap_secs)
            .filter(|w| level(w[0].1, w[1].0).is_some_and(|gap| gap < speech * 0.35))
            .last()
            .map(|w| ((w[0].1 + w[1].0) / 2.0 * rate) as usize);
    }
    let after = (after_secs * rate) as usize / WINDOW;
    let gap = ((gap_secs * rate) as usize / WINDOW).max(1);
    let quiet = quiet_level(&rms)?;
    let (mut best, mut run) = (None, 0usize);
    for (i, level) in rms.iter().enumerate() {
        if *level < quiet {
            run += 1;
            continue;
        }
        if run >= gap && i - run >= after {
            best = Some((i - run / 2) * WINDOW);
        }
        run = 0;
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `secs` of speech-like sound, then `gap` of silence, repeated.
    fn talk(pattern: &[(f32, bool)]) -> Vec<f32> {
        let mut out = Vec::new();
        for (secs, loud) in pattern {
            let n = (secs * 16_000.0) as usize;
            out.extend((0..n).map(|i| if *loud { 0.2 * ((i as f32) * 0.3).sin() } else { 0.0 }));
        }
        out
    }

    #[test]
    fn a_stretch_ends_in_the_middle_of_a_pause() {
        let audio = talk(&[(4.0, true), (0.6, false), (1.0, true)]);
        let cut = commit_point(&audio, 3.0, 12.0).expect("the pause is a place to cut");
        let secs = cut as f32 / 16_000.0;
        assert!((4.1..4.5).contains(&secs), "cut at {secs}s");
    }

    #[test]
    fn nothing_is_cut_too_early_or_mid_breath() {
        // Too short to be worth a pass on its own.
        assert_eq!(commit_point(&talk(&[(1.5, true), (0.6, false), (0.5, true)]), 3.0, 12.0), None);
        // A pause still running at the end may just be a breath.
        assert_eq!(commit_point(&talk(&[(4.0, true), (0.6, false)]), 3.0, 12.0), None);
        // A short gap between words is not a pause.
        assert_eq!(commit_point(&talk(&[(4.0, true), (0.1, false), (1.0, true)]), 3.0, 12.0), None);
    }

    #[test]
    fn talking_without_a_pause_is_cut_at_the_quietest_moment() {
        let mut audio = talk(&[(13.0, true)]);
        let dip = 6 * 16_000;
        for s in &mut audio[dip..dip + 640] {
            *s *= 0.3;
        }
        let cut = commit_point(&audio, 3.0, 12.0).expect("forced by the length");
        assert!((dip..dip + 640).contains(&cut), "cut at {cut}");
    }

    #[test]
    fn the_stop_press_waits_while_he_is_still_saying_the_last_word() {
        // Still talking at the very end: keep listening.
        assert!(!voice_has_stopped(&talk(&[(2.0, true)]), 0.15, false));
        // A short gap inside a word or between two is not the end.
        assert!(!voice_has_stopped(&talk(&[(2.0, true), (0.08, false)]), 0.15, false));
        // Quiet for long enough: done.
        assert!(voice_has_stopped(&talk(&[(2.0, true), (0.2, false)]), 0.15, false));
        // A soft ending is still talking: far quieter than his loudest, but above the room.
        let mut soft = talk(&[(2.0, true), (0.3, false), (0.5, true)]);
        let end = soft.len();
        soft[end - 8_000..].iter_mut().for_each(|s| *s *= 0.05);
        assert!(!voice_has_stopped(&soft, 0.25, false), "a soft last sound is not silence");
        // Codex's seventh review: the same with no gap earlier, so the soft sound itself is the
        // quietest thing in the audio.
        let mut no_gap = talk(&[(1.0, true), (0.3, true)]);
        let end = no_gap.len();
        no_gap[end - 4_800..].iter_mut().for_each(|s| *s *= 0.05);
        assert!(!voice_has_stopped(&no_gap, 0.25, false), "the soft sound is not the room");
        assert_eq!(last_pause(&no_gap, 0.5, 0.15), None, "nor a place to cut");
        // Quiet relative to his own voice, not to an absolute level: a steady hum underneath
        // still counts as quiet once the words stop.
        let mut hum = talk(&[(2.0, true), (0.2, false)]);
        for (i, s) in hum.iter_mut().enumerate() {
            *s += 0.002 * ((i as f32) * 0.05).sin();
        }
        assert!(voice_has_stopped(&hum, 0.15, false));
        // Too little audio to tell.
        assert!(!voice_has_stopped(&[], 0.15, false));
    }

    /// Codex's eighth review: the detector missed a quiet voice still going, and the stop closed.
    #[test]
    fn the_detector_saying_stopped_is_not_enough_while_the_sound_goes_on() {
        let still_talking = talk(&[(1.0, true), (0.5, false), (1.0, true)]);
        assert!(!stopped_given(Some(&[]), &still_talking, 0.25, false), "the detector missed it; the sound did not");
        let done = talk(&[(1.0, true), (0.5, false)]);
        assert!(stopped_given(Some(&[(0.0, 1.0)]), &done, 0.25, false));
        assert!(!stopped_given(Some(&[(0.0, 1.45)]), &done, 0.25, false), "the detector still hears him");
        // His headset microphone: a steady hiss 20 dB under his voice. Back down to it, with the
        // detector agreeing, is stopped - not a wait for a silence that never comes.
        let hiss: Vec<f32> = (0..28_800).map(|i| 0.014 * ((i as f32) * 1.7).sin()).collect();
        let mut noisy = talk(&[(0.3, false), (1.0, true), (0.5, false)]);
        noisy.iter_mut().zip(&hiss).for_each(|(s, h)| *s += h);
        assert!(stopped_given(Some(&[(0.3, 1.3)]), &noisy, 0.25, false), "the room heard before he spoke");
        assert!(!stopped_given(Some(&[(0.3, 1.3)]), &noisy, 0.25, true), "careful: waits the full moment");
        assert!(!stopped_given(None, &noisy, 0.25, false), "without the detector, loudness stays careful");
        // Codex's tenth review: the detector missed a soft last syllable, with no silence before
        // the speech to measure the room in - careful loudness decides, and it is still talking.
        let mut soft = talk(&[(1.0, true), (0.3, true)]);
        let end = soft.len();
        soft[end - 4_800..].iter_mut().for_each(|s| *s *= 0.05);
        assert!(!stopped_given(Some(&[(0.0, 1.0)]), &soft, 0.25, false));
        assert!(!stopped_given(Some(&[]), &soft, 0.25, false), "an empty answer is no proof either");
    }

    #[test]
    fn a_gap_the_detector_reports_must_be_quiet_in_the_sound_too() {
        let audio = talk(&[(6.0, true), (0.4, false), (3.0, true)]);
        assert!(pause_given(Some(&[(0.0, 6.0), (6.4, 9.4)]), &audio, 5.0, 0.15).is_some());
        // The same "gap", but the sound never dropped: a missed quiet word, not a pause.
        let loud = talk(&[(9.4, true)]);
        assert_eq!(pause_given(Some(&[(0.0, 6.0), (6.4, 9.4)]), &loud, 5.0, 0.15), None);
    }

    #[test]
    fn the_last_pause_is_found_but_not_one_still_running_or_too_early() {
        let audio = talk(&[(6.0, true), (0.5, false), (4.0, true), (0.6, false), (3.0, true)]);
        let at = last_pause(&audio, 5.0, 0.3).expect("two pauses") as f32 / 16_000.0;
        assert!((10.6..10.9).contains(&at), "the later one, mid-pause: {at}");
        assert_eq!(last_pause(&talk(&[(2.0, true), (0.5, false), (9.0, true)]), 5.0, 0.3), None, "too early");
        assert_eq!(last_pause(&talk(&[(9.0, true)]), 5.0, 0.3), None, "never paused");
        // A gap between two sentences said quickly is still a place no word is.
        let quick = talk(&[(6.0, true), (0.18, false), (6.0, true)]);
        assert_eq!(last_pause(&quick, 5.0, 0.3), None);
        assert!(last_pause(&quick, 5.0, 0.15).is_some());
    }

    #[test]
    fn silence_is_never_sent_for_recognition() {
        assert!(!has_speech(&talk(&[(2.0, false)])));
        assert!(has_speech(&talk(&[(1.0, false), (0.5, true)])));
    }

    fn scaled(x: Vec<f32>, by: f32) -> Vec<f32> {
        x.into_iter().map(|s| s * by).collect()
    }

    fn hiss(n: usize, level: f32) -> Vec<f32> {
        let mut s = 0x2545_F491_4F6C_DD1Du64;
        (0..n)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                level * ((s >> 11) as f32 / (1u64 << 53) as f32 - 0.5) * 3.4
            })
            .collect()
    }

    /// Codex's sixth review: speech at 1% and 0.3% of normal volume, which Whisper heard, was
    /// thrown away as silence.
    #[test]
    fn quiet_speech_is_heard_however_low_the_microphone() {
        let phrase = talk(&[(0.4, false), (0.6, true), (0.2, false), (0.5, true), (0.3, false)]);
        assert!(has_speech(&scaled(phrase.clone(), 0.01)));
        assert!(has_speech(&scaled(phrase.clone(), 0.003)));
        // Quiet speech over a steady fan.
        let fan: Vec<f32> = (0..phrase.len()).map(|i| 0.0015 * ((i as f32) * 0.02).sin()).collect();
        let over_fan: Vec<f32> = scaled(phrase, 0.03).iter().zip(&fan).map(|(a, b)| a + b).collect();
        assert!(has_speech(&over_fan));
    }

    #[test]
    fn nothing_at_all_is_silence_and_faint_sound_is_left_to_whisper() {
        assert_eq!(hear_speech(&vec![0.0; 16_000]), Heard::Silence);
        assert_eq!(hear_speech(&hiss(32_000, 0.001)), Heard::Unsure, "not speech for sure, not silence");
    }

    #[test]
    fn steady_noise_and_a_single_click_are_not_speech() {
        assert!(!has_speech(&hiss(32_000, 0.001)), "a microphone's hiss");
        let fan: Vec<f32> = (0..32_000).map(|i| 0.002 * ((i as f32) * 0.02).sin()).collect();
        assert!(!has_speech(&fan), "a fan");
        let mut click = vec![0.0f32; 32_000];
        click[16_000..16_100].iter_mut().for_each(|s| *s = 0.003);
        assert!(!has_speech(&click), "a click");
    }

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
        let input: Vec<f32> = (0..4800).map(|i| i as f32 / 4800.0).collect();
        let out = resample(&input, 48_000, 16_000);
        assert_eq!(out.len(), 1600);
    }

    #[test]
    fn resampling_to_the_same_rate_changes_nothing() {
        let input = vec![0.1, 0.2, 0.3];
        assert_eq!(resample(&input, 16_000, 16_000), input);
    }

    fn tone(hz: f32, rate: u32, secs: f32) -> Vec<f32> {
        let n = (secs * rate as f32) as usize;
        (0..n).map(|i| (2.0 * std::f32::consts::PI * hz * i as f32 / rate as f32).sin()).collect()
    }

    /// Loudness away from the ends, where the filter meets silence.
    fn middle_rms(x: &[f32]) -> f32 {
        let m = &x[x.len() / 4..x.len() * 3 / 4];
        (m.iter().map(|v| v * v).sum::<f32>() / m.len() as f32).sqrt()
    }

    #[test]
    fn a_voice_passes_through_at_the_same_loudness() {
        for rate in [48_000, 44_100, 32_000, 24_000, 16_000, 8_000] {
            // A narrowband (8 kHz) microphone carries nothing above 4 kHz to begin with.
            let tones: &[f32] = if rate == 8_000 { &[200.0, 1_000.0] } else { &[200.0, 1_000.0, 3_500.0] };
            for &hz in tones {
                let out = resample(&tone(hz, rate, 0.5), rate, 16_000);
                let level = middle_rms(&out) / std::f32::consts::FRAC_1_SQRT_2;
                assert!((0.97..1.03).contains(&level), "{hz} Hz from {rate}: level {level}");
            }
        }
    }

    /// The bug until 2026-09-30: taking every third sample folded an 11 kHz hiss down to 5 kHz,
    /// at full strength, right among the voice's own sounds.
    #[test]
    fn what_whisper_cannot_hear_is_removed_not_folded_into_the_voice() {
        for rate in [48_000, 44_100] {
            let out = resample(&tone(11_000.0, rate, 0.5), rate, 16_000);
            assert!(middle_rms(&out) < 0.001, "11 kHz from {rate} came through at {}", middle_rms(&out));
        }
    }

    #[test]
    fn empty_input_is_handled_everywhere() {
        assert!(resample(&[], 48_000, 16_000).is_empty());
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
