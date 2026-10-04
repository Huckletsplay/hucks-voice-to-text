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

/// Recognition takes every core; two tests recognising at once slowed each other from seconds to
/// a minute and a half (2026-09-28). They take turns.
static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn model_path() -> Option<PathBuf> {
    let p = hvtt_core::paths::models_dir()?.join("ggml-base.en.bin");
    p.exists().then_some(p)
}

/// macOS: `say`, then ffmpeg to a 16 kHz mono WAV.
#[cfg(target_os = "macos")]
fn synthesize(text: &str, tag: &str, wav: &std::path::Path) -> Option<()> {
    let aiff = std::env::temp_dir().join(format!("hvtt-test-{tag}.aiff"));
    let said = Command::new("say").args(["-o", aiff.to_str()?, text]).status().ok()?;
    if !said.success() {
        return None;
    }
    let converted = Command::new("ffmpeg")
        .args(["-y", "-i", aiff.to_str()?, "-ar", "16000", "-ac", "1", wav.to_str()?])
        .output()
        .ok()?;
    let _ = std::fs::remove_file(&aiff);
    converted.status.success().then_some(())
}

/// Windows: its own speech synthesizer writes the 16 kHz mono WAV directly. No ffmpeg needed.
#[cfg(windows)]
fn synthesize(text: &str, _tag: &str, wav: &std::path::Path) -> Option<()> {
    let script = format!(
        "Add-Type -AssemblyName System.Speech; \
         $s = New-Object System.Speech.Synthesis.SpeechSynthesizer; \
         $f = New-Object System.Speech.AudioFormat.SpeechAudioFormatInfo(16000, \
              [System.Speech.AudioFormat.AudioBitsPerSample]::Sixteen, \
              [System.Speech.AudioFormat.AudioChannel]::Mono); \
         $s.SetOutputToWaveFile('{}', $f); $s.Speak('{}'); $s.Dispose()",
        wav.to_str()?.replace('\'', "''"),
        text.replace('\'', "''"),
    );
    let said = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output()
        .ok()?;
    said.status.success().then_some(())
}

#[cfg(not(any(target_os = "macos", windows)))]
fn synthesize(_: &str, _: &str, _: &std::path::Path) -> Option<()> {
    None
}

/// Synthesize a sentence to 16 kHz mono f32, the format recognition expects.
fn speak(text: &str, tag: &str) -> Option<Vec<f32>> {
    let wav = std::env::temp_dir().join(format!("hvtt-test-{tag}.wav"));
    synthesize(text, tag, &wav)?;

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

    let _ = std::fs::remove_file(&wav);

    // Run it through the same conditioning the microphone path uses.
    Some(hvtt_core::audio::condition(&samples, spec.channels, spec.sample_rate))
}

#[test]
fn a_spoken_sentence_is_transcribed_locally() {
    let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Some(model) = model_path() else {
        eprintln!("skipping: no model - run scripts/fetch-model.sh base.en");
        return;
    };
    let Some(samples) = speak("The quick brown fox jumps over the lazy dog.", "fox") else {
        eprintln!("skipping: no speech synthesizer (macOS `say` + ffmpeg, or Windows System.Speech)");
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
            give_up: None,
            provisional: false,
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
        fn deliver(&self, _: &str, _: bool) -> Result<(), DeliveryError> {
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
#[cfg(target_os = "macos")]
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

/// The model inside the program ("Best" on the Mac, "Quick" on Windows) hears a quick phrase - the
/// rest of these tests use base.en for speed, and a fresh setup now fetches only the default
/// (Codex's thirteenth review).
#[test]
fn the_built_in_model_hears_a_quick_phrase() {
    let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Some(model) = hvtt_core::paths::models_dir()
        .map(|d| d.join(hvtt_core::models::built_in().file))
        .filter(|p| p.exists())
    else {
        eprintln!("skipping: the built-in model is not downloaded - run scripts/fetch-model.sh");
        return;
    };
    let Some(samples) = speak("Push the fix to GitHub tonight.", "default") else { return };
    let engine = hvtt_desktop::engine_whisper::WhisperEngine::load(&model).expect("the model loads");
    let got = engine
        .transcribe(&hvtt_core::engine::TranscriptionRequest {
            samples,
            vocabulary_prompt: None,
            give_up: None,
            provisional: false,
        })
        .expect("recognition succeeds")
        .text
        .to_lowercase();
    for word in ["push", "fix", "tonight"] {
        assert!(got.contains(word), "expected {word:?}, got {got:?}");
    }
}

/// Live recognition: the audio is fed in as it would arrive, half a second at a time, and each full
/// window (`WINDOW_SECS`) is recognised for good up to its last whole sentence, exactly as the
/// running program does; the rest at the stop. Every sentence must come through once - a word lost
/// or said twice where one window hands over to the next would be dictation lost or garbled.
#[test]
fn speech_recognised_a_window_at_a_time_keeps_every_sentence_once() {
    let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Some(model) = model_path() else {
        eprintln!("skipping: no model");
        return;
    };
    let passage = "The weather was cold this morning. We walked the dog down to the river. \
                   Later we made pancakes for breakfast. Then everyone went back to sleep. \
                   In the afternoon the neighbours came over with a basket of apples. \
                   My brother fixed the fence behind the garage while it was still light. \
                   We talked about the trip to the mountains next summer. \
                   Nobody could agree on which trail to take, so we flipped a coin. \
                   After dinner the children played cards in the kitchen. \
                   The cat slept on the windowsill until the rain started.";
    let Some(samples) = speak(passage, "window") else {
        eprintln!("skipping: no speech synthesizer");
        return;
    };
    let window = (hvtt_core::engine::WINDOW_SECS * 16_000.0) as usize;
    assert!(samples.len() > window, "the passage must outlast one window to test the hand-over");
    let engine = hvtt_desktop::engine_whisper::WhisperEngine::load(&model).expect("the model loads");
    let hear = |audio: &[f32], before: &str| {
        let words: Vec<&str> = before.split_whitespace().collect();
        let context = words[words.len().saturating_sub(25)..].join(" ");
        engine
            .transcribe(&hvtt_core::engine::TranscriptionRequest {
                samples: audio.to_vec(),
                vocabulary_prompt: (!context.is_empty()).then_some(context),
                give_up: None,
                provisional: false,
            })
            .expect("recognition succeeds")
    };

    // Exactly the program's path: the voice detector, `window_step` while he talks, and
    // `recognise_all` for the rest at the stop.
    hvtt_desktop::load_voice_detector();
    let mut hear = |audio: &[f32], before: &str| -> Result<hvtt_core::engine::TranscriptionResult, ()> {
        Ok(hear(audio, before))
    };
    let (mut heard, mut text, mut windows) = (0usize, String::new(), 0);
    let mut upto = 8_000;
    while upto < samples.len() {
        if upto - heard >= window {
            let step = hvtt_core::engine::window_step(&samples[heard..heard + window], &text, &mut hear).unwrap();
            let hvtt_core::engine::Window::Keep { text: words, upto: cut } = step else {
                panic!("a clear passage should not be held");
            };
            text = format!("{text} {words}").trim().to_string();
            heard += cut;
            windows += 1;
        }
        upto += 8_000;
    }
    let started = std::time::Instant::now();
    let (rest, failed) = hvtt_core::engine::recognise_all(&samples[heard..], &text, &mut hear);
    assert!(failed.is_none(), "recognition succeeds");
    assert_eq!(rest.unrecognised_secs, 0.0, "nothing left unrecognised");
    text = format!("{text} {}", rest.text).trim().to_string();
    eprintln!(
        "{:.1}s of speech in {windows} window(s) + the last {:.1}s ({} ms at the stop): {text:?}",
        samples.len() as f32 / 16_000.0,
        (samples.len() - heard) as f32 / 16_000.0,
        started.elapsed().as_millis(),
    );

    assert!(windows >= 1, "a passage this long is recognised before the stop, not all at it");
    // Windows' voice has it written as two words; it is the same sentence, heard once.
    let got = text.to_lowercase().replace("window sill", "windowsill");
    for word in ["weather", "river", "pancakes", "apples", "fence", "mountains", "coin", "cards", "windowsill"] {
        assert_eq!(got.matches(word).count(), 1, "{word:?} once, got {got:?}");
    }
}

/// Codex's sixth and seventh reviews: quiet speech - a microphone set low - with a fan behind it
/// was judged silence and never reached Whisper. With the voice detector it is heard.
#[test]
fn quiet_speech_under_a_fan_is_still_heard() {
    let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Some(model) = model_path() else { return };
    let Some(loud) = speak("The quick brown fox jumps over the lazy dog.", "quiet") else { return };
    if !hvtt_desktop::load_voice_detector_loaded() {
        eprintln!("skipping: no voice detector file");
        return;
    }
    let level = (loud.iter().map(|s| s * s).sum::<f32>() / loud.len() as f32).sqrt() * 0.01;
    let quiet: Vec<f32> = loud
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let t = i as f32 / 16_000.0;
            s * 0.01 + level * 0.5 * (0.6 * (std::f32::consts::TAU * 120.0 * t).sin() + 0.3 * (std::f32::consts::TAU * 240.0 * t).sin())
        })
        .collect();
    assert_ne!(hvtt_core::audio::hear_speech(&quiet), hvtt_core::audio::Heard::Silence);
    let engine = hvtt_desktop::engine_whisper::WhisperEngine::load(&model).expect("the model loads");
    let got = engine
        .transcribe(&hvtt_core::engine::TranscriptionRequest {
            samples: quiet,
            vocabulary_prompt: None,
            give_up: None,
            provisional: false,
        })
        .expect("recognition succeeds")
        .text
        .to_lowercase();
    assert!(got.contains("fox"), "heard {got:?}");
}

/// Pausing or sending abandons a live pass part-way, so the final pass never waits behind it.
#[test]
fn a_live_pass_gives_up_when_asked() {
    let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Some(model) = model_path() else { return };
    let Some(one) = speak(
        "The weather was cold this morning. We walked the dog down to the river. \
         Later we made pancakes for breakfast. Then everyone went back to sleep.",
        "give-up",
    ) else {
        return;
    };
    // Four times over: long enough that finishing it would take clearly longer than giving up.
    let samples: Vec<f32> = one.iter().chain(&one).chain(&one).chain(&one).copied().collect();
    let engine = hvtt_desktop::engine_whisper::WhisperEngine::load(&model).expect("the model loads");
    let counter = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let request = |give_up| hvtt_core::engine::TranscriptionRequest {
        samples: samples.clone(),
        vocabulary_prompt: None,
        provisional: false,
        give_up,
    };

    let started = std::time::Instant::now();
    let _ = engine.transcribe(&request(None));
    let whole = started.elapsed();

    // Left alone, a pass that could give up must finish with the words. whisper-rs's own "safe"
    // callback gave up every time, and this test missed it by only ever asking it to give up.
    let kept = engine
        .transcribe(&request(Some(hvtt_core::engine::GiveUp { counter: counter.clone(), value: 0 })))
        .expect("a pass nobody abandoned finishes");
    assert!(kept.text.to_lowercase().contains("pancakes"), "got {:?}", kept.text);

    let bump = counter.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(100));
        bump.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    });
    let started = std::time::Instant::now();
    let abandoned = engine.transcribe(&request(Some(hvtt_core::engine::GiveUp {
        counter: counter.clone(),
        value: 0,
    })));
    let given_up = started.elapsed();
    eprintln!("whole pass {whole:?}; abandoned after {given_up:?} ({:?})", abandoned.as_ref().map(|r| r.text.len()));
    assert!(given_up < whole / 2, "gave up in {given_up:?}, a whole pass takes {whole:?}");
}
