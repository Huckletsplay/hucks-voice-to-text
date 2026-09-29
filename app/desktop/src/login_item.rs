//! Start at Login (macOS) / Start with Windows - the H's one-click startup switch, as in Huck's
//! Snip 'n' Clip.
//!
//! The operating system is the authority, so nothing is mirrored into settings: a saved choice
//! could disagree after he changes Login Items or Startup Apps by hand.
//!
//! - **macOS:** `SMAppService.mainApp` (macOS 13 and later), the same call Snip 'n' Clip makes.
//!   Older systems show the row switched off and greyed.
//! - **Windows:** a value under the current user's `Run` key, pointing at the running program.
//!   The installer removes it on uninstall.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginState {
    Off,
    On,
    /// Registered, but macOS has it switched off in Login Items until he approves it there.
    NeedsApproval,
    /// This system has no way to do it (macOS before 13).
    Unavailable,
}

/// The menu row: its words and whether it carries a tick.
pub fn presentation(state: LoginState) -> (&'static str, bool) {
    #[cfg(target_os = "macos")]
    const TITLE: &str = "Start at Login";
    #[cfg(not(target_os = "macos"))]
    const TITLE: &str = "Start with Windows";
    match state {
        LoginState::Off => (TITLE, false),
        LoginState::On => (TITLE, true),
        LoginState::NeedsApproval => ("Start at Login — Needs Approval…", false),
        LoginState::Unavailable => ("Start at Login — needs macOS 13", false),
    }
}

/// What a click on the row does, given what the system says now.
pub fn toggle() -> Result<(), String> {
    match state() {
        LoginState::Off => set(true),
        LoginState::On => set(false),
        LoginState::NeedsApproval => {
            open_approval();
            Ok(())
        }
        LoginState::Unavailable => Ok(()),
    }
}

// ---------------------------------------------------------------------------- macOS

#[cfg(target_os = "macos")]
mod platform {
    use super::LoginState;
    use objc2::msg_send;
    use objc2::runtime::{AnyClass, AnyObject, Bool};

    #[link(name = "ServiceManagement", kind = "framework")]
    extern "C" {}

    // SMAppServiceStatus
    const ENABLED: isize = 1;
    const REQUIRES_APPROVAL: isize = 2;

    fn class() -> Option<&'static AnyClass> {
        AnyClass::get(c"SMAppService")
    }

    fn main_app() -> Option<&'static AnyObject> {
        let class = class()?;
        let service: *mut AnyObject = unsafe { msg_send![class, mainApp] };
        unsafe { service.as_ref() }
    }

    pub fn state() -> LoginState {
        let Some(service) = main_app() else { return LoginState::Unavailable };
        // A never-registered app reports "not found" on macOS 26; registering is still right.
        let status: isize = unsafe { msg_send![service, status] };
        match status {
            ENABLED => LoginState::On,
            REQUIRES_APPROVAL => LoginState::NeedsApproval,
            _ => LoginState::Off,
        }
    }

    pub fn set(on: bool) -> Result<(), String> {
        let service = main_app().ok_or("Start at Login needs macOS 13 or later.")?;
        let no_error = std::ptr::null_mut::<*mut AnyObject>();
        let ok: Bool = unsafe {
            if on {
                msg_send![service, registerAndReturnError: no_error]
            } else {
                msg_send![service, unregisterAndReturnError: no_error]
            }
        };
        if ok.as_bool() {
            Ok(())
        } else if on {
            Err("macOS would not add Huck's Voice to Text to Login Items.".into())
        } else {
            Err("macOS would not remove Huck's Voice to Text from Login Items.".into())
        }
    }

    pub fn open_approval() {
        if let Some(class) = class() {
            let _: () = unsafe { msg_send![class, openSystemSettingsLoginItems] };
        }
    }
}

// ---------------------------------------------------------------------------- Windows

#[cfg(windows)]
mod platform {
    use super::LoginState;
    use windows::core::{w, PCWSTR};
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{
        RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ,
    };

    const RUN: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");
    const NAME: PCWSTR = w!("HucksVoiceToText");

    fn configured() -> Option<String> {
        let mut buffer = vec![0u16; 1024];
        let mut bytes = (buffer.len() * 2) as u32;
        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                RUN,
                NAME,
                RRF_RT_REG_SZ,
                None,
                Some(buffer.as_mut_ptr().cast()),
                Some(&mut bytes),
            )
        };
        if status != ERROR_SUCCESS {
            return None;
        }
        let len = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
        Some(String::from_utf16_lossy(&buffer[..len]))
    }

    fn current_exe() -> Option<String> {
        std::env::current_exe().ok().map(|p| p.display().to_string())
    }

    pub fn state() -> LoginState {
        match (configured(), current_exe()) {
            (Some(command), Some(exe)) if super::command_points_at(&command, &exe) => LoginState::On,
            _ => LoginState::Off,
        }
    }

    pub fn set(on: bool) -> Result<(), String> {
        let status = if on {
            let exe = current_exe().ok_or("Windows would not say where this program is.")?;
            let command: Vec<u16> = format!("\"{exe}\"").encode_utf16().chain([0]).collect();
            unsafe {
                RegSetKeyValueW(
                    HKEY_CURRENT_USER,
                    RUN,
                    NAME,
                    REG_SZ.0,
                    Some(command.as_ptr().cast()),
                    (command.len() * 2) as u32,
                )
            }
        } else {
            match unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN, NAME) } {
                s if s.0 == 2 => ERROR_SUCCESS, // already gone: ERROR_FILE_NOT_FOUND
                s => s,
            }
        };
        if status == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(format!("Windows would not change the startup setting (error {}).", status.0))
        }
    }

    pub fn open_approval() {}
}

#[cfg(not(any(target_os = "macos", windows)))]
mod platform {
    use super::LoginState;
    pub fn state() -> LoginState {
        LoginState::Unavailable
    }
    pub fn set(_: bool) -> Result<(), String> {
        Err("Not available on this system.".into())
    }
    pub fn open_approval() {}
}

pub use platform::{open_approval, set, state};

/// Does a `Run` command start this executable? Quotes, arguments and letter case aside - a
/// program moved elsewhere does not count, so the tick never lies about what starts.
#[cfg_attr(not(windows), allow(dead_code))]
fn command_points_at(command: &str, exe: &str) -> bool {
    let command = command.trim();
    let path = match command.strip_prefix('"') {
        Some(rest) => rest.split('"').next().unwrap_or(""),
        None => command.split(" -").next().unwrap_or("").trim(),
    };
    path.eq_ignore_ascii_case(exe)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_run_command_is_matched_to_this_program_only() {
        let exe = r"C:\Programs\HucksVoiceToText\hucks-voice-to-text.exe";
        assert!(command_points_at(&format!("\"{exe}\""), exe));
        assert!(command_points_at(&format!("\"{}\" --quiet", exe.to_uppercase()), exe));
        assert!(command_points_at(exe, exe));
        assert!(!command_points_at(r#""D:\Old\hucks-voice-to-text.exe""#, exe));
        assert!(!command_points_at("", exe));
    }

    /// On purpose only: writes the real startup entry (for this test binary), then removes it.
    #[cfg(windows)]
    #[test]
    #[ignore]
    fn start_with_windows_switches_on_and_off() {
        set(true).unwrap();
        assert_eq!(state(), LoginState::On);
        set(false).unwrap();
        assert_eq!(state(), LoginState::Off);
        set(false).unwrap(); // already off is not an error
    }

    #[test]
    fn the_row_only_ticks_when_it_is_really_on() {
        assert!(presentation(LoginState::On).1);
        assert!(!presentation(LoginState::Off).1);
        assert!(!presentation(LoginState::NeedsApproval).1);
        assert!(!presentation(LoginState::Unavailable).1);
    }
}
