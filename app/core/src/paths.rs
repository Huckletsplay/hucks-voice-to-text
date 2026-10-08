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

/// Everything the program leaves on a Mac besides the app itself, for Uninstall to remove: his
/// settings, learned fixes, drafts and speech models (the app-data folder), what the system keeps
/// for the app's window (caches, WebKit's storage, preferences), the file that tells each
/// Chromium browser where the extension's helper is, and a downloaded update.
///
/// Fixed names under `home` and `temp`, every one of them this program's own - nothing is found
/// by searching, so nothing of anyone else's can be on the list. Huck's Clipboard is not here:
/// it is the system's, and Huck's other programs share it. Laid out as macOS lays a home folder
/// out; the Windows uninstaller is the installer's own.
pub fn mac_leftovers(home: &Path, temp: &Path) -> Vec<PathBuf> {
    let library = home.join("Library");
    // The installed app's name for these, and the bare program's (run from a terminal).
    let names = [APP_DIR_MAC, "hvtt-desktop"];
    let mut all = vec![library.join("Application Support").join(APP_DIR_MAC)];
    for name in names {
        all.push(library.join("Caches").join(name));
        all.push(library.join("WebKit").join(name));
        all.push(library.join("HTTPStorages").join(name));
        all.push(library.join("HTTPStorages").join(format!("{name}.binarycookies")));
        all.push(library.join("Preferences").join(format!("{name}.plist")));
        all.push(library.join("Saved Application State").join(format!("{name}.savedState")));
    }
    for browser in [
        "Google/Chrome",
        "Google/Chrome Beta",
        "Microsoft Edge",
        "BraveSoftware/Brave-Browser",
        "Chromium",
    ] {
        all.push(
            library.join("Application Support").join(browser).join("NativeMessagingHosts").join(NATIVE_HOST_FILE),
        );
    }
    all.push(temp.join("HucksVoiceToText-Update"));
    all
}

/// The Mac's name for the app-data folder: the bundle identifier.
pub const APP_DIR_MAC: &str = "com.huck.voice-to-text";
/// The file `install-native-host.sh` puts in each browser's `NativeMessagingHosts`.
pub const NATIVE_HOST_FILE: &str = "com.huck.voicetotext.json";

/// Remove one of those, whatever it is: a folder with all in it, a file - or, if something has
/// put a link there, the link alone, never what it leads to. `Ok(false)` when nothing was there.
pub fn remove_leftover(path: &Path) -> Result<bool, String> {
    let kind = match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(format!("could not look at {}: {e}", path.display())),
        Ok(m) => m.file_type(),
    };
    // `remove_dir_all` does not follow links inside the folder either.
    let removed = if kind.is_dir() { std::fs::remove_dir_all(path) } else { std::fs::remove_file(path) };
    removed.map(|()| true).map_err(|e| format!("could not remove {}: {e}", path.display()))
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

    #[test]
    fn uninstall_removes_only_what_is_this_programs_own() {
        let root = std::env::temp_dir().join(format!("hvtt-leftovers-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (home, temp) = (root.join("home"), root.join("tmp"));
        let support = home.join("Library/Application Support");
        // His things, and somebody else's beside them.
        std::fs::create_dir_all(support.join("com.huck.voice-to-text/models")).unwrap();
        std::fs::write(support.join("com.huck.voice-to-text/settings.json"), "{}").unwrap();
        std::fs::create_dir_all(support.join("Google/Chrome/NativeMessagingHosts")).unwrap();
        std::fs::write(support.join("Google/Chrome/NativeMessagingHosts/com.huck.voicetotext.json"), "{}").unwrap();
        std::fs::write(support.join("Google/Chrome/NativeMessagingHosts/com.other.json"), "{}").unwrap();
        std::fs::create_dir_all(home.join("Library/Caches/com.huck.voice-to-text")).unwrap();
        std::fs::create_dir_all(home.join("Library/Caches/com.other.app")).unwrap();
        std::fs::create_dir_all(temp.join("HucksVoiceToText-Update")).unwrap();
        // Something has put a link where WebKit's folder would be: the link goes, not his file.
        let precious = root.join("precious");
        std::fs::create_dir_all(&precious).unwrap();
        std::fs::write(precious.join("keep.txt"), "keep").unwrap();
        std::fs::create_dir_all(home.join("Library/WebKit")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&precious, home.join("Library/WebKit/com.huck.voice-to-text")).unwrap();

        let all = mac_leftovers(&home, &temp);
        assert!(all.iter().all(|p| p.starts_with(&home) || p.starts_with(&temp)));
        let removed = all.iter().filter(|p| remove_leftover(p).unwrap()).count();
        assert_eq!(removed, if cfg!(unix) { 5 } else { 4 });

        assert!(!support.join("com.huck.voice-to-text").exists());
        assert!(!support.join("Google/Chrome/NativeMessagingHosts/com.huck.voicetotext.json").exists());
        assert!(!home.join("Library/Caches/com.huck.voice-to-text").exists());
        assert!(!temp.join("HucksVoiceToText-Update").exists());
        assert!(support.join("Google/Chrome/NativeMessagingHosts/com.other.json").exists());
        assert!(home.join("Library/Caches/com.other.app").exists());
        assert!(precious.join("keep.txt").exists(), "a link is removed, never followed");
        assert!(std::fs::symlink_metadata(home.join("Library/WebKit/com.huck.voice-to-text")).is_err());
        // Nothing there any more: nothing to do, and no error.
        assert!(all.iter().all(|p| remove_leftover(p) == Ok(false)));
        let _ = std::fs::remove_dir_all(&root);
    }
}
