//! Where the installed program keeps its things.
//!
//! Never inside the Project Playground hub, never on the SSD, never next to the source. The
//! program must keep working with the drive unplugged, and that starts here.

use std::path::PathBuf;

/// The app-data folder name, matching the bundle identifier in `tauri.conf.json`.
/// Huck's Voice to Text owns its identity outright, so it coexists with anything else on the
/// machine and shares settings or permissions with nothing.
#[cfg(target_os = "macos")]
pub const APP_DIR: &str = "com.huck.voice-to-text";
#[cfg(not(target_os = "macos"))]
pub const APP_DIR: &str = "Huck's Voice to Text";

/// `~/Library/Application Support/com.huck.voice-to-text` on macOS,
/// `%LOCALAPPDATA%\Huck's Voice to Text` on Windows.
pub fn data_dir() -> Option<PathBuf> {
    dirs::data_local_dir().map(|d| d.join(APP_DIR))
}

pub fn models_dir() -> Option<PathBuf> {
    data_dir().map(|d| d.join("models"))
}

pub fn drafts_dir() -> Option<PathBuf> {
    data_dir().map(|d| d.join("drafts"))
}

pub fn settings_path() -> Option<PathBuf> {
    data_dir().map(|d| d.join("settings.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_dir_is_absolute_and_not_on_the_project_drive() {
        let d = data_dir().expect("a home directory should exist");
        assert!(d.is_absolute());
        assert!(
            !d.to_string_lossy().contains("Project Playground"),
            "app data must never resolve into the hub: {}",
            d.display()
        );
    }

    #[test]
    fn everything_lives_under_one_folder() {
        let d = data_dir().unwrap();
        assert!(models_dir().unwrap().starts_with(&d));
        assert!(drafts_dir().unwrap().starts_with(&d));
        assert!(settings_path().unwrap().starts_with(&d));
    }
}
