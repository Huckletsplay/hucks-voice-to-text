//! End-to-end check of the local recognition path.
//!
//! This test speaks a known sentence with the macOS `say` command, conditions the audio exactly
//! as the microphone path does, runs it through the real Whisper model, and checks the words
//! come back. It is the difference between "the code compiles" and "dictation works".
//!
//! It skips rather than fails when the model or the tools are absent, so a fresh clone with no
//! 141 MB download still has a green test suite.

use hvtt_core::engine::Transcriber;
use std::path::PathBuf;
use std::process::Command;

fn model_path() -> Option<PathBuf> {
    let p = hvtt_core::paths::models_dir()?.join("ggml-base.en.bin");
    p.exists().then_some(p)
}

/// Synthesize a sentence to 16 kHz mono f32, the format recognition expects.
fn speak(text: &str, tag: &str) -> Option<Vec<f32>> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let dir = std::env::temp_dir();
    let aiff = dir.join(format!("hvtt-test-{tag}.aiff"));
    let wav = dir.join(format!("hvtt-test-{tag}.wav"));

    let said = Command::new("say")
        .args(["-o", aiff.to_str()?, text])
        .status()
        .ok()?;
    if !said.success() {
        return None;
    }

    let converted = Command::new("ffmpeg")
        .args(["-y", "-i", aiff.to_str()?, "-ar", "16000", "-ac", "1", wav.to_str()?])
        .output()
        .ok()?;
    if !converted.status.success() {
        return None;
    }

    let mut reader = hound::WavReader::open(&wav).ok()?;
    let spec = reader.spec();
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader.samples::<f32>().filter_map(|s| s.ok()).collect(),
        hound::SampleFormat::Int => reader
            .samples::<i16>()
            .filter_map(|s| s.ok())
            .map(|s| s as f32 / i16::MAX as f32)
            .collect(),
    };

    let _ = std::fs::remove_file(&aiff);
    let _ = std::fs::remove_file(&wav);

    // Run it through the same conditioning the microphone path uses.
    Some(hvtt_core::audio::condition(&samples, spec.channels, spec.sample_rate))
}

#[test]
fn a_spoken_sentence_is_transcribed_locally() {
    let Some(model) = model_path() else {
        eprintln!("skipping: no model - run scripts/fetch-model.sh base.en");
        return;
    };
    let Some(samples) = speak("The quick brown fox jumps over the lazy dog.", "fox") else {
        eprintln!("skipping: `say` or `ffmpeg` unavailable");
        return;
    };

    assert!(
        hvtt_core::audio::is_long_enough(samples.len(), hvtt_core::audio::WHISPER_SAMPLE_RATE),
        "synthesized audio was too short to be a fair test"
    );

    let engine = hvtt_desktop::engine_whisper::WhisperEngine::load(&model)
        .expect("the model should load");

    let result = engine
        .transcribe(&hvtt_core::engine::TranscriptionRequest {
            samples,
            vocabulary_prompt: None,
        })
        .expect("recognition should succeed");

    let got = result.text.to_lowercase();
    eprintln!("transcribed in {} ms: {:?}", result.elapsed_ms, result.text);

    for word in ["quick", "brown", "fox", "lazy", "dog"] {
        assert!(got.contains(word), "expected {word:?} in transcript, got {got:?}");
    }
}

#[test]
fn a_transcript_reaches_the_clipboard_even_when_delivery_fails() {
    // The product rule, exercised against the real completion sequence rather than a mock of
    // it: a destination that fails must still leave the words recoverable.
    use hvtt_core::pipeline::{Clipboard, DeliveryError, Destination, Liveness};
    use std::sync::Mutex;

    struct Clip(Mutex<String>);
    impl Clipboard for Clip {
        fn set_text(&self, t: &str) -> Result<(), String> {
            *self.0.lock().unwrap() = t.to_string();
            Ok(())
        }
    }
    struct Dead;
    impl Destination for Dead {
        fn label(&self) -> String { "a window that closed".into() }
        fn is_alive(&self) -> Liveness { Liveness::dead("window-closed") }
        fn deliver(&self, _: &str) -> Result<(), DeliveryError> {
            panic!("deliver must never be called for a dead destination");
        }
    }

    let dir = std::env::temp_dir().join(format!(
        "hvtt-e2e-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let drafts = hvtt_core::drafts::DraftStore::new(&dir, 5);
    let clip = Clip(Mutex::new(String::new()));
    let transcript = hvtt_core::transcript::Transcript::settled("do not lose these words");

    let report =
        hvtt_core::complete_transcription(&transcript, &clip, Some(&Dead), Some(&drafts));

    assert_eq!(*clip.0.lock().unwrap(), "do not lose these words");
    assert!(report.text_is_safe());
    let draft = report.draft_path.expect("a recovery draft should exist on disk");
    assert_eq!(std::fs::read_to_string(&draft).unwrap(), "do not lose these words");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn the_real_ax_destination_refuses_when_accessibility_is_not_granted() {
    // Not a mock: this calls the production capture path. Whichever way the permission falls on
    // this machine, it must never panic and never return a destination it cannot verify.
    match hvtt_desktop::destination::macos_ax::capture_focused() {
        Err(reason) => {
            assert!(!reason.is_empty(), "a refusal must say why");
        }
        Ok(dest) => {
            use hvtt_core::pipeline::Destination;
            // If it did capture something, it must be a text field and answer a liveness probe.
            assert!(!dest.label().is_empty());
            let _ = dest.is_alive();
            assert!(
                matches!(dest.role(), "AXTextArea" | "AXTextField" | "AXComboBox"),
                "only text fields are ever pinned, got {}",
                dest.role()
            );
        }
    }
}
