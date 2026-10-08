//! Uninstall: take the program and everything it kept off this computer.
//!
//! A download gives him a way in, so the program gives a way out that leaves nothing behind -
//! and a tester a way back to the very first run.
//!
//! What goes: the app (to the Trash), settings and learned fixes, recovery drafts, downloaded
//! speech models, the Start at Login entry, the Microphone and Accessibility grants (so macOS
//! asks afresh next time), the browsers' pointer to the extension's helper, and what the system
//! cached for the window (`hvtt_core::paths::mac_leftovers`). What stays: Huck's Clipboard,
//! which is the system's and shared with Huck's other programs.
//!
//! **Windows:** the removing is done by the installer's own uninstaller (`HucksVoiceToText.iss`),
//! so Settings > Apps and the H's Uninstall… are one way out, not two. Here the program only finds
//! it and starts it; it waits for this copy to be gone, then removes the program, its shortcuts,
//! the Start with Windows entry, and what the program kept in the app-data folders.

use std::path::PathBuf;
#[cfg(target_os = "macos")]
use std::path::Path;

/// The uninstaller the installer left beside the program - `None` for a copy it did not put
/// there (a developer's build), which has nothing to hand over to.
#[cfg(windows)]
fn uninstaller() -> Option<PathBuf> {
    let beside = std::env::current_exe().ok()?.parent()?.join("unins000.exe");
    beside.is_file().then_some(beside)
}

#[cfg(windows)]
pub fn installed() -> bool {
    uninstaller().is_some()
}

/// Start the uninstaller, with no window and no questions: he has been asked in the box. The
/// caller leaves straight after - the uninstaller waits for that before it removes anything.
#[cfg(windows)]
pub fn start() -> Result<(), String> {
    let uninstaller = uninstaller().ok_or("This copy has no uninstaller beside it.")?;
    std::process::Command::new(uninstaller)
        .args(["/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART"])
        // Not the program's own folder: a process standing in it would keep it from being removed.
        .current_dir(std::env::temp_dir())
        .spawn()
        .map(drop)
        .map_err(|e| format!("The uninstaller could not be started: {e}"))
}

#[cfg(target_os = "macos")]
/// The app this copy is running from - `Some.app`, three folders above the program - or `None`
/// when it is a bare program run from a terminal, which has no app to remove.
pub fn running_app() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let app = exe.parent()?.parent()?.parent()?;
    (app.extension()? == "app").then(|| app.to_path_buf())
}

#[cfg(target_os = "macos")]
/// Everything but the app itself. Returns what could not be removed, for the log.
pub fn remove_what_it_kept(identifier: &str) -> Vec<String> {
    let mut stuck = Vec::new();
    // The grants: without this the next install shows ticks that belong to a copy that is gone.
    for service in ["Accessibility", "Microphone"] {
        let reset = std::process::Command::new("/usr/bin/tccutil").args(["reset", service, identifier]).status();
        if !reset.is_ok_and(|s| s.success()) {
            stuck.push(format!("the {service} permission could not be cleared"));
        }
    }
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from).filter(|h| h.is_absolute()) else {
        stuck.push("no home folder is known, so nothing in it was removed".into());
        return stuck;
    };
    for path in hvtt_core::paths::mac_leftovers(&home, &std::env::temp_dir()) {
        if let Err(why) = hvtt_core::paths::remove_leftover(&path) {
            stuck.push(why);
        }
    }
    stuck
}

#[cfg(target_os = "macos")]
/// Move a file or folder to the Trash, as Finder would: he can still put it back.
pub fn move_to_trash(path: &Path) -> bool {
    use objc2::msg_send;
    use objc2::runtime::{AnyClass, AnyObject, Bool};
    use objc2_foundation::NSString;
    let (Some(managers), Some(urls)) = (AnyClass::get(c"NSFileManager"), AnyClass::get(c"NSURL")) else {
        return false;
    };
    let text = NSString::from_str(&path.to_string_lossy());
    unsafe {
        let manager: *mut AnyObject = msg_send![managers, defaultManager];
        let url: *mut AnyObject = msg_send![urls, fileURLWithPath: &*text];
        if manager.is_null() || url.is_null() {
            return false;
        }
        let nowhere = std::ptr::null_mut::<*mut AnyObject>();
        let moved: Bool = msg_send![manager, trashItemAtURL: url, resultingItemURL: nowhere, error: nowhere];
        moved.as_bool()
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    /// On purpose, because it leaves a file in his Trash: `dev.sh test -p hvtt-desktop --lib -- --ignored a_file_can_be_moved_to_the_trash`.
    /// A wrong selector would stop the program here rather than at the end of an uninstall.
    #[test]
    #[ignore]
    fn a_file_can_be_moved_to_the_trash() {
        let file = std::env::temp_dir().join(format!("hvtt-trash-test-{}.txt", std::process::id()));
        std::fs::write(&file, "from Huck's Voice to Text's tests - safe to delete").unwrap();
        assert!(move_to_trash(&file));
        assert!(!file.exists());
        assert!(!move_to_trash(&file), "nothing there any more");
    }
}
