//! Local settings. Small on purpose.
//!
//! Each field here earns its place by being something that genuinely differs between machines or
//! between people; the program deliberately avoids a large settings interface.

use crate::paths;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// Global start/stop shortcut, in Tauri accelerator syntax.
    ///
    /// One binding toggles: first press starts listening, second press stops and transcribes.
    /// It lives here rather than in the recording code so it can be rebound without touching
    /// anything that records.
    pub shortcut: String,
    /// Whisper model file name inside the models directory.
    pub model: String,
    /// Input device name; `None` means the system default.
    pub input_device: Option<String>,
    /// Keep recovery drafts on disk. Dictated speech is sensitive; this can be turned off.
    pub keep_drafts: bool,
    /// No longer used: the last five recordings were kept from 2026-10-01 until his decision of
    /// 2026-10-03, "we don't need them at all". Still read, so an older settings file loads
    /// (unknown names are refused), and never written again.
    #[serde(skip_serializing)]
    pub keep_recordings: bool,
    /// Words the recogniser habitually gets wrong. Cheap accuracy win, near-zero cost.
    pub vocabulary: Vec<String>,
    /// Which clipboard dictations go to: the normal one or Huck's own. One or the other.
    pub clipboard: ClipboardChoice,
    /// Pastes from Huck's clipboard. Only bound while Huck's clipboard is the choice; on the
    /// normal clipboard, Cmd+V already does the job.
    pub paste_shortcut: String,
    /// Remember his fixes in the box and apply them from then on. On unless he turns it off,
    /// from the box or H › Settings.
    pub learning: bool,
    /// What Learning has remembered, oldest first.
    pub fixes: Vec<crate::learning::Fix>,
    /// How hard the box works to show the words while he talks.
    pub live_words: LiveWords,
    /// H › Settings › When I Stop Talking: wait the full moment after every stop, so even a last
    /// word fainter than the microphone's own hiss is kept (Codex's eleventh review), instead of
    /// finishing as soon as he is back down to the room. His choice, 2026-10-02: an option, off by
    /// default.
    pub careful_stop: bool,
    /// H › Settings › Check for Updates When It Opens: ask GitHub once, quietly, each time the
    /// program starts, and say something only if there is a newer version. His decision,
    /// 2026-10-02 - on by default; off, the network is used only when he asks.
    pub check_updates_on_start: bool,
    /// The program version and model the speed check last ran for ("0.1.7 ggml-...bin"): it runs
    /// again when either changes.
    pub speed_checked: String,
}

/// H › Settings › Live Words: how much of the processor showing the words while he talks may
/// take. Decided with him 2026-09-28, so a slower computer is never bogged down. Whichever is
/// chosen, the words that are sent are the same.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LiveWords {
    /// Refreshed about three times a second; the processor at most about half busy.
    #[default]
    AsYouTalk,
    /// Refreshed about once a second; the processor at most about a quarter busy.
    Lighter,
    /// Nothing recognised until he pauses or sends, as the first version did.
    Off,
}

impl LiveWords {
    /// The rest before the next live pass, given how long the last one took; `None` when live
    /// words are off. The rest scales with the pass, so a slow machine refreshes less often
    /// instead of being kept busy.
    pub fn rest_after(self, pass: std::time::Duration) -> Option<std::time::Duration> {
        use std::time::Duration;
        match self {
            LiveWords::AsYouTalk => Some(pass.max(Duration::from_millis(300))),
            LiveWords::Lighter => Some((pass * 3).max(Duration::from_millis(1000))),
            LiveWords::Off => None,
        }
    }
}

/// Where the safety-net copy of every dictation goes - one clipboard, never both.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClipboardChoice {
    /// The normal clipboard: paste with Cmd+V (Ctrl+V on Windows).
    #[default]
    System,
    /// Huck's own clipboard, so dictation never overwrites something he copied on purpose.
    /// Pasted with `paste_shortcut`, and shared by name with Huck's other programs.
    Huck,
}

/// Paste from Huck's clipboard. Control + Option + V: next to the paste he already knows, and
/// unclaimed by the system.
#[cfg(target_os = "macos")]
pub const DEFAULT_PASTE_SHORTCUT: &str = "Ctrl+Alt+V";
#[cfg(not(target_os = "macos"))]
pub const DEFAULT_PASTE_SHORTCUT: &str = "Ctrl+Alt+Shift+V";

/// The default global shortcut.
///
/// **Option+Space on macOS, Alt+Space on Windows** - the same keys under the same thumb. Close to
/// the thumb, unclaimed by the system on macOS (Control+Space is input sources, Command+Space is
/// Spotlight), and one chord rather than three keys — this is the front door of a product whose
/// whole promise is feeling instant.
///
/// On Windows Alt+Space also opens a window's system menu; a registered shortcut takes precedence,
/// as PowerToys Run's does, so that menu is not reachable by keyboard while the program runs.
/// Chosen by Quintin 2026-09-26 over the earlier Ctrl+Alt+Space, to match the Mac.
pub const DEFAULT_SHORTCUT: &str = "Alt+Space";

/// Render an accelerator the way the user's keyboard is labelled.
///
/// Tauri's syntax is not what anyone calls these keys on a Mac.
pub fn describe_shortcut(accelerator: &str) -> String {
    accelerator
        .split('+')
        .map(|part| match part.trim().to_ascii_lowercase().as_str() {
            "alt" | "option" => if cfg!(target_os = "macos") { "Option" } else { "Alt" },
            "cmd" | "command" | "super" | "meta" => "Command",
            "cmdorctrl" | "commandorcontrol" => {
                if cfg!(target_os = "macos") { "Command" } else { "Ctrl" }
            }
            "ctrl" | "control" => "Ctrl",
            "shift" => "Shift",
            "space" => "Space",
            other => return other.to_uppercase(),
        }
        .to_string())
        .collect::<Vec<_>>()
        .join(" + ")
}

/// A shortcut must have a non-modifier key, or it can never fire.
pub fn shortcut_looks_valid(accelerator: &str) -> bool {
    const MODIFIERS: &[&str] = &[
        "alt", "option", "cmd", "command", "super", "meta", "cmdorctrl",
        "commandorcontrol", "ctrl", "control", "shift",
    ];
    let parts: Vec<&str> = accelerator
        .split('+')
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .collect();
    !parts.is_empty()
        && parts
            .iter()
            .any(|p| !MODIFIERS.contains(&p.to_ascii_lowercase().as_str()))
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            shortcut: DEFAULT_SHORTCUT.to_string(),
            // The model inside the program: "Best" on the Mac, "Quick" on Windows.
            model: crate::models::built_in().file.to_string(),
            input_device: None,
            keep_drafts: true,
            keep_recordings: false,
            vocabulary: Vec::new(),
            clipboard: ClipboardChoice::System,
            paste_shortcut: DEFAULT_PASTE_SHORTCUT.to_string(),
            learning: true,
            fixes: Vec::new(),
            live_words: LiveWords::AsYouTalk,
            careful_stop: false,
            check_updates_on_start: true,
            speed_checked: String::new(),
        }
    }
}

impl Settings {
    /// Load from the OS app-data directory, falling back to defaults.
    ///
    /// A corrupt or unreadable settings file yields defaults rather than an error: the program
    /// starting with the wrong shortcut is recoverable, the program refusing to start is not.
    pub fn load() -> Self {
        match paths::settings_path() {
            Some(p) => Self::load_from(&p),
            None => Settings::default(),
        }
    }

    pub fn load_from(path: &Path) -> Self {
        fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> std::io::Result<()> {
        let path = paths::settings_path().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "no app data directory")
        })?;
        self.save_to(&path)
    }

    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        fs::write(path, json)
    }

    /// The initial prompt biasing Whisper toward the user's own jargon.
    ///
    /// The words his fixes taught are part of it, so the model starts hearing them right, not
    /// only being corrected afterwards.
    pub fn vocabulary_prompt(&self) -> Option<String> {
        let mut words: Vec<&str> = self.vocabulary.iter().map(String::as_str).collect();
        for fix in &self.fixes {
            if !words.contains(&fix.to.as_str()) {
                words.push(&fix.to);
            }
        }
        if words.is_empty() {
            None
        } else {
            Some(words.join(", "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "hvtt-settings-{tag}-{}.json",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn settings_round_trip_through_disk() {
        let p = temp_path("roundtrip");
        let mut s = Settings::default();
        s.vocabulary = vec!["HyperFrames".into(), "Taylor".into()];
        s.model = "ggml-small.en.bin".into();
        s.save_to(&p).unwrap();

        let back = Settings::load_from(&p);
        assert_eq!(back, s);
        fs::remove_file(&p).ok();
    }

    #[test]
    fn a_corrupt_settings_file_falls_back_to_defaults_instead_of_failing() {
        let p = temp_path("corrupt");
        fs::write(&p, "{ this is not json ").unwrap();
        assert_eq!(Settings::load_from(&p), Settings::default());
        fs::remove_file(&p).ok();
    }

    #[test]
    fn a_missing_settings_file_yields_defaults() {
        let p = temp_path("missing");
        assert_eq!(Settings::load_from(&p), Settings::default());
    }

    #[test]
    fn a_partial_settings_file_keeps_defaults_for_the_rest() {
        let p = temp_path("partial");
        fs::write(&p, r#"{"model":"ggml-small.en.bin"}"#).unwrap();
        let s = Settings::load_from(&p);
        assert_eq!(s.model, "ggml-small.en.bin");
        assert_eq!(s.shortcut, Settings::default().shortcut);
        assert!(s.keep_drafts);
        fs::remove_file(&p).ok();
    }

    #[test]
    fn the_normal_clipboard_is_the_default_and_older_files_keep_it() {
        // A settings file from before the choice existed must still load, on the normal one.
        let p = temp_path("pre-clipboard");
        fs::write(&p, r#"{"shortcut":"Alt+Space","keep_drafts":true}"#).unwrap();
        let s = Settings::load_from(&p);
        assert_eq!(s.clipboard, ClipboardChoice::System);
        assert_eq!(Settings::default().clipboard, ClipboardChoice::System);
        fs::remove_file(&p).ok();
    }

    #[test]
    fn a_settings_file_from_when_recordings_were_kept_still_loads_and_drops_the_setting() {
        // Unknown names are refused, so the retired switch must still be read - or his settings
        // would all fall back to defaults.
        let p = temp_path("recordings");
        fs::write(&p, r#"{"model":"ggml-small.en.bin","keep_recordings":true}"#).unwrap();
        let s = Settings::load_from(&p);
        assert_eq!(s.model, "ggml-small.en.bin");
        assert!(!serde_json::to_string(&s).unwrap().contains("keep_recordings"), "never written again");
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn the_paste_shortcut_is_usable_and_never_the_dictation_shortcut() {
        let s = Settings::default();
        assert!(shortcut_looks_valid(&s.paste_shortcut));
        assert_ne!(s.paste_shortcut, s.shortcut);
    }

    #[test]
    fn the_clipboard_choice_survives_a_restart() {
        let p = temp_path("huck-clipboard");
        let mut s = Settings::default();
        s.clipboard = ClipboardChoice::Huck;
        s.save_to(&p).unwrap();
        assert_eq!(Settings::load_from(&p).clipboard, ClipboardChoice::Huck);
        fs::remove_file(&p).ok();
    }

    #[test]
    fn the_default_shortcut_is_option_space_on_the_mac_and_alt_space_on_windows() {
        let s = Settings::default();
        assert_eq!(s.shortcut, "Alt+Space");
        assert_eq!(
            describe_shortcut(&s.shortcut),
            if cfg!(target_os = "macos") { "Option + Space" } else { "Alt + Space" }
        );
        assert!(shortcut_looks_valid(&s.shortcut));
    }

    #[test]
    fn a_rebound_shortcut_survives_a_restart() {
        // The binding lives in settings, not in the recording code, so rebinding is a setting
        // change and nothing else.
        let p = temp_path("rebind");
        let s = Settings { shortcut: "Cmd+Shift+Space".into(), ..Default::default() };
        s.save_to(&p).unwrap();
        assert_eq!(Settings::load_from(&p).shortcut, "Cmd+Shift+Space");
        fs::remove_file(&p).ok();
    }

    #[test]
    fn a_shortcut_with_only_modifiers_is_rejected() {
        assert!(!shortcut_looks_valid("Alt+Shift"));
        assert!(!shortcut_looks_valid(""));
        assert!(!shortcut_looks_valid("   "));
        assert!(shortcut_looks_valid("Alt+Space"));
        assert!(shortcut_looks_valid("F5"));
    }

    #[test]
    fn shortcuts_are_described_in_keyboard_words() {
        assert_eq!(describe_shortcut("Alt+Space"),
                   if cfg!(target_os = "macos") { "Option + Space" } else { "Alt + Space" });
        assert_eq!(describe_shortcut("Shift+F5"), "Shift + F5");
    }

    #[test]
    fn vocabulary_becomes_a_prompt_only_when_it_has_words() {
        assert_eq!(Settings::default().vocabulary_prompt(), None);
        let s = Settings { vocabulary: vec!["Huck".into(), "exFAT".into()], ..Default::default() };
        assert_eq!(s.vocabulary_prompt().as_deref(), Some("Huck, exFAT"));
    }

    #[test]
    fn live_words_never_keep_the_processor_busier_than_chosen() {
        use std::time::Duration;
        let ms = Duration::from_millis;
        // A fast machine: refreshed as often as the floor allows.
        assert_eq!(LiveWords::AsYouTalk.rest_after(ms(100)), Some(ms(300)));
        assert_eq!(LiveWords::Lighter.rest_after(ms(100)), Some(ms(1000)));
        // A slow machine: resting at least as long as it worked (half), or three times (a quarter).
        assert_eq!(LiveWords::AsYouTalk.rest_after(ms(800)), Some(ms(800)));
        assert_eq!(LiveWords::Lighter.rest_after(ms(800)), Some(ms(2400)));
        assert_eq!(LiveWords::Off.rest_after(ms(100)), None);
        assert_eq!(serde_json::to_string(&LiveWords::AsYouTalk).unwrap(), "\"as_you_talk\"");
    }

    #[test]
    fn learned_words_join_the_prompt_once() {
        use crate::learning::Fix;
        let s = Settings {
            vocabulary: vec!["Huck's".into()],
            fixes: vec![
                Fix { from: "hux".into(), to: "Huck's".into() },
                Fix { from: "github".into(), to: "GitHub".into() },
            ],
            ..Default::default()
        };
        assert_eq!(s.vocabulary_prompt().as_deref(), Some("Huck's, GitHub"));
    }

    #[test]
    fn a_settings_file_from_before_learning_still_loads_with_learning_on() {
        let p = temp_path("pre-learning");
        std::fs::write(&p, r#"{"shortcut":"Alt+Space","keep_drafts":false}"#).unwrap();
        let s = Settings::load_from(&p);
        assert!(!s.keep_drafts, "the old file was read, not replaced by defaults");
        assert!(s.learning);
        assert!(s.fixes.is_empty());
        assert_eq!(s.live_words, LiveWords::AsYouTalk);
        let _ = std::fs::remove_file(&p);
    }
}
