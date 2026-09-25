//! Whisper recognition, via whisper.cpp.
//!
//! Loaded once and kept resident — `docs/user-experience.md` decides to pay RAM rather than
//! seconds so the hotkey responds instantly. Behind `hvtt_core::engine::Transcriber`, so
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
        let bundled = std::env::current_exe().ok().and_then(|exe| {
            Some(exe.parent()?.parent()?.join("Resources").join("models").join(model_file))
        });
        match bundled {
            Some(b) if b.exists() => Some(b),
            _ => own,
        }
    }
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

        if let Some(prompt) = req.vocabulary_prompt.as_deref() {
            params.set_initial_prompt(prompt);
        }

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
