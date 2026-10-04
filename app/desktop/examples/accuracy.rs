//! How well quick dictations are recognised.
//!
//! Runs a folder of short spoken phrases through Whisper under the settings named on the command
//! line, and prints the word error rate and how long each took. No microphone: the phrases are
//! made by `scripts/make-accuracy-corpus.py` (system voices at fast speaking rates, trimmed tight -
//! pressed, spoke at once, pressed again), 48 kHz like a real microphone.
//!
//!   scripts/dev.sh bench <corpus-dir> <model.bin> <case> [<case> ...]
//!
//! A case is `sound:resampler:decoder`:
//!   sound      clean | hiss25 | hiss15   white noise at that SNR, like a microphone's own hiss
//!              lateN | earlyN            the first or last N ms missing
//!              volN                      at N tenths of a percent of its volume (vol10 = 1%)
//!   resampler  linear                    every third sample, as the app did until 0.1.7
//!              sinc                      `hvtt_core::audio::condition`, filtered first
//!   decoder    fit                       greedy, window sized to the audio (the app until 0.1.7)
//!              full | beam | beamfit     greedy / beam search of 5, whole window / sized window
//!              pad | beampad             300 ms of silence either side, whole window
//!              floorN                    `pad`, window sized to the audio but at least N seconds
//!              scaleN                    `pad`, window N times the audio, at least 10 seconds
//!              leadN                     N ms of silence before the speech only, whole window
//!              engine                    exactly what the app's engine does for words it sends
//!              rollingN[s]               (s: also kept for good after a second of quiet)
//!                                        while he talks, each 28 s waiting is recognised for good
//!                                        up to its last whole sentence, Whisper's own way; at the
//!                                        stop, the rest. Its time is the stop's alone
//!              stretches                 the engine as the app used it while he talked until
//!                                        2026-10-01: cut at pauses (3-12 s), each stretch
//!                                        prompted with the last 25 words, the rest at the stop
//!
//! `SHOW=1` prints every clip that came back wrong. `HVTT_BENCH_CPU=N`: processor only, N threads.

use hvtt_core::engine::{TranscriptionRequest, Transcriber};
use std::path::{Path, PathBuf};
use std::time::Instant;
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

struct Clip {
    file: String,
    voice: String,
    reference: String,
    samples: Vec<f32>,
}

fn load_corpus(dir: &Path) -> Vec<Clip> {
    let manifest = std::fs::read_to_string(dir.join("manifest.tsv")).expect("manifest.tsv");
    manifest
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let cols: Vec<&str> = line.split('\t').collect();
            let mut reader = hound::WavReader::open(dir.join(cols[0])).expect("a wav");
            assert_eq!(reader.spec().sample_rate, 48_000, "the corpus is 48 kHz");
            let samples = reader.samples::<i16>().map(|s| s.unwrap() as f32 / 32768.0).collect();
            Clip {
                file: cols[0].into(),
                voice: cols[1].into(),
                reference: cols[3].into(),
                samples,
            }
        })
        .collect()
}

/// A tiny deterministic generator, so every run hears the same hiss.
fn noise(seed: u64, n: usize) -> Vec<f32> {
    let mut s = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..n)
        .map(|_| {
            // Sum of four uniforms: close enough to Gaussian for hiss.
            (0..4)
                .map(|_| {
                    s ^= s << 13;
                    s ^= s >> 7;
                    s ^= s << 17;
                    (s >> 11) as f32 / (1u64 << 53) as f32 - 0.5
                })
                .sum::<f32>()
        })
        .collect()
}

fn rms(x: &[f32]) -> f32 {
    (x.iter().map(|v| v * v).sum::<f32>() / x.len().max(1) as f32).sqrt()
}

fn sound(kind: &str, clip: &Clip, seed: u64) -> Vec<f32> {
    let x = &clip.samples;
    let ms = |k: &str, prefix: &str| 48 * k[prefix.len()..].parse::<usize>().expect("a number of ms");
    match kind {
        "clean" => x.clone(),
        k if k.starts_with("late") => x[ms(k, "late").min(x.len())..].to_vec(),
        k if k.starts_with("early") => x[..x.len().saturating_sub(ms(k, "early"))].to_vec(),
        // `volN`: the whole clip at N tenths of a percent of its volume - a microphone set low.
        k if k.starts_with("vol") => {
            let gain = k[3..].parse::<f32>().expect("volN") / 1000.0;
            x.iter().map(|s| s * gain).collect()
        }
        k if k.starts_with("hiss") => {
            let snr_db: f32 = k[4..].parse().expect("hissNN");
            let n = noise(seed, x.len());
            let gain = rms(x) / rms(&n) / 10f32.powf(snr_db / 20.0);
            x.iter().zip(&n).map(|(s, h)| s + h * gain).collect()
        }
        other => panic!("unknown sound {other}"),
    }
}

/// The app's conditioning until 0.1.7: linear interpolation, which at 48 kHz to 16 kHz is simply
/// every third sample.
fn linear(input: &[f32]) -> Vec<f32> {
    let ratio = 16_000f64 / 48_000f64;
    let out_len = ((input.len() as f64) * ratio).round() as usize;
    (0..out_len)
        .map(|i| {
            let src = i as f64 / ratio;
            let lo = (src.floor() as usize).min(input.len() - 1);
            let hi = (lo + 1).min(input.len() - 1);
            let frac = (src - lo as f64) as f32;
            input[lo] + (input[hi] - input[lo]) * frac
        })
        .collect()
}

fn resample(kind: &str, x: &[f32]) -> Vec<f32> {
    match kind {
        "linear" => linear(x),
        "sinc" => hvtt_core::audio::condition(x, 1, 48_000),
        other => panic!("unknown resampler {other}"),
    }
}

/// Word error rate's ingredients: lower case, punctuation gone, apostrophes kept.
fn words(s: &str) -> Vec<String> {
    s.to_lowercase()
        .replace('’', "'")
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '\'' { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .map(|w| w.trim_matches('\'').to_string())
        .filter(|w| !w.is_empty())
        .collect()
}

fn edits(a: &[String], b: &[String]) -> usize {
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, wa) in a.iter().enumerate() {
        let mut cur = vec![i + 1; b.len() + 1];
        for (j, wb) in b.iter().enumerate() {
            cur[j + 1] = (prev[j] + usize::from(wa != wb)).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        prev = cur;
    }
    prev[b.len()]
}

fn threads() -> i32 {
    if let Some(n) = std::env::var("HVTT_BENCH_CPU").ok().and_then(|n| n.parse().ok()) {
        return n;
    }
    (std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).saturating_sub(1)).max(1)
        as i32
}

fn decode(ctx: &WhisperContext, kind: &str, audio: &[f32]) -> String {
    // `floorN`: the window sized to the audio, but never under N seconds. `scaleN`: N times the
    // audio, never under 10 seconds. `leadN`: N ms of silence before the speech only.
    let (beam, fit, pad_ms, floor, lead_ms) = match kind {
        "fit" => (false, true, 0, 0.0, 0),
        "full" => (false, false, 0, 0.0, 0),
        "beam" => (true, false, 0, 0.0, 0),
        "beamfit" => (true, true, 0, 0.0, 0),
        "pad" => (false, false, 300, 0.0, 0),
        "beampad" => (true, false, 300, 0.0, 0),
        k if k.starts_with("floor") => (false, true, 300, k[5..].parse::<f32>().unwrap(), 0),
        k if k.starts_with("scale") => {
            let times = k[5..].parse::<f32>().unwrap();
            (false, true, 300, (times * audio.len() as f32 / 16_000.0).max(10.0), 0)
        }
        k if k.starts_with("lead") => (false, false, 0, 0.0, k[4..].parse::<usize>().unwrap()),
        other => panic!("unknown decoder {other}"),
    };
    let pad = vec![0.0f32; 16 * pad_ms];
    let lead = vec![0.0f32; 16 * lead_ms];
    let samples: Vec<f32> =
        lead.iter().chain(&pad).chain(audio).chain(&pad).copied().collect();
    let mut params = FullParams::new(if beam {
        SamplingStrategy::BeamSearch { beam_size: 5, patience: -1.0 }
    } else {
        SamplingStrategy::Greedy { best_of: 1 }
    });
    params.set_n_threads(threads());
    params.set_language(Some("en"));
    params.set_translate(false);
    params.set_print_special(false);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);
    params.set_suppress_blank(true);
    let secs = samples.len() as f32 / 16_000.0;
    if fit && secs.max(floor) < 29.0 {
        params.set_audio_ctx(((secs.max(floor) * 50.0).ceil() as i32 + 64).min(1500));
    }
    let mut state = ctx.create_state().expect("state");
    state.full(params, &samples).expect("recognition");
    let mut raw = String::new();
    for i in 0..state.full_n_segments() {
        if let Some(t) = state.get_segment(i).and_then(|s| s.to_str_lossy().ok()) {
            raw.push_str(&t);
            raw.push('\n');
        }
    }
    hvtt_core::engine::tidy(&raw)
}

/// Recognise the way the live passes do: every half second, a finished stretch (ended at a pause)
/// is recognised for good with the last 25 words as its prompt; what is left, at the stop.
fn stretches(engine: &hvtt_desktop::engine_whisper::WhisperEngine, audio: &[f32]) -> String {
    let hear = |part: &[f32], before: &str| -> String {
        if !hvtt_core::audio::has_speech(part) || part.len() < 4_000 {
            return String::new();
        }
        let words: Vec<&str> = before.split_whitespace().collect();
        let context = words[words.len().saturating_sub(25)..].join(" ");
        engine
            .transcribe(&TranscriptionRequest {
                samples: part.to_vec(),
                vocabulary_prompt: (!context.is_empty()).then_some(context),
                give_up: None,
                provisional: false,
            })
            .expect("recognition")
            .text
    };
    let (mut heard, mut text) = (0usize, String::new());
    let mut upto = 8_000;
    while upto < audio.len() {
        if let Some(cut) = hvtt_core::audio::commit_point(&audio[heard..upto], 3.0, 12.0) {
            let words = hear(&audio[heard..heard + cut], &text);
            text = format!("{text} {words}").trim().to_string();
            heard += cut;
        }
        upto += 8_000;
    }
    let words = hear(&audio[heard..], &text);
    format!("{text} {words}").trim().to_string()
}

/// Sentences of one recognition: (start, end) in seconds from the start of `audio`, the words,
/// and Whisper's own no-speech judgement.
fn sentences(ctx: &WhisperContext, audio: &[f32], prompt: &str) -> Vec<(f32, f32, String, f32)> {
    let lead = vec![0.0f32; 16 * 300];
    let samples: Vec<f32> = lead.iter().chain(audio).copied().collect();
    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    params.set_n_threads(threads());
    params.set_language(Some("en"));
    params.set_print_special(false);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);
    params.set_suppress_blank(true);
    if !prompt.is_empty() {
        params.set_initial_prompt(prompt);
    }
    let mut state = ctx.create_state().expect("state");
    state.full(params, &samples).expect("recognition");
    (0..state.full_n_segments())
        .filter_map(|i| {
            let seg = state.get_segment(i)?;
            let text = hvtt_core::engine::tidy(&seg.to_str_lossy().ok()?);
            let at = |cs: i64| (cs as f32 / 100.0 - 0.3).max(0.0);
            Some((at(seg.start_timestamp()), at(seg.end_timestamp()), text, seg.no_speech_probability()))
        })
        .collect()
}

/// While he talks: whenever a full window (`WINDOW_SECS`) is waiting past `seek`, it is recognised
/// for good by the program's own rolling rule (`hvtt_core::engine::window_step`); at the stop, the
/// rest by `recognise_all`, as the program does. `settle_on_quiet`: also keep everything after a
/// second of quiet (measured and rejected 2026-10-01). Returns the words and the stop's time.
fn rolling(ctx: &WhisperContext, audio: &[f32], context_words: usize, settle_on_quiet: bool) -> (String, u128) {
    let window = (hvtt_core::engine::WINDOW_SECS * 16_000.0) as usize;
    let prompt = |text: &str| {
        let words: Vec<&str> = text.split_whitespace().collect();
        words[words.len().saturating_sub(context_words)..].join(" ")
    };
    let mut hear = |part: &[f32], before: &str| -> Result<hvtt_core::engine::TranscriptionResult, ()> {
        let found: Vec<hvtt_core::engine::Sentence> = sentences(ctx, part, &prompt(before))
            .into_iter()
            .map(|(start, end, text, no_speech)| hvtt_core::engine::Sentence { start, end, text, no_speech })
            .collect();
        // Raw, as the program's engine gives it: dots are dealt with by whoever knows the end.
        let joined = found.iter().map(|s| s.text.as_str()).collect::<Vec<_>>().join("\n");
        let text = hvtt_core::engine::tidy(&joined);
        Ok(hvtt_core::engine::TranscriptionResult { text, sentences: found, elapsed_ms: 0 })
    };
    let (mut seek, mut text, mut held) = (0usize, String::new(), false);
    let mut upto = 8_000;
    while upto <= audio.len() {
        if upto - seek >= window && !held {
            match hvtt_core::engine::window_step(&audio[seek..seek + window], &text, &mut hear).unwrap() {
                hvtt_core::engine::Window::Keep { text: words, upto: cut } if cut > 0 => {
                    text = format!("{text} {words}").trim().to_string();
                    seek += cut;
                }
                _ => {
                    held = true;
                    if std::env::var_os("SHOW").is_some() {
                        println!("    held for the stop");
                    }
                }
            }
        } else if settle_on_quiet
            && hvtt_core::audio::has_speech(&audio[seek..upto])
            && hvtt_core::audio::voice_has_stopped(&audio[seek..upto], 1.0, false)
        {
            let words = hvtt_core::engine::without_ellipses(&hear(&audio[seek..upto], &text).unwrap().text);
            text = format!("{text} {words}").trim().to_string();
            seek = upto;
        }
        upto += 8_000;
    }
    let started = Instant::now();
    if audio.len() > seek {
        let words = hvtt_core::engine::recognise_all(&audio[seek..], &text, &mut hear).0.text;
        text = format!("{text} {words}").trim().to_string();
    }
    (text, started.elapsed().as_millis())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 3 {
        eprintln!("usage: accuracy <corpus-dir> <model.bin> <sound:resampler:decoder> ...");
        std::process::exit(2);
    }
    let corpus = load_corpus(Path::new(&args[0]));
    let model = PathBuf::from(&args[1]);
    let show = std::env::var_os("SHOW").is_some();

    // As the program does: the voice detector, when its file is there.
    hvtt_desktop::load_voice_detector();
    // `HVTT_BENCH_CPU=N`: no graphics processor, N threads - a stand-in for a computer without one
    // (the Windows build recognises on the processor alone).
    let cpu: Option<i32> = std::env::var("HVTT_BENCH_CPU").ok().and_then(|n| n.parse().ok());
    let mut cp = WhisperContextParameters::default();
    cp.use_gpu(cpu.is_none());
    let ctx = WhisperContext::new_with_params(&model, cp).expect("model loads");
    // The program's own engine under the same conditions (`engine`, `stretches`).
    let engine = hvtt_desktop::engine_whisper::WhisperEngine::load_with(&model, cpu.is_none(), cpu)
        .expect("engine loads");
    let name = model.file_name().unwrap().to_string_lossy().replace("ggml-", "").replace(".bin", "");

    // Warm up, so the first clip's time is not the GPU's start-up.
    decode(&ctx, "full", &vec![0.0; 16_000]);

    for case in &args[2..] {
        let parts: Vec<&str> = case.split(':').collect();
        assert_eq!(parts.len(), 3, "a case is sound:resampler:decoder, got {case}");
        let (mut errs, mut total, mut wrong) = (0usize, 0usize, 0usize);
        let mut times = Vec::new();
        for (i, clip) in corpus.iter().enumerate() {
            let audio = resample(parts[1], &sound(parts[0], clip, i as u64 + 1));
            let started = Instant::now();
            let mut stop_ms = None;
            let got = if let Some(words) = parts[2].strip_prefix("rolling") {
                let settle = words.ends_with('s');
                let words = words.trim_end_matches('s');
                let (text, ms) = rolling(&ctx, &audio, words.parse().unwrap_or(25), settle);
                stop_ms = Some(ms);
                text
            } else if parts[2] == "stretches" {
                stretches(&engine, &audio)
            } else if parts[2] == "engine" {
                engine
                    .transcribe(&TranscriptionRequest {
                        samples: audio,
                        vocabulary_prompt: None,
                        give_up: None,
                        provisional: false,
                    })
                    .expect("recognition")
                    .text
            } else {
                decode(&ctx, parts[2], &audio)
            };
            times.push(stop_ms.unwrap_or_else(|| started.elapsed().as_millis()));
            let (r, h) = (words(&clip.reference), words(&got));
            let e = edits(&r, &h);
            errs += e;
            total += r.len();
            if e > 0 {
                wrong += 1;
                if show {
                    println!("    {} {:<22} {:?} -> {:?}", clip.file, clip.voice, clip.reference, got);
                }
            }
        }
        times.sort();
        let mean = times.iter().sum::<u128>() / times.len() as u128;
        let p90 = times[times.len() * 9 / 10];
        println!(
            "{name:<22} {case:<26} WER {:5.1}%  wrong {:>3}/{}  mean {:>4} ms  p90 {:>4} ms",
            100.0 * errs as f64 / total as f64,
            wrong,
            corpus.len(),
            mean,
            p90
        );
    }
}
