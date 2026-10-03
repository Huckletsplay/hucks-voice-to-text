//! Where the installed program keeps its things.
//!
//! Never inside the Project Playground hub, never on the SSD, never next to the source. The
//! program must keep working with the drive unplugged, and that starts here.

use std::path::{Path, PathBuf};

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

/// Where the last five dictations' sound was kept until 2026-10-03 - now only so the program can
/// delete what is left there.
pub fn recordings_dir() -> Option<PathBuf> {
    data_dir().map(|d| d.join("recordings"))
}

pub fn settings_path() -> Option<PathBuf> {
    data_dir().map(|d| d.join("settings.json"))
}

/// Delete the recordings the program made while it kept them (2026-10-01 to 10-03) - and nothing
/// else: only files named as it named them, `recording-<ms>.wav` or `recording-<ms>-<n>.wav`, and
/// only real files, never a link or a folder. The folder goes too, once nothing else is in it
/// (Codex's twenty-first review: deleting the whole folder would take whatever he put there).
/// A `dir` that is itself a link is left alone - it leads somewhere else (Codex's twenty-second).
///
/// `Ok((deleted, could_not))`; no folder at all is `Ok((0, 0))`. `Err` when the folder could not
/// be looked through, so recordings may remain.
pub fn remove_old_recordings(dir: &Path) -> Result<(usize, usize), String> {
    match std::fs::symlink_metadata(dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((0, 0)),
        Err(e) => return Err(format!("could not look at {}: {e}", dir.display())),
        Ok(m) if !m.file_type().is_dir() => {
            return Err(format!("{} is not a plain folder (a link?), so it was left alone", dir.display()))
        }
        Ok(_) => {}
    }
    let entries = std::fs::read_dir(dir).map_err(|e| format!("could not read {}: {e}", dir.display()))?;
    let (mut deleted, mut stuck) = (0, 0);
    for entry in entries {
        // An entry that cannot even be read might be one of ours.
        let Ok(entry) = entry else {
            stuck += 1;
            continue;
        };
        if !entry.file_name().to_str().is_some_and(is_old_recording_name) {
            continue;
        }
        match entry.path().symlink_metadata() {
            Ok(m) if m.file_type().is_file() => match std::fs::remove_file(entry.path()) {
                Ok(()) => deleted += 1,
                Err(_) => stuck += 1,
            },
            // Its name, but a link or a folder: not something it made.
            Ok(_) => {}
            Err(_) => stuck += 1,
        }
    }
    // Refused unless empty - which is the point.
    let _ = std::fs::remove_dir(dir);
    Ok((deleted, stuck))
}

fn is_old_recording_name(name: &str) -> bool {
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let Some(middle) = name.strip_prefix("recording-").and_then(|n| n.strip_suffix(".wav")) else {
        return false;
    };
    match middle.split_once('-') {
        Some((ms, n)) => digits(ms) && digits(n),
        None => digits(middle),
    }
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
        assert!(recordings_dir().unwrap().starts_with(&d));
        assert!(settings_path().unwrap().starts_with(&d));
    }

    #[test]
    fn only_the_programs_own_old_recordings_are_deleted() {
        let dir = std::env::temp_dir().join(format!("hvtt-old-recordings-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("his folder")).unwrap();
        for name in ["recording-1759400000000.wav", "recording-1759400000000-2.wav"] {
            std::fs::write(dir.join(name), b"RIFF").unwrap();
        }
        for name in ["recording-notes.wav", "my-recording-1.wav", "recording-1.wav.txt", "song.wav"] {
            std::fs::write(dir.join(name), b"his").unwrap();
        }
        std::fs::write(dir.join("his folder").join("recording-1.wav"), b"his").unwrap();
        // A folder named like a recording is left alone too.
        std::fs::create_dir(dir.join("recording-2.wav")).unwrap();

        assert_eq!(remove_old_recordings(&dir), Ok((2, 0)));
        assert!(!dir.join("recording-1759400000000.wav").exists());
        assert!(!dir.join("recording-1759400000000-2.wav").exists());
        for name in ["recording-notes.wav", "my-recording-1.wav", "recording-1.wav.txt", "song.wav"] {
            assert!(dir.join(name).exists(), "{name} is his");
        }
        assert!(dir.join("his folder").join("recording-1.wav").exists());
        assert!(dir.join("recording-2.wav").is_dir());
        assert!(dir.exists(), "not empty, so kept");
        std::fs::remove_dir_all(&dir).unwrap();

        // Only its own files there: the folder goes with them.
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("recording-5.wav"), b"RIFF").unwrap();
        assert_eq!(remove_old_recordings(&dir), Ok((1, 0)));
        assert!(!dir.exists());
        // And no folder at all is nothing to do.
        assert_eq!(remove_old_recordings(&dir), Ok((0, 0)));
    }

    #[cfg(unix)]
    #[test]
    fn a_recordings_folder_that_is_a_link_is_left_alone() {
        let base = std::env::temp_dir().join(format!("hvtt-linked-recordings-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let elsewhere = base.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(elsewhere.join("recording-123.wav"), b"his").unwrap();
        let link = base.join("recordings");
        std::os::unix::fs::symlink(&elsewhere, &link).unwrap();

        assert!(remove_old_recordings(&link).is_err(), "said, not silently skipped");
        assert!(elsewhere.join("recording-123.wav").exists(), "nothing deleted through the link");
        assert!(link.symlink_metadata().is_ok(), "the link itself stays");
        std::fs::remove_dir_all(&base).unwrap();
    }
}
