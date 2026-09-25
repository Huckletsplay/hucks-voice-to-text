//! Local settings. Small on purpose.
//!
//! `docs/user-experience.md` warns against a big settings interface, and the hub guardrail
//! repeats it. Each field here earns its place by being something that genuinely differs
//! between machines or between people.

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
    /// Words the recogniser habitually gets wrong. Cheap accuracy win, near-zero cost.
    pub vocabulary: Vec<String>,
    /// Which clipboard dictations go to: the normal one or Huck's own. One or the other.
    pub clipboard: ClipboardChoice,
    /// Pastes from Huck's clipboard. Only bound while Huck's clipboard is the choice; on the
    /// normal clipboard, Cmd+V already does the job.
    pub paste_shortcut: String,
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
/// **Option+Space on macOS.** Close to the thumb, unclaimed by the system (Control+Space is
/// input sources, Command+Space is Spotlight), and one chord rather than three keys — this is
/// the front door of a product whose whole promise is feeling instant.
///
/// Elsewhere Alt+Space opens the window menu, so it is not reused.
#[cfg(target_os = "macos")]
pub const DEFAULT_SHORTCUT: &str = "Alt+Space";
#[cfg(not(target_os = "macos"))]
pub const DEFAULT_SHORTCUT: &str = "Ctrl+Alt+Space";

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
            model: "ggml-base.en.bin".to_string(),
            input_device: None,
            keep_drafts: true,
            vocabulary: Vec::new(),
            clipboard: ClipboardChoice::System,
            paste_shortcut: DEFAULT_PASTE_SHORTCUT.to_string(),
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
    pub fn vocabulary_prompt(&self) -> Option<String> {
        if self.vocabulary.is_empty() {
            None
        } else {
            Some(self.vocabulary.join(", "))
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
        s.vocabulary = vec!["HyperFrames".into(), "Quintin".into()];
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
    fn the_macos_default_shortcut_is_option_space() {
        let s = Settings::default();
        if cfg!(target_os = "macos") {
            assert_eq!(s.shortcut, "Alt+Space");
            assert_eq!(describe_shortcut(&s.shortcut), "Option + Space");
        }
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
}
