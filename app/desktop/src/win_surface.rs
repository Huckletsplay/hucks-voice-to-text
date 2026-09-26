//! The floating box on Windows: shown without ever taking the foreground.
//!
//! On macOS the accessory activation policy does this for the whole app. Windows has no such
//! switch - showing a window normally activates it, which would pull him out of the text box he
//! is dictating into. So the box carries the same styles as Snip 'n' Clip's floating recording
//! controller: `WS_EX_NOACTIVATE` (clicking it does not activate it either) and
//! `WS_EX_TOOLWINDOW` (never in Alt+Tab), and it is shown with `SW_SHOWNOACTIVATE`.
//!
//! Show and hide both go through here rather than through Tauri, so the window's visible state
//! has one owner.

use tauri::WebviewWindow;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowLongPtrW, SetForegroundWindow, SetWindowLongPtrW, SetWindowPos, ShowWindow,
    GWL_EXSTYLE, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW, SW_HIDE,
    SW_SHOWNOACTIVATE, WS_EX_APPWINDOW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
};

fn hwnd(w: &WebviewWindow) -> Option<HWND> {
    // Tauri's handle comes from its own copy of the `windows` crate; the raw pointer is the same.
    w.hwnd().ok().map(|h| HWND(h.0 as *mut _))
}

fn set_no_activate(h: HWND, on: bool) {
    unsafe {
        let style = GetWindowLongPtrW(h, GWL_EXSTYLE);
        let bits = (WS_EX_NOACTIVATE.0 | WS_EX_TOOLWINDOW.0) as isize;
        // Tauri marks the window WS_EX_APPWINDOW, which would put it back in Alt+Tab.
        let next = if on {
            (style | bits) & !(WS_EX_APPWINDOW.0 as isize)
        } else {
            style & !(WS_EX_NOACTIVATE.0 as isize)
        };
        SetWindowLongPtrW(h, GWL_EXSTYLE, next);
    }
}

/// Once, at startup.
pub fn prepare(w: &WebviewWindow) {
    if let Some(h) = hwnd(w) {
        set_no_activate(h, true);
    }
}

/// Bring the box into view, on top, without activating it.
pub fn show(w: &WebviewWindow) {
    let Some(h) = hwnd(w) else { return };
    unsafe {
        let _ = ShowWindow(h, SW_SHOWNOACTIVATE);
        let _ = SetWindowPos(
            h,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
    }
}

pub fn hide(w: &WebviewWindow) {
    if let Some(h) = hwnd(w) {
        unsafe {
            let _ = ShowWindow(h, SW_HIDE);
        }
        // Back to never-activate, in case the key prompt had lifted it.
        set_no_activate(h, true);
    }
}

static SINGLE_INSTANCE: parking_lot::Mutex<Option<isize>> = parking_lot::Mutex::new(None);

/// Let go of the one-copy marker just before starting an update's installer. The installer looks
/// for it (`AppMutex`) and, run silently, gives up if this copy has not finished quitting yet -
/// which it may not have, a few milliseconds after starting the installer.
pub fn release_single_instance() {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::Threading::ReleaseMutex;
    if let Some(h) = SINGLE_INSTANCE.lock().take() {
        unsafe {
            let _ = ReleaseMutex(HANDLE(h as *mut _));
            let _ = CloseHandle(HANDLE(h as *mut _));
        }
    }
}

/// One copy at a time, as Snip 'n' Clip does. A second copy could not register the shortcut and
/// would add a second H; it leaves quietly instead. The installer names the same mutex
/// (`AppMutex`) to find the running copy and close it for an update.
///
/// Returns false when another copy already holds it. The handle is kept until the process ends,
/// or until `release_single_instance` hands it over to an updating installer.
pub fn claim_single_instance() -> bool {
    use windows::core::w;
    use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows::Win32::System::Threading::CreateMutexW;
    match unsafe { CreateMutexW(None, true, w!("Local\\HucksVoiceToText.SingleInstance")) } {
        Ok(held) => {
            let why = unsafe { GetLastError() };
            *SINGLE_INSTANCE.lock() = Some(held.0 as isize);
            why != ERROR_ALREADY_EXISTS
        }
        // Without a mutex there is no way to tell; running is the safer mistake.
        Err(_) => true,
    }
}

/// The window he was working in, if the one in front now is somewhere he could be typing: not
/// this program, and not the taskbar or its notification area.
pub fn outside_foreground() -> Option<isize> {
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};
    let h = unsafe { GetForegroundWindow() };
    if h.0.is_null() {
        return None;
    }
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(h, Some(&mut pid)) };
    let class = crate::destination::windows_paste::class_of(h.0 as isize);
    let shell = matches!(
        class.as_str(),
        "Shell_TrayWnd" | "Shell_SecondaryTrayWnd" | "NotifyIconOverflowWindow"
            | "TopLevelWindowForOverflowXamlIsland" | "Progman" | "WorkerW"
    );
    (pid != std::process::id() && !shell).then_some(h.0 as isize)
}

/// Hand the foreground back to the window he was in before he opened the H's menu.
///
/// Opening a tray menu makes Windows put the program's own hidden window in front, and it stays
/// there after the menu closes. With it in front, the shortcut prompt heard no keys until he
/// clicked into another window (found 2026-09-26, from a recording), and Start Dictation from
/// the menu had nothing to aim at. Giving the foreground straight back does what his click did.
pub fn give_back_foreground(to: isize) {
    use windows::Win32::UI::WindowsAndMessaging::IsWindow;
    let h = HWND(to as *mut _);
    unsafe {
        if IsWindow(Some(h)).as_bool() {
            let _ = SetForegroundWindow(h);
        }
    }
}

/// The one time the box takes the keyboard: Settings › Shortcuts, which he chose from the menu.
pub fn show_for_keys(w: &WebviewWindow) {
    let Some(h) = hwnd(w) else { return };
    set_no_activate(h, false);
    show(w);
    unsafe {
        let _ = SetForegroundWindow(h);
    }
    let _ = w.set_focus();
}
