//! The recognition engine seam.
//!
//! Milestone 2 picks the engine by measurement, not on paper — `VOICE_TO_TEXT.md` lists Whisper,
//! Parakeet, Moonshine and others as live candidates. This trait is what makes that a swap
//! rather than a rewrite: nothing above it knows which engine is underneath.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct TranscriptionRequest {
    /// 16 kHz mono f32, already conditioned by [`crate::audio::condition`].
    pub samples: Vec<f32>,
    /// Bias the recogniser toward the user's own jargon.
    pub vocabulary_prompt: Option<String>,
    /// Abandon this recognition part-way when it is no longer wanted.
    pub give_up: Option<GiveUp>,
    /// A live pass, shown while he talks and replaced moments later: one quick attempt with a
    /// length limit, never Whisper's slow retries. The words that are sent never use this.
    pub provisional: bool,
}

/// Lets a recognition already running be abandoned: it stops once `counter` no longer holds
/// `value`. The passes made while he talks carry one, so pausing or sending never waits behind
/// words that are about to be recognised again anyway.
#[derive(Debug, Clone)]
pub struct GiveUp {
    pub counter: Arc<AtomicU64>,
    pub value: u64,
}

impl GiveUp {
    pub fn now(&self) -> bool {
        self.counter.load(Ordering::SeqCst) != self.value
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TranscriptionResult {
    pub text: String,
    /// The same words, a sentence (Whisper's segment) at a time, with where each lies in the
    /// request's samples - which is what lets a window be kept up to its last whole sentence.
    pub sentences: Vec<Sentence>,
    /// Wall-clock recognition time, retained so normal use exposes latency regressions.
    pub elapsed_ms: u128,
}

/// One of Whisper's segments: its words, and where it starts and ends, in seconds from the start
/// of the request's samples.
#[derive(Debug, Clone, PartialEq)]
pub struct Sentence {
    pub start: f32,
    pub end: f32,
    pub text: String,
    /// Whisper's own judgement that there was no speech here, 0-1.
    pub no_speech: f32,
}

/// What Whisper writes when it hears only noise - learned from video subtitles.
const NOISE_WORDS: &[&str] =
    &["thank you", "thank you so much", "thanks for watching", "thank you for watching", "you", "bye"];

/// The words of a recognition worth keeping. Whisper has already dropped what it judged to be no
/// speech, by its own rule (no-speech probability *and* a weak decoding together); a second veto
/// on the probability alone threw away correct quiet sentences (Codex's eighth review: rated 0.61
/// and 0.67 and right). Only on audio neither judge called speech (`Heard::Unsure`), a result that
/// is nothing but one of Whisper's noise phrases ("Thank you.") is taken for noise. Raw: hesitation
/// dots are dealt with by whoever knows whether the words end here.
pub fn confident(result: &TranscriptionResult, heard: crate::audio::Heard) -> String {
    let said = result
        .text
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect::<String>();
    let said = said.split_whitespace().collect::<Vec<_>>().join(" ");
    if heard == crate::audio::Heard::Unsure && NOISE_WORDS.contains(&said.as_str()) {
        return String::new();
    }
    result.text.clone()
}

/// How much is recognised for good at a time while he talks: one of Whisper's 30-second windows,
/// less the lead-in. Whisper goes through a long recording this way itself; doing it while he
/// talks leaves only the last window for the stop press. Measured 2026-10-01 on 30-60 s
/// dictations (`rolling` in `examples/accuracy.rs`), share of words wrong against hearing the
/// whole dictation at the stop: base.en 3.6% / 3.3%, small.en 2.0% / 1.8% - a few words in ten
/// dictations - with the stop's wait two to three times shorter. Shorter than this, the whole
/// dictation is heard at the stop, as one window.
pub const WINDOW_SECS: f32 = 29.0;

/// What a window recognised for good gives back.
#[derive(Debug, Clone, PartialEq)]
pub enum Settled {
    /// Its words up to the end of its last whole sentence, and how far that is (16 kHz samples).
    Keep { text: String, upto: usize },
    /// Not sure where its words end: nothing is kept and nothing skipped - the stop press, or a
    /// pause, recognises all of it (Codex's sixth review).
    Unsure,
}

/// A window that is finished only if a pause of at least this long follows its one sentence.
const FINISHED_GAP_SECS: f32 = 1.0;

/// Of a window recognised for good: its words up to the end of its last whole sentence. The last
/// sentence may still be running when the window closes, so it is left for the next window. Only
/// sure answers are kept - times that make sense, a sentence that clearly ended - because what is
/// kept is never heard again: Whisper returning nothing for speech, times that are missing or out
/// of order, or one sentence running into the window's end are [`Settled::Unsure`].
pub fn settle(sentences: &[Sentence], window_samples: usize) -> Settled {
    let rate = crate::audio::WHISPER_SAMPLE_RATE as f32;
    let window = window_samples as f32 / rate;
    let keep = match sentences.len() {
        0 => return Settled::Unsure,
        1 => 1,
        n => n - 1,
    };
    let kept = &sentences[..keep];
    // The kept sentences' times must make sense; the one left for the next window may say
    // anything about its end (Whisper often has it run on into the window's silent padding).
    let sensible = kept.iter().all(|s| {
        s.start.is_finite() && s.end.is_finite() && 0.0 <= s.start && s.start <= s.end && s.end <= window
    }) && kept.windows(2).all(|w| w[0].start <= w[1].start && w[0].end <= w[1].end);
    let end = kept[keep - 1].end;
    if !sensible || end <= 0.0 {
        return Settled::Unsure;
    }
    // One sentence is finished only if a pause follows it.
    if keep == 1 && sentences.len() == 1 && end + FINISHED_GAP_SECS > window {
        return Settled::Unsure;
    }
    // The next sentence must start where the kept ones end, or later: an overlap means the times
    // contradict each other, and cutting back would hear kept words again (Codex's seventh
    // review). Unsure instead - `window_step` then cuts at a gap in the sound itself.
    match sentences.get(keep) {
        Some(next) if next.start.is_finite() && next.start + 0.05 >= end => {}
        Some(_) => return Settled::Unsure,
        None => {}
    }
    let text = strip_ellipses(
        &tidy(&sentences[..keep].iter().map(|s| s.text.as_str()).collect::<Vec<_>>().join("\n")),
        false,
    );
    Settled::Keep { text, upto: ((end * rate) as usize).min(window_samples) }
}

/// What one window gives, once its handling below is done.
#[derive(Debug, Clone, PartialEq)]
pub enum Window {
    /// Words kept for good, and how far they reach (16 kHz samples).
    Keep { text: String, upto: usize },
    /// Nothing could be kept for sure - not even up to a pause: left whole for the stop.
    Hold,
}

/// The rolling rule, in one place for the live passes, the stop press, the tests and the bench.
/// `hear` recognises audio given the words before it.
///
/// A window with no speech moves on (keeping its last five seconds). Otherwise it is kept up to
/// its last whole sentence (`settle`). When that is unsure - typically one long sentence filling
/// the window, which Whisper does for continuous speech - the window is heard again only up to
/// the last gap of at least 0.15 s (between sentences said quickly, or between words) and kept
/// whole: it ends in silence, so no word is cut. Only a window with no such gap is held.
/// (2026-10-01: holding every unsure window left long stretches to Whisper's own long-form at the
/// stop, which dropped 100 words of a minute under hiss.)
pub fn window_step<E>(
    window: &[f32],
    before: &str,
    hear: &mut impl FnMut(&[f32], &str) -> Result<TranscriptionResult, E>,
) -> Result<Window, E> {
    use crate::audio::Heard;
    let rate = crate::audio::WHISPER_SAMPLE_RATE as usize;
    let nothing = Window::Keep { text: String::new(), upto: window.len().saturating_sub(5 * rate) };
    let speech = crate::audio::hear_speech(window);
    if speech == Heard::Silence {
        return Ok(nothing);
    }
    let result = hear(window, before)?;
    if speech == Heard::Unsure && confident(&result, speech).trim().is_empty() {
        // Neither judge heard speech, and Whisper found none (or only its noise phrases).
        return Ok(nothing);
    }
    if let Settled::Keep { text, upto } = settle(&result.sentences, window.len()) {
        return Ok(Window::Keep { text, upto });
    }
    let Some(pause) = crate::audio::last_pause(window, 5.0, 0.15) else { return Ok(Window::Hold) };
    let part = &window[..pause];
    let heard = confident(&hear(part, before)?, crate::audio::hear_speech(part));
    if heard.trim().is_empty() && crate::audio::has_speech(part) {
        return Ok(Window::Hold);
    }
    Ok(Window::Keep { text: strip_ellipses(&heard, false), upto: pause })
}

/// What [`recognise_all`] heard.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AllHeard {
    /// The words, with hesitation dots dealt with (they end here, unless `unfinished_from`).
    pub text: String,
    /// Seconds that held speech but gave no words, even asked twice. Never passed over quietly:
    /// the program says so (Codex's seventh review).
    pub unrecognised_secs: f32,
    /// Recognition failed here (16 kHz samples into the audio): `text` holds everything before
    /// it, and only the rest needs another try (Codex's eighth review: an error used to throw the
    /// words already recognised away with it).
    pub unfinished_from: Option<usize>,
}

/// Everything in `samples`, recognised for good a window at a time with [`window_step`] - the
/// stop press's and a pause's way through what the live passes had not kept. Nothing is handed to
/// Whisper's own long-form: a window that would be held is kept whole here (its last word may be
/// split - far less harm than the hundred words long-form dropped). Speech that gives no words is
/// asked once more, then counted in `unrecognised_secs`. A recognition that fails ends the work
/// there, with the words so far and the error.
pub fn recognise_all<E>(
    samples: &[f32],
    before: &str,
    hear: &mut impl FnMut(&[f32], &str) -> Result<TranscriptionResult, E>,
) -> (AllHeard, Option<E>) {
    let rate = crate::audio::WHISPER_SAMPLE_RATE as f32;
    let window = (WINDOW_SECS * rate) as usize;
    let mut all = AllHeard::default();
    let so_far = |text: &str| format!("{before} {text}").trim().to_string();
    let mut at = 0usize;
    let mut text = String::new();
    let stop = |all: &mut AllHeard, text: &str, at: usize| {
        all.text = strip_ellipses(text, false);
        all.unfinished_from = Some(at);
    };
    while samples.len() - at > window {
        let part = &samples[at..at + window];
        let step = match window_step(part, &so_far(&text), hear) {
            Ok(step) => step,
            Err(e) => {
                stop(&mut all, &text, at);
                return (all, Some(e));
            }
        };
        let (words, upto) = match step {
            Window::Keep { text: words, upto } if upto > 0 => (words, upto),
            _ => match words_of(part, &so_far(&text), &mut all, hear) {
                Ok(words) => (strip_ellipses(&words, false), window),
                Err(e) => {
                    stop(&mut all, &text, at);
                    return (all, Some(e));
                }
            },
        };
        text = format!("{text} {words}").trim().to_string();
        at += upto;
    }
    match words_of(&samples[at..], &so_far(&text), &mut all, hear) {
        Ok(last) => {
            all.text = strip_ellipses(format!("{text} {last}").trim(), true);
            (all, None)
        }
        Err(e) => {
            stop(&mut all, &text, at);
            (all, Some(e))
        }
    }
}

/// Speech must give words: asked twice, then counted as unrecognised.
fn words_of<E>(
    part: &[f32],
    so_far: &str,
    all: &mut AllHeard,
    hear: &mut impl FnMut(&[f32], &str) -> Result<TranscriptionResult, E>,
) -> Result<String, E> {
    let speech = crate::audio::hear_speech(part);
    let rate = crate::audio::WHISPER_SAMPLE_RATE;
    if speech == crate::audio::Heard::Silence || !crate::audio::is_long_enough(part.len(), rate) {
        return Ok(String::new());
    }
    let mut words = confident(&hear(part, so_far)?, speech);
    if words.trim().is_empty() && speech == crate::audio::Heard::Speech {
        words = confident(&hear(part, so_far)?, speech);
        if words.trim().is_empty() {
            all.unrecognised_secs += part.len() as f32 / crate::audio::WHISPER_SAMPLE_RATE as f32;
        }
    }
    Ok(words)
}

/// Whisper writes "..." where he hesitated ("working on... accuracy"). Nobody types that in a
/// message (his call, 2026-10-01): between words it goes, and where a sentence ends - the text
/// ends, or a capital follows that is not "I" - it becomes a full stop.
pub fn without_ellipses(text: &str) -> String {
    strip_ellipses(text, true)
}

/// `finished`: the text ends there. Not at a window's hand-over, where more of the same sentence
/// may follow - trailing dots there just go (Codex's sixth review: "working on..." became
/// "working on." before "accuracy").
fn strip_ellipses(text: &str, finished: bool) -> String {
    let text = text.replace('…', "...");
    let mut out = String::with_capacity(text.len());
    let mut rest = text.as_str();
    while let Some(at) = rest.find("...") {
        out.push_str(rest[..at].trim_end());
        let next = rest[at..].trim_start_matches('.').trim_start();
        let first_word = next
            .split_whitespace()
            .next()
            .unwrap_or("")
            .trim_start_matches(|c: char| !c.is_alphanumeric());
        let is_i = first_word == "I"
            || first_word.starts_with("I'")
            || first_word.starts_with("I\u{2019}")
            || first_word.strip_prefix('I').is_some_and(|r| r.starts_with(|c: char| c.is_ascii_punctuation()));
        let starts_sentence = if next.is_empty() {
            finished
        } else {
            first_word.starts_with(char::is_uppercase) && !is_i
        };
        let punctuated = out.ends_with(['.', '!', '?', ',', ';', ':']);
        if starts_sentence && !out.is_empty() && !punctuated {
            out.push('.');
        }
        if !out.is_empty() && !next.is_empty() && !next.starts_with(['.', ',', ';', ':', '!', '?']) {
            out.push(' ');
        }
        rest = next;
    }
    out.push_str(rest);
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[derive(Debug)]
pub enum EngineError {
    /// The model file is absent. The user is told how to fetch it, not given a stack trace.
    ModelMissing { path: String },
    ModelLoad(String),
    Recognition(String),
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EngineError::ModelMissing { path } => write!(
                f,
                "No speech model found at {path}. Run app/scripts/fetch-model.sh to download one."
            ),
            EngineError::ModelLoad(e) => write!(f, "Could not load the speech model: {e}"),
            EngineError::Recognition(e) => write!(f, "Recognition failed: {e}"),
        }
    }
}

impl std::error::Error for EngineError {}

/// A loaded, resident recognition model.
///
/// Resident is deliberate: pay RAM rather than seconds so the hotkey responds instantly.
/// Implementations are expected to be created once at startup.
pub trait Transcriber: Send + Sync {
    fn name(&self) -> &str;
    fn transcribe(&self, req: &TranscriptionRequest) -> Result<TranscriptionResult, EngineError>;
}

/// Tidy the raw recogniser output.
///
/// Deliberately rule-based and tiny. The heavier restructuring is a *button* on the composer,
/// never a pipeline stage — that decision is recorded in `VOICE_TO_TEXT.md`.
pub fn tidy(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // Whisper marks non-speech as bracketed annotations; they are never wanted in a
        // dictated message.
        if line.starts_with('[') && line.ends_with(']') {
            continue;
        }
        if line.starts_with('(') && line.ends_with(')') {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(line);
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tidy_joins_whisper_segment_lines_into_one_paragraph() {
        let raw = " Hello there.\n This is a test.\n";
        assert_eq!(tidy(raw), "Hello there. This is a test.");
    }

    #[test]
    fn tidy_drops_non_speech_annotations() {
        let raw = "[BLANK_AUDIO]\n Real words here.\n(typing)";
        assert_eq!(tidy(raw), "Real words here.");
    }

    #[test]
    fn hesitation_dots_are_not_typed() {
        assert_eq!(
            without_ellipses("I want to start working on... accuracy and maybe... being able to."),
            "I want to start working on accuracy and maybe being able to."
        );
        assert_eq!(without_ellipses("Hold on... Let me check."), "Hold on. Let me check.");
        assert_eq!(without_ellipses("and... I think so"), "and I think so");
        assert_eq!(without_ellipses("and... I'm sure"), "and I'm sure");
        assert_eq!(without_ellipses("and... I\u{2019}m sure"), "and I\u{2019}m sure");
        assert_eq!(without_ellipses("and... I\u{2019}ll go"), "and I\u{2019}ll go");
        assert_eq!(without_ellipses("and... I, for one, agree"), "and I, for one, agree");
        assert_eq!(without_ellipses("Okay..."), "Okay.");
        assert_eq!(without_ellipses("Okay...."), "Okay.");
        assert_eq!(without_ellipses("So… what now?"), "So what now?");
        assert_eq!(without_ellipses("...and then it worked"), "and then it worked");
        assert_eq!(without_ellipses("wait..., really"), "wait, really");
        assert_eq!(without_ellipses("Done!... Next one."), "Done! Next one.");
        assert_eq!(without_ellipses("No dots here."), "No dots here.");
    }

    #[test]
    fn tidy_collapses_runs_of_whitespace() {
        assert_eq!(tidy("  too    many   spaces  "), "too many spaces");
    }

    #[test]
    fn tidy_of_silence_is_empty_not_a_panic() {
        assert_eq!(tidy("[BLANK_AUDIO]"), "");
        assert_eq!(tidy(""), "");
    }

    fn sentence(start: f32, end: f32, text: &str) -> Sentence {
        Sentence { start, end, text: text.into(), no_speech: 0.0 }
    }

    fn window() -> usize {
        (WINDOW_SECS * 16_000.0) as usize
    }

    #[test]
    fn a_window_is_kept_up_to_its_last_whole_sentence() {
        let found = [
            sentence(0.0, 9.5, "First thought."),
            sentence(9.8, 21.0, "Second thought."),
            sentence(21.4, 29.0, "A third one still going"),
        ];
        assert_eq!(
            settle(&found, window()),
            Settled::Keep { text: "First thought. Second thought.".into(), upto: 21 * 16_000 }
        );
    }

    #[test]
    fn one_sentence_is_kept_only_when_a_pause_shows_it_ended() {
        let ended = settle(&[sentence(0.0, 20.0, "Said and done.")], window());
        assert_eq!(ended, Settled::Keep { text: "Said and done.".into(), upto: 20 * 16_000 });
        // Running into the window's end: maybe mid-word. Kept for the stop, not cut here.
        assert_eq!(settle(&[sentence(0.0, 28.9, "He never stopped")], window()), Settled::Unsure);
    }

    /// Codex's sixth review: each of these used to skip or swallow audio for good.
    #[test]
    fn nothing_is_kept_on_an_answer_it_cannot_trust() {
        assert_eq!(settle(&[], window()), Settled::Unsure, "speech, and Whisper said nothing");
        let zero = [sentence(0.0, 0.0, "One."), sentence(0.0, 9.0, "Two.")];
        assert_eq!(settle(&zero, window()), Settled::Unsure, "a sentence ending at 0 s");
        let backwards = [sentence(5.0, 12.0, "One."), sentence(2.0, 9.0, "Two."), sentence(13.0, 20.0, "x")];
        assert_eq!(settle(&backwards, window()), Settled::Unsure, "out of order");
        let beyond = [sentence(0.0, 35.0, "One."), sentence(35.0, 40.0, "Two.")];
        assert_eq!(settle(&beyond, window()), Settled::Unsure, "a kept sentence past the window");
        let next_nan = [sentence(0.0, 5.0, "One."), sentence(f32::NAN, 9.0, "Two.")];
        assert_eq!(settle(&next_nan, window()), Settled::Unsure, "no idea where the next one starts");
        let nan = [sentence(0.0, f32::NAN, "One."), sentence(10.0, 20.0, "Two.")];
        assert_eq!(settle(&nan, window()), Settled::Unsure);
    }

    #[test]
    fn the_unfinished_sentence_may_run_on_past_the_window() {
        let found = [sentence(0.0, 12.0, "Kept."), sentence(12.5, 30.0, "Still going")];
        assert_eq!(settle(&found, window()), Settled::Keep { text: "Kept.".into(), upto: 12 * 16_000 });
    }

    #[test]
    fn overlapping_or_reversed_times_are_not_trusted() {
        let overlap = [sentence(0.0, 12.0, "Kept."), sentence(11.0, 20.0, "Overlapping")];
        assert_eq!(settle(&overlap, window()), Settled::Unsure, "an overlap would replay kept words");
        // Codex's seventh review: starts reversed, ends in order.
        let reversed = [sentence(5.0, 8.0, "One."), sentence(2.0, 9.0, "Two."), sentence(10.0, 20.0, "x")];
        assert_eq!(settle(&reversed, window()), Settled::Unsure);
    }

    fn result(sentences: &[Sentence]) -> TranscriptionResult {
        let text = tidy(&sentences.iter().map(|s| s.text.as_str()).collect::<Vec<_>>().join("\n"));
        TranscriptionResult { text, sentences: sentences.to_vec(), elapsed_ms: 0 }
    }

    fn voice(secs: f32) -> Vec<f32> {
        (0..(secs * 16_000.0) as usize).map(|i| 0.2 * ((i as f32) * 0.3).sin()).collect()
    }

    /// Codex's seventh review: a speech window answered with nothing, twice, was passed over as if
    /// heard - only the last second came back, and nothing said so.
    #[test]
    fn speech_that_gives_no_words_is_reported_not_passed_over() {
        let audio = voice(30.0);
        let mut calls = 0;
        let mut hear = |part: &[f32], _: &str| -> Result<TranscriptionResult, ()> {
            calls += 1;
            // The last second is heard; every full window comes back empty.
            Ok(if part.len() < 2 * 16_000 { result(&[sentence(0.0, 0.9, "the end")]) } else { result(&[]) })
        };
        let (all, failed) = recognise_all(&audio, "", &mut hear);
        assert!(failed.is_none());
        assert_eq!(all.text, "the end");
        assert!(all.unrecognised_secs >= 28.9, "the empty window is counted: {}", all.unrecognised_secs);
        assert!(calls >= 3, "asked again before giving up");
    }

    fn faint() -> Vec<f32> {
        (0..16_000).map(|i| 0.001 * (((i * 7919) % 1000) as f32 / 1000.0 - 0.5)).collect()
    }

    #[test]
    fn noise_phrases_on_audio_nobody_called_speech_give_no_words() {
        let mut hear = |_: &[f32], _: &str| -> Result<TranscriptionResult, ()> {
            Ok(result(&[Sentence { start: 0.0, end: 1.0, text: "Thank you.".into(), no_speech: 0.9 }]))
        };
        assert_eq!(recognise_all(&faint(), "", &mut hear).0.text, "");
    }

    /// Codex's eighth review: quiet speech Whisper heard correctly, rated 0.61 and 0.67 "no
    /// speech", was thrown away by a second veto. Whisper's own rule already ran; its words stay.
    #[test]
    fn a_quiet_sentence_whisper_heard_is_kept_whatever_its_no_speech_rating() {
        let said = "Can you send me the file when you get a chance?";
        let mut hear = |_: &[f32], _: &str| -> Result<TranscriptionResult, ()> {
            Ok(result(&[Sentence { start: 0.0, end: 1.0, text: said.into(), no_speech: 0.67 }]))
        };
        assert_eq!(recognise_all(&faint(), "", &mut hear).0.text, said);
    }

    /// Codex's eighth review: a failure after words were recognised threw those words away too.
    #[test]
    fn a_failure_part_way_keeps_the_words_before_it_and_says_where() {
        let audio = voice(40.0);
        let mut calls = 0;
        let mut hear = |part: &[f32], _: &str| -> Result<TranscriptionResult, &str> {
            calls += 1;
            if calls == 1 {
                // The first window: two sentences, the first kept.
                assert_eq!(part.len(), (WINDOW_SECS * 16_000.0) as usize);
                Ok(result(&[sentence(0.0, 12.0, "First part..."), sentence(12.5, 28.0, "x")]))
            } else {
                Err("the engine failed")
            }
        };
        let (all, failed) = recognise_all(&audio, "", &mut hear);
        assert_eq!(failed, Some("the engine failed"));
        assert_eq!(all.text, "First part", "kept, and not ended with a full stop - more follows");
        assert_eq!(all.unfinished_from, Some(12 * 16_000));
    }

    #[test]
    fn the_last_words_lose_their_hesitation_dots_and_end_properly() {
        let mut hear = |_: &[f32], _: &str| -> Result<TranscriptionResult, ()> {
            Ok(result(&[sentence(0.0, 1.0, "I was going to...")]))
        };
        assert_eq!(recognise_all(&voice(1.0), "", &mut hear).0.text, "I was going to.");
    }

    #[test]
    fn a_hesitation_at_the_hand_over_is_not_turned_into_a_full_stop() {
        let found = [sentence(0.0, 8.0, "I want to start working on..."), sentence(8.5, 15.0, "accuracy now")];
        let Settled::Keep { text, .. } = settle(&found, window()) else { panic!("kept") };
        assert_eq!(text, "I want to start working on");
    }

    #[test]
    fn model_missing_error_tells_the_user_what_to_do() {
        let e = EngineError::ModelMissing { path: "/tmp/x.bin".into() };
        let msg = e.to_string();
        assert!(msg.contains("/tmp/x.bin"));
        assert!(msg.contains("fetch-model"), "must point at the fix, got: {msg}");
    }
}
