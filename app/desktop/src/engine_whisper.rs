//! Whisper recognition, via whisper.cpp.
//!
//! Loaded once and kept resident — pay RAM rather than seconds so the hotkey responds instantly.
//! Behind `hvtt_core::engine::Transcriber`, so
//! milestone 2 can swap the engine after measuring rather than rewriting.

use hvtt_core::engine::{EngineError, TranscriptionRequest, TranscriptionResult, Transcriber};
use parking_lot::Mutex;
use std::path::{Path, PathBuf};
use std::time::Instant;
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

pub struct WhisperEngine {
    ctx: Mutex<WhisperContext>,
    label: String,
    threads: i32,
}

impl WhisperEngine {
    pub fn load(model_path: &Path) -> Result<Self, EngineError> {
        Self::load_with(model_path, true, None)
    }

    /// `load`, with the graphics processor left out (`gpu` false) or a set number of threads - for
    /// measuring what another computer would do (`examples/accuracy.rs`, `HVTT_BENCH_CPU`).
    pub fn load_with(model_path: &Path, gpu: bool, threads: Option<i32>) -> Result<Self, EngineError> {
        Self::load_on(model_path, gpu.then_some(0), threads)
    }

    /// `load`, on one graphics card (`card`, counted as `graphics_cards` lists them) or on the
    /// processor alone (`None`).
    pub fn load_on(model_path: &Path, card: Option<i32>, threads: Option<i32>) -> Result<Self, EngineError> {
        // Direct callers (including tests) get the same protection as load_fastest. No helper
        // executable, or a driver that fails in it: use the processor without touching that card.
        #[cfg(windows)]
        let card = card.filter(|&i| {
            graphics_cards().get(i as usize).is_some_and(|name| {
                probe_executable().and_then(|exe| probe_card(&exe, model_path, i, name)).is_some()
            })
        });
        Self::load_on_probed(model_path, card, threads)
    }

    // On Windows, called with a card only by the probe itself or after that probe succeeded.
    fn load_on_probed(model_path: &Path, card: Option<i32>, threads: Option<i32>) -> Result<Self, EngineError> {
        // The public Windows build targets AVX2-class processors (Intel 2013 on, AMD 2015 on)
        // rather than the building machine's own. On anything older, whisper.cpp would stop the
        // program with an illegal instruction; say so plainly instead.
        #[cfg(all(windows, target_arch = "x86_64"))]
        {
            use std::arch::is_x86_feature_detected as has;
            if !(has!("avx2") && has!("fma") && has!("f16c") && has!("bmi2")) {
                return Err(EngineError::ModelLoad(
                    "this PC's processor is too old for the speech engine, which needs AVX2 \
                     (Intel from 2013, AMD from 2015)."
                        .into(),
                ));
            }
        }
        if !model_path.exists() {
            return Err(EngineError::ModelMissing {
                path: model_path.display().to_string(),
            });
        }

        // Without Vulkan's loader the engine cannot start at all, graphics card or not.
        #[cfg(windows)]
        if !vulkan_ready() {
            return Err(EngineError::ModelLoad(
                "a file this program needs (vulkan-1.dll) is missing. Install Huck's Voice to Text again.".into(),
            ));
        }

        let mut params = WhisperContextParameters::default();
        // Metal on Apple Silicon, Vulkan on Windows; the flag is harmless where neither is there.
        params.use_gpu(card.is_some());
        params.gpu_device(card.unwrap_or(0));

        let ctx = WhisperContext::new_with_params(model_path, params)
            .map_err(|e| EngineError::ModelLoad(e.to_string()))?;

        let label = model_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "whisper".into());

        // Leave a core for the UI so the composer never stutters mid-recognition.
        let threads = threads.unwrap_or_else(|| {
            (std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4)
                .saturating_sub(1))
            .max(1) as i32
        });

        Ok(WhisperEngine { ctx: Mutex::new(ctx), label, threads })
    }

    /// Windows: the model loaded where this computer runs it quickest - one of its graphics cards,
    /// or the processor - and the name of that place, to remember (`known`: the place found
    /// before, for this model and these cards).
    ///
    /// His idea, 2026-10-05: "when it does a test it'll just figure out" which is better. Nobody
    /// is asked "processor or graphics card?"; a card built into the processor can be slower than
    /// the processor itself, so each is timed rather than assumed. Each card gets a first pass
    /// that readies it and a second that is timed. A card that is quick enough (`CARD_IS_FINE`)
    /// is taken there and then: timing the processor against it could not change anything he
    /// would notice, and can take most of a minute with a large model - whisper.cpp looks for
    /// "give up" only between its steps, never inside the long one (Codex's review, 2026-10-05).
    /// Only a slow card is raced against the processor, which is then worth its time. All card
    /// loads and measurements happen in a child first: a driver may throw or exit the process.
    /// A remembered card is probed again (drivers can change); a remembered processor is not.
    #[cfg(windows)]
    pub fn load_fastest(model_path: &Path, known: Option<&str>) -> Result<(Self, String), EngineError> {
        let processor = || {
            Ok((Self::load_on_probed(model_path, None, None)?, PROCESSOR.to_string()))
        };
        if known == Some(PROCESSOR) {
            return processor();
        }
        let cards = graphics_cards();
        let Some(exe) = probe_executable().filter(|_| !cards.is_empty()) else {
            return processor();
        };
        if let Some((i, name)) = known.and_then(|name| cards.iter().enumerate().find(|(_, c)| c.as_str() == name)) {
            // Failure goes straight to the processor and is remembered by load_engine; do not
            // re-probe this failing card on every start under the same cache key.
            if probe_card(&exe, model_path, i as i32, name).is_some() {
                if let Ok(engine) = Self::load_on_probed(model_path, Some(i as i32), None) {
                    return Ok((engine, name.clone()));
                }
            }
            return processor();
        }
        let mut best: Option<(u128, i32, String)> = None;
        for (i, name) in cards.iter().enumerate() {
            let started = Instant::now();
            let Some(ms) = probe_card(&exe, model_path, i as i32, name) else { continue };
            eprintln!("[hvtt] {name}: {ms} ms a pass, ready after {} ms", started.elapsed().as_millis());
            if best.as_ref().map_or(true, |b| ms < b.0) {
                best = Some((ms, i as i32, name.clone()));
            }
        }
        let Some((card_ms, card, name)) = best else { return processor() };
        if card_ms <= CARD_IS_FINE {
            return match Self::load_on_probed(model_path, Some(card), None) {
                Ok(engine) => Ok((engine, name)),
                Err(_) => processor(),
            };
        }
        if let Ok(engine) = Self::load_on_probed(model_path, None, None) {
            if let Some(ms) = engine.timed_pass(false, Some(card_ms)).filter(|&ms| ms < card_ms) {
                eprintln!("[hvtt] the processor is quicker: {ms} ms a pass");
                return Ok((engine, PROCESSOR.to_string()));
            }
            // Drop the CPU candidate before loading the winning card in this process.
        }
        match Self::load_on_probed(model_path, Some(card), None) {
            Ok(engine) => Ok((engine, name)),
            Err(_) => processor(),
        }
    }

    /// One recognition of a second of faint made-up sound, timed - after one that is not, when
    /// `warm` (a graphics card's first pass readies it). Given up on past `limit_ms`.
    #[cfg(windows)]
    fn timed_pass(&self, warm: bool, limit_ms: Option<u128>) -> Option<u128> {
        use std::sync::atomic::{AtomicU64, Ordering};
        let samples: Vec<f32> =
            (0..48_000u32).map(|i| 0.002 * (((i.wrapping_mul(2_654_435_761)) >> 16) as f32 / 65_536.0 - 0.5)).collect();
        let counter = std::sync::Arc::new(AtomicU64::new(0));
        let give_up = Some(hvtt_core::engine::GiveUp { counter: counter.clone(), value: 0 });
        let request = TranscriptionRequest { samples, vocabulary_prompt: None, give_up, provisional: false };
        if warm {
            self.transcribe(&request).ok()?;
        }
        if let Some(ms) = limit_ms {
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(ms as u64));
                counter.store(1, Ordering::SeqCst);
            });
        }
        self.transcribe(&request).ok().map(|r| r.elapsed_ms)
    }

    /// The model this engine would load, from settings.
    ///
    /// A model he downloaded himself, in the app-data folder, wins. Otherwise the one shipped
    /// inside the app (`Contents/Resources/models/`), which is what makes a downloaded release
    /// work straight away. If neither exists the app-data path is returned, so the "model
    /// missing" message names the place a model belongs.
    pub fn expected_path(model_file: &str) -> Option<PathBuf> {
        let own = hvtt_core::paths::models_dir().map(|d| d.join(model_file));
        if own.as_deref().is_some_and(Path::exists) {
            return own;
        }
        // Windows installs keep it in `models\` beside the program.
        let bundled = std::env::current_exe().ok().and_then(|exe| {
            let dir = exe.parent()?;
            Some(if cfg!(windows) { dir.join("models") } else { dir.parent()?.join("Resources").join("models") }
                .join(model_file))
        });
        match bundled {
            Some(b) if b.exists() => Some(b),
            _ => own,
        }
    }
}

/// What `load_fastest` calls the processor, beside the graphics cards' own names.
pub const PROCESSOR: &str = "processor";

/// A graphics card this quick for one pass, in ms, is used without timing the processor against
/// it - the speed check's own "quick enough" (`hvtt_core::models::speed_advice`).
#[cfg(windows)]
const CARD_IS_FINE: u128 = 1_500;

#[cfg(windows)]
const CARD_PROBE_ARGUMENT: &str = "--probe-graphics-card";

/// Dispatch only the hidden Windows probe. No app state, settings, mutex or window is created.
#[cfg(windows)]
pub fn run_card_probe_if_requested() -> Option<i32> {
    use std::io::Write;
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() != Some(std::ffi::OsStr::new(CARD_PROBE_ARGUMENT)) {
        return None;
    }
    // A crashing driver must not open a Windows error dialog either.
    #[link(name = "kernel32")]
    extern "system" { fn SetErrorMode(mode: u32) -> u32; }
    unsafe { SetErrorMode(0x0001 | 0x0002 | 0x8000); }
    let probe = || -> Option<u128> {
        let model = PathBuf::from(args.next()?);
        let card: i32 = args.next()?.to_str()?.parse().ok()?;
        let expected = args.next();
        if card < 0 || !model.is_file() || args.next().is_some() { return None; }
        let cards = graphics_cards();
        let name = cards.get(card as usize)?;
        if expected.as_ref().is_some_and(|e| e != std::ffi::OsStr::new(name)) {
            return None;
        }
        let engine = WhisperEngine::load_on_probed(&model, Some(card), None).ok()?;
        engine.timed_pass(true, None)
    };
    let mut probe = probe;
    Some(match probe() {
        Some(ms) if writeln!(std::io::stdout().lock(), "{ms}").is_ok() => 0,
        _ => 1,
    })
}

/// Cargo's test/example executables live in deps/ or examples/ beside the desktop executable.
/// Never invoke a test harness as a probe, or fall back to an installed (possibly older) app.
#[cfg(windows)]
fn probe_executable() -> Option<PathBuf> {
    let current = std::env::current_exe().ok()?;
    let name = current.file_name()?.to_str()?;
    if name.eq_ignore_ascii_case("hvtt-desktop.exe") || name.eq_ignore_ascii_case("HucksVoiceToText.exe") {
        return Some(current);
    }
    let dir = current.parent()?;
    let profile = match dir.file_name()?.to_str()? {
        "deps" | "examples" => dir.parent()?,
        _ => return None,
    };
    let exe = profile.join("hvtt-desktop.exe");
    // An example-only build may leave an older desktop binary here. Do not launch one whose
    // entry point predates probe mode (it would start the ordinary app instead).
    let bytes = std::fs::read(&exe).ok()?;
    bytes.windows(CARD_PROBE_ARGUMENT.len())
        .any(|part| part == CARD_PROBE_ARGUMENT.as_bytes()).then_some(exe)
}

#[cfg(windows)]
#[derive(Clone, Copy)]
enum ProbeExit { Exited(bool), TimedOut, Failed }

/// Only a clean exit and one decimal number are evidence that the card completed both passes.
#[cfg(windows)]
fn probe_milliseconds(exit: ProbeExit, output: &[u8]) -> Option<u128> {
    if !matches!(exit, ProbeExit::Exited(true)) { return None; }
    let number = std::str::from_utf8(output).ok()?.trim();
    if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) { return None; }
    number.parse().ok()
}

/// A generous two minutes includes model loading and first-use shader compilation. Drain stdout
/// concurrently (bounded), so even unexpected driver output cannot block the timeout. Reap the
/// child on failure. Cleanup is bounded too: a broken driver must not turn wait() after kill()
/// into another indefinite wait. Windows releases our process handle when Child is dropped.
#[cfg(windows)]
#[doc(hidden)]
pub fn probe_card(exe: &Path, model: &Path, card: i32, name: &str) -> Option<u128> {
    use std::io::Read;
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::time::Duration;
    let mut child = Command::new(exe)
        .arg(CARD_PROBE_ARGUMENT).arg(model).arg(card.to_string()).arg(name)
        .creation_flags(0x08000000) // CREATE_NO_WINDOW
        .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null())
        .spawn().ok()?;
    let stdout = child.stdout.take().expect("piped stdout");
    let (send, receive) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stdout.take(128).read_to_end(&mut bytes).map(|_| bytes);
        let _ = send.send(result);
    });
    let deadline = Instant::now() + Duration::from_secs(120);
    let exit = loop {
        match child.try_wait() {
            Ok(Some(status)) => break ProbeExit::Exited(status.success()),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            result => {
                let _ = child.kill();
                let cleanup_until = Instant::now() + Duration::from_secs(1);
                while matches!(child.try_wait(), Ok(None)) && Instant::now() < cleanup_until {
                    std::thread::sleep(Duration::from_millis(20));
                }
                break if result.is_err() { ProbeExit::Failed } else { ProbeExit::TimedOut };
            }
        }
    };
    let bytes = receive.recv_timeout(Duration::from_secs(1)).ok()?.ok()?;
    probe_milliseconds(exit, &bytes)
}

/// Windows: is Vulkan's loader in the program? It must be before whisper.cpp is first touched:
/// the engine looks for graphics cards whenever it starts, and with no loader that look would end
/// the program. The graphics driver's own copy comes first - it is the one that matches the
/// driver; on a PC with none, the copy the installer puts beside the program is used, finds no
/// card, and recognition runs on the processor.
#[cfg(windows)]
pub fn vulkan_ready() -> bool {
    use windows::core::w;
    use windows::Win32::System::LibraryLoader::{LoadLibraryExW, LoadLibraryW, LOAD_LIBRARY_SEARCH_SYSTEM32};
    static READY: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *READY.get_or_init(|| unsafe {
        LoadLibraryExW(w!("vulkan-1.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32).is_ok()
            || LoadLibraryW(w!("vulkan-1.dll")).is_ok()
    })
}

/// The graphics cards whisper.cpp can run on here, by the names their drivers give, in the order
/// `load_on` counts them. Two cards of the same name are told apart - "… (2)" - so the one
/// remembered is the one found again, not the first of that name (Codex's review, 2026-10-05).
/// Empty off Windows, where `load` needs no choosing.
pub fn graphics_cards() -> Vec<String> {
    #[cfg(windows)]
    {
        use whisper_rs_sys as sys;
        if !vulkan_ready() {
            return Vec::new();
        }
        let mut cards = Vec::new();
        unsafe {
            for i in 0..sys::ggml_backend_dev_count() {
                let device = sys::ggml_backend_dev_get(i);
                let kind = sys::ggml_backend_dev_type(device);
                if kind == sys::ggml_backend_dev_type_GGML_BACKEND_DEVICE_TYPE_GPU
                    || kind == sys::ggml_backend_dev_type_GGML_BACKEND_DEVICE_TYPE_IGPU
                {
                    let name = sys::ggml_backend_dev_description(device);
                    let name = if name.is_null() {
                        "graphics card".to_string()
                    } else {
                        std::ffi::CStr::from_ptr(name).to_string_lossy().into_owned()
                    };
                    cards.push(numbered(&cards, name));
                }
            }
        }
        cards
    }
    #[cfg(not(windows))]
    Vec::new()
}

/// `name`, or "`name` (2)", "`name` (3)"… when `cards` already has one called that.
#[cfg_attr(not(windows), allow(dead_code))]
fn numbered(cards: &[String], name: String) -> String {
    (1..)
        .map(|n| if n == 1 { name.clone() } else { format!("{name} ({n})") })
        .find(|candidate| !cards.contains(candidate))
        .expect("some number is free")
}

/// Whisper asks this between steps; `true` abandons the recognition.
unsafe extern "C" fn should_give_up(data: *mut std::ffi::c_void) -> bool {
    (*(data as *const hvtt_core::engine::GiveUp)).now()
}

/// Silence put before the speech. Whisper learned from recordings that seldom begin on the first
/// syllable; a quick dictation - pressed, and spoke at once - does, and loses words for it.
/// Measured 2026-09-30 (`examples/accuracy.rs`, 164 fast phrases): base.en 7.8% of words wrong
/// without it, 6.2% with it, for no measurable time.
const LEAD_IN_MS: usize = 300;

/// The window, in seconds, a pass may be given instead of Whisper's whole 30 - `None` for all of it.
///
/// Whisper hears a fixed 30-second window, however short the audio. From 2026-09-28 every pass
/// used one sized to its audio, which made passes quick on a processor alone - and cost most of
/// the accuracy. Measured 2026-09-30 (`examples/accuracy.rs`), share of words wrong:
///
/// | | sized to the audio | at least 10 s | all 30 s |
/// |---|---|---|---|
/// | base.en, quick phrases | 18% | 6.4% | 6.2% |
/// | base.en, 4-12 s stretches | 5.4% | 3.4% | 2.6% |
/// | small.en, quick phrases | 6.8% | 2.7% | 3.2% |
/// | large-v3-turbo, quick phrases | 41% | 2.4% | 1.9% |
/// | large-v3-turbo, 4-12 s stretches | 3.6% | 10.6% | 0.8% |
///
/// So words that are sent always get all of it; on the Mac's graphics processor that takes 0.08 s
/// for base.en, 0.22 s for small.en and 0.9 s for large-v3-turbo. Only the processor-only Windows
/// build gives the provisional words - shown fainter while he talks, replaced moments later - a
/// smaller window, never under 10 s (at 5, large-v3-turbo got 37% wrong).
fn window_secs(audio_secs: f32, provisional: bool, model: &str) -> Option<f32> {
    let window = audio_secs.max(10.0);
    // large-v3-turbo breaks with any shortened window (10.6% wrong on 4-12 s at 10 s, against 0.8%):
    // always the whole one, previews included (Codex's twelfth review).
    let shortened_ok = !model.contains("large");
    (provisional && shortened_ok && !cfg!(target_os = "macos") && window < 29.0).then_some(window)
}

impl Transcriber for WhisperEngine {
    fn name(&self) -> &str {
        &self.label
    }

    fn transcribe(&self, req: &TranscriptionRequest) -> Result<TranscriptionResult, EngineError> {
        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        params.set_n_threads(self.threads);
        // English-first is a product decision, not a limitation to work around later.
        params.set_language(Some("en"));
        params.set_translate(false);
        // Nothing should be printed to stdout by a GUI app.
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);
        params.set_suppress_blank(true);
        let rate = hvtt_core::audio::WHISPER_SAMPLE_RATE as usize;
        let samples: Vec<f32> = std::iter::repeat(0.0)
            .take(rate * LEAD_IN_MS / 1000)
            .chain(req.samples.iter().copied())
            .collect();
        let secs = req.samples.len() as f32 / rate as f32;
        // 50 frames a second, with a margin.
        if let Some(window) = window_secs(samples.len() as f32 / rate as f32, req.provisional, &self.label) {
            params.set_audio_ctx(((window * 50.0).ceil() as i32 + 64).min(1500));
        }
        // Unsure of a scrap of audio, Whisper can loop on a word, and each time it rejects its
        // own guess it starts over, up to five times - one live pass then took seconds and showed
        // words nobody said ("Rob Syke", 2026-09-28). Live passes get one attempt, no longer than
        // anyone speaks (about 3 tokens a second; 6 allowed, plus a margin).
        if req.provisional {
            params.set_temperature_inc(0.0);
            params.set_max_tokens((secs * 6.0).ceil() as i32 + 8);
        }

        if let Some(prompt) = req.vocabulary_prompt.as_deref() {
            params.set_initial_prompt(prompt);
        }
        // Not `set_abort_callback_safe`: in whisper-rs 0.16 its trampoline reads the boxed closure
        // as the closure itself, so it answered "give up" at the first check, every time - and no
        // word ever filled in while he talked (found 2026-09-28). The plain C callback, with the
        // request's own `GiveUp` as its data, which outlives the recognition below.
        if let Some(give_up) = req.give_up.as_ref() {
            unsafe {
                params.set_abort_callback(Some(should_give_up));
                params.set_abort_callback_user_data(
                    give_up as *const hvtt_core::engine::GiveUp as *mut std::ffi::c_void,
                );
            }
        }

        // One recognition at a time: two at once each take every core and slow each other
        // down many times over (measured 2026-09-28). A pass made while he talks is abandoned
        // instead, the moment he pauses or sends (`GiveUp`).
        let ctx = self.ctx.lock();
        // Timed from here: waiting behind another recognition is not this one's time (the speed
        // check reads it - Codex's seventeenth review).
        let started = Instant::now();
        let mut state = ctx
            .create_state()
            .map_err(|e| EngineError::Recognition(e.to_string()))?;

        state
            .full(params, &samples)
            .map_err(|e| EngineError::Recognition(e.to_string()))?;

        let mut raw = String::new();
        let mut sentences = Vec::new();
        // Whisper's times are in hundredths of a second, from the start of the lead-in.
        let lead = LEAD_IN_MS as f32 / 1000.0;
        let at = |cs: i64| (cs as f32 / 100.0 - lead).max(0.0);
        for i in 0..state.full_n_segments() {
            let Some(segment) = state.get_segment(i) else { continue };
            // Lossy on purpose: a single malformed byte must not cost the user the whole
            // dictation.
            if let Ok(text) = segment.to_str_lossy() {
                raw.push_str(&text);
                raw.push('\n');
                let text = hvtt_core::engine::tidy(&text);
                if !text.is_empty() {
                    sentences.push(hvtt_core::engine::Sentence {
                        start: at(segment.start_timestamp()),
                        end: at(segment.end_timestamp()),
                        text,
                        no_speech: segment.no_speech_probability(),
                    });
                }
            }
        }

        Ok(TranscriptionResult {
            // Hesitation dots stay until the caller knows whether the words end here (Codex's
            // seventh review: turned into a full stop here, they could not be undone at a
            // hand-over).
            text: hvtt_core::engine::tidy(&raw),
            sentences,
            elapsed_ms: started.elapsed().as_millis(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn only_a_clean_probe_with_a_number_is_usable() {
        assert_eq!(probe_milliseconds(ProbeExit::Exited(true), b"530\r\n"), Some(530));
        assert_eq!(probe_milliseconds(ProbeExit::Exited(true), b"0\n"), Some(0));
        for output in [b"".as_slice(), b"driver error", b"12\n34", b"-1", b"NaN", b"\xff"] {
            assert_eq!(probe_milliseconds(ProbeExit::Exited(true), output), None);
        }
        assert_eq!(probe_milliseconds(ProbeExit::Exited(true), &[b'9'; 128]), None);
    }

    #[cfg(windows)]
    #[test]
    fn a_failed_or_timed_out_probe_is_unusable_even_if_it_printed_a_time() {
        for exit in [ProbeExit::Exited(false), ProbeExit::TimedOut, ProbeExit::Failed] {
            assert_eq!(probe_milliseconds(exit, b"530\n"), None);
        }
    }

    #[cfg(windows)]
    #[test]
    fn tests_find_the_desktop_binary_instead_of_probing_the_test_harness() {
        let exe = probe_executable().expect("cargo test builds the desktop binary for integration tests");
        assert_eq!(exe.file_name().unwrap(), "hvtt-desktop.exe");
        assert_ne!(exe, std::env::current_exe().unwrap());
    }

    #[test]
    fn two_graphics_cards_of_the_same_name_are_told_apart() {
        let mut cards: Vec<String> = Vec::new();
        for name in ["Arc A750", "Radeon", "Arc A750", "Arc A750"] {
            let name = numbered(&cards, name.to_string());
            cards.push(name);
        }
        assert_eq!(cards, ["Arc A750", "Radeon", "Arc A750 (2)", "Arc A750 (3)"]);
    }
}
