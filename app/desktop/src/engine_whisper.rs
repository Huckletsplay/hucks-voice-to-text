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

        let mut params = WhisperContextParameters::default();
        // Metal on Apple Silicon; the flag is harmless where it is unavailable.
        params.use_gpu(true);

        let ctx = WhisperContext::new_with_params(model_path, params)
            .map_err(|e| EngineError::ModelLoad(e.to_string()))?;

        let label = model_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "whisper".into());

        // Leave a core for the UI so the composer never stutters mid-recognition.
        let threads = (std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .saturating_sub(1))
        .max(1) as i32;

        Ok(WhisperEngine { ctx: Mutex::new(ctx), label, threads })
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

/// Whisper asks this between steps; `true` abandons the recognition.
unsafe extern "C" fn should_give_up(data: *mut std::ffi::c_void) -> bool {
    (*(data as *const hvtt_core::engine::GiveUp)).now()
}

impl Transcriber for WhisperEngine {
    fn name(&self) -> &str {
        &self.label
    }

    fn transcribe(&self, req: &TranscriptionRequest) -> Result<TranscriptionResult, EngineError> {
        let started = Instant::now();

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
        // Whisper works on a 30-second window whatever the length of the audio, and most of the
        // time goes on that window. Sized to the audio instead (50 frames a second, with a margin),
        // a few seconds of speech costs a fraction as much.
        let secs = req.samples.len() as f32 / hvtt_core::audio::WHISPER_SAMPLE_RATE as f32;
        if secs < 29.0 {
            params.set_audio_ctx(((secs * 50.0).ceil() as i32 + 64).min(1500));
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
        let mut state = ctx
            .create_state()
            .map_err(|e| EngineError::Recognition(e.to_string()))?;

        state
            .full(params, &req.samples)
            .map_err(|e| EngineError::Recognition(e.to_string()))?;

        let mut raw = String::new();
        for i in 0..state.full_n_segments() {
            // Lossy on purpose: a single malformed byte must not cost the user the whole
            // dictation.
            if let Some(text) = state.get_segment(i).and_then(|s| s.to_str_lossy().ok()) {
                raw.push_str(&text);
                raw.push('\n');
            }
        }

        Ok(TranscriptionResult {
            text: hvtt_core::engine::tidy(&raw),
            elapsed_ms: started.elapsed().as_millis(),
        })
    }
}
