//! The recognition engine seam.
//!
//! Milestone 2 picks the engine by measurement, not on paper — `VOICE_TO_TEXT.md` lists Whisper,
//! Parakeet, Moonshine and others as live candidates. This trait is what makes that a swap
//! rather than a rewrite: nothing above it knows which engine is underneath.

use std::fmt;

#[derive(Debug, Clone)]
pub struct TranscriptionRequest {
    /// 16 kHz mono f32, already conditioned by [`crate::audio::condition`].
    pub samples: Vec<f32>,
    /// Bias the recogniser toward the user's own jargon.
    pub vocabulary_prompt: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TranscriptionResult {
    pub text: String,
    /// Wall-clock recognition time, retained so normal use exposes latency regressions.
    pub elapsed_ms: u128,
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
    fn tidy_collapses_runs_of_whitespace() {
        assert_eq!(tidy("  too    many   spaces  "), "too many spaces");
    }

    #[test]
    fn tidy_of_silence_is_empty_not_a_panic() {
        assert_eq!(tidy("[BLANK_AUDIO]"), "");
        assert_eq!(tidy(""), "");
    }

    #[test]
    fn model_missing_error_tells_the_user_what_to_do() {
        let e = EngineError::ModelMissing { path: "/tmp/x.bin".into() };
        let msg = e.to_string();
        assert!(msg.contains("/tmp/x.bin"));
        assert!(msg.contains("fetch-model"), "must point at the fix, got: {msg}");
    }
}
