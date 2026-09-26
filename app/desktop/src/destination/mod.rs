//! Pinned destinations.
//!
//! One narrow platform seam, exactly as planned from the first commit: everything above this
//! module is shared and testable, and only what is inside it knows about the Accessibility API
//! (macOS), UI Automation (Windows) or a browser extension.
//!
//! **Three ways in, in order.** A silent Accessibility write (native apps, Safari - works even
//! after he clicks away); the browser extension (Chrome); and a paste for everything else -
//! Electron apps like VS Code, Slack and Discord, and Chrome without the extension - which is
//! only made while nothing has moved since the keypress (`macos_paste`). Past that, the words
//! wait on the clipboard.
//!
//! Windows has the same three ways in: UI Automation (`windows_uia`), the same extension, and the
//! same gated paste (`windows_paste`).

#[cfg(target_os = "macos")]
pub mod macos_ax;
#[cfg(target_os = "macos")]
pub mod macos_paste;
#[cfg(windows)]
pub mod windows_paste;
#[cfg(windows)]
pub mod windows_uia;

pub mod chromium;

/// Why a pin attempt did not produce a destination. Each maps to one short, plain sentence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PinError {
    /// macOS Accessibility permission has not been granted yet.
    AccessibilityPermissionMissing,
    /// The focused thing is a password box. Never pinned, never dictated into.
    SecureField,
    /// Focus is not in a text field at all.
    NotATextField,
    /// A Chromium browser is frontmost but the extension is not installed or not running.
    BrowserExtensionMissing,
    /// The app is one we have no safe way to write into. Clipboard-only, not a failure.
    Unsupported { app: String },
    Other(String),
}

impl PinError {
    /// One short line for the composer. Never a dialog, never a stack trace.
    pub fn message(&self) -> String {
        match self {
            PinError::AccessibilityPermissionMissing => {
                "Huck needs Accessibility permission to type into other apps.".into()
            }
            PinError::SecureField => {
                "That is a password field — Huck will not dictate into it.".into()
            }
            PinError::NotATextField => "Click into a text field first.".into(),
            PinError::BrowserExtensionMissing => {
                "Install the Huck Voice browser extension to send text into Chrome.".into()
            }
            PinError::Unsupported { app } => {
                format!("{app} can't receive text directly — it will be copied instead.")
            }
            PinError::Other(m) => m.clone(),
        }
    }

    /// Is this a normal, expected situation rather than something that went wrong?
    ///
    /// Drives tone in the UI: an unsupported app is not an error, it is how v1 works.
    pub fn is_expected(&self) -> bool {
        matches!(self, PinError::Unsupported { .. } | PinError::NotATextField)
    }
}

/// Applications known to expose no safe write path. Clipboard-only in v1, by design.
///
/// Measured 2026-09-20: Chromium-based apps advertise `AXValue` as settable, return success from
/// the write, and change nothing. Detecting them by bundle id lets the UI say so calmly instead
/// of attempting a delivery that would silently fail.
pub fn is_known_unsupported(bundle_id: &str) -> bool {
    const ELECTRON_APPS: &[&str] = &[
        "com.microsoft.VSCode",
        "com.todesktop.230313mzl4w4u92", // Cursor
        "com.tinyspeck.slackmacgap",
        "com.hnc.Discord",
        "com.openai.chat",
        "com.anthropic.claudefordesktop",
        "notion.id",
        "com.spotify.client",
    ];
    ELECTRON_APPS.contains(&bundle_id)
}

/// The file name at the end of an executable path, lower-cased: `chrome.exe`.
fn exe_file(exe: &str) -> String {
    exe.rsplit(['/', '\\']).next().unwrap_or(exe).to_ascii_lowercase()
}

/// Executable names of Chromium browsers. Matched on the running process rather than a bundle
/// id so the check needs no `osascript` call. Windows names are matched on the whole file name.
pub fn is_chromium_executable(exe: &str) -> bool {
    const BROWSERS: &[&str] =
        &["Google Chrome", "Chromium", "Microsoft Edge", "Brave Browser", "Vivaldi"];
    const WINDOWS: &[&str] =
        &["chrome.exe", "chromium.exe", "msedge.exe", "brave.exe", "vivaldi.exe"];
    BROWSERS.iter().any(|b| exe.contains(b)) || WINDOWS.contains(&exe_file(exe).as_str())
}

/// Electron applications, by executable name.
///
/// They must be recognised *before* the Accessibility path is tried: their fields advertise
/// themselves as writable, the write returns success, and nothing happens. Detecting them up
/// front turns a silent failure into an honest "it'll be on your clipboard".
pub fn is_unsupported_executable(exe: &str) -> Option<String> {
    const ELECTRON: &[(&str, &str)] = &[
        ("Visual Studio Code", "VS Code"),
        ("Code Helper", "VS Code"),
        ("Cursor", "Cursor"),
        ("Slack", "Slack"),
        ("Discord", "Discord"),
        ("ChatGPT", "ChatGPT"),
        ("Claude", "Claude"),
        ("Notion", "Notion"),
        ("Spotify", "Spotify"),
    ];
    const WINDOWS: &[(&str, &str)] = &[
        ("code.exe", "VS Code"),
        ("cursor.exe", "Cursor"),
        ("slack.exe", "Slack"),
        ("discord.exe", "Discord"),
        ("chatgpt.exe", "ChatGPT"),
        ("claude.exe", "Claude"),
        ("notion.exe", "Notion"),
        ("spotify.exe", "Spotify"),
    ];
    let file = exe_file(exe);
    WINDOWS
        .iter()
        .find(|(name, _)| file == *name)
        .or_else(|| ELECTRON.iter().find(|(needle, _)| exe.contains(needle)))
        .map(|(_, pretty)| pretty.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_apps_are_phrased_as_normal_not_as_failure() {
        let e = PinError::Unsupported { app: "Slack".into() };
        assert!(e.is_expected());
        let m = e.message();
        assert!(m.contains("copied"), "should point at the clipboard, got: {m}");
        assert!(!m.to_lowercase().contains("error"), "must not read as an error: {m}");
    }

    #[test]
    fn every_pin_error_has_a_short_plain_message() {
        for e in [
            PinError::AccessibilityPermissionMissing,
            PinError::SecureField,
            PinError::NotATextField,
            PinError::BrowserExtensionMissing,
            PinError::Unsupported { app: "VS Code".into() },
        ] {
            let m = e.message();
            assert!(!m.is_empty());
            assert!(m.len() < 90, "composer lines stay short: {m:?}");
        }
    }

    #[test]
    fn known_electron_apps_are_recognised() {
        assert!(is_known_unsupported("com.microsoft.VSCode"));
        assert!(is_known_unsupported("com.tinyspeck.slackmacgap"));
        assert!(!is_known_unsupported("com.apple.TextEdit"));
        assert!(!is_known_unsupported("com.apple.Safari"));
    }

    #[test]
    fn chromium_browsers_are_recognised_from_the_executable_path() {
        assert!(is_chromium_executable("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"));
        assert!(is_chromium_executable("/Applications/Brave Browser.app/Contents/MacOS/Brave Browser"));
        assert!(!is_chromium_executable("/System/Applications/TextEdit.app/Contents/MacOS/TextEdit"));
        assert!(!is_chromium_executable("/Applications/Safari.app/Contents/MacOS/Safari"));
    }

    #[test]
    fn windows_programs_are_recognised_by_file_name() {
        assert!(is_chromium_executable(r"C:\Program Files\Google\Chrome\Application\chrome.exe"));
        assert!(is_chromium_executable(r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe"));
        assert!(!is_chromium_executable(r"C:\Windows\System32\notepad.exe"));
        assert_eq!(
            is_unsupported_executable(r"D:\Tools\Microsoft VS Code\Code.exe"),
            Some("VS Code".into())
        );
        assert_eq!(is_unsupported_executable(r"C:\Windows\System32\notepad.exe"), None);
    }

    #[test]
    fn electron_apps_are_caught_before_the_accessibility_path_is_tried() {
        // They claim to be writable and silently discard the write, so they must be identified
        // up front rather than after a delivery that appears to succeed.
        assert_eq!(is_unsupported_executable("/Applications/Slack.app/Contents/MacOS/Slack"),
                   Some("Slack".into()));
        assert_eq!(is_unsupported_executable(".../Visual Studio Code"), Some("VS Code".into()));
        assert_eq!(is_unsupported_executable(".../TextEdit"), None);
    }

    #[test]
    fn a_secure_field_is_never_treated_as_expected() {
        // It must stand out: dictating into a password box is a safety failure, not routine.
        assert!(!PinError::SecureField.is_expected());
    }
}
