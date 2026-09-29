//! The Windows half of the field-capture spike, against a real Notepad.
//!
//! The question the product rests on, asked of UI Automation: does a pinned text field survive
//! the user going somewhere else, take a silent write without touching the foreground, and get
//! refused once it is gone? macOS answered it in `spikes/field-capture/mac/`; this is Windows.
//!
//! `#[ignore]`d because it opens Notepad and moves the foreground for a second or two. Run on
//! purpose: `scripts\dev.ps1 test --ignored windows_delivery`.

#![cfg(windows)]

use hvtt_core::pipeline::Destination;
use hvtt_desktop::destination::{windows_paste::FocusStamp, windows_uia};
use std::process::Command;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, EnumWindows, FindWindowExW, GetForegroundWindow, GetWindowThreadProcessId,
    IsWindowVisible, SendMessageW, SetForegroundWindow, WM_GETTEXT,
};

/// The visible top-level window of a process, once it has one.
fn window_of(pid: u32) -> Option<HWND> {
    unsafe extern "system" fn each(hwnd: HWND, found: LPARAM) -> windows::core::BOOL {
        let found = &mut *(found.0 as *mut (u32, isize));
        let mut owner = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut owner));
        if owner == found.0 && IsWindowVisible(hwnd).as_bool() {
            found.1 = hwnd.0 as isize;
            return false.into();
        }
        true.into()
    }
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(10) {
        let mut found = (pid, 0isize);
        unsafe {
            let _ = EnumWindows(Some(each), LPARAM(&mut found as *mut _ as isize));
        }
        if found.1 != 0 {
            return Some(HWND(found.1 as *mut _));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

/// Put `hwnd` in front the way a click would. Windows only lets the foreground process do that,
/// so this borrows the foreground thread's input state for the moment.
fn bring_forward(hwnd: HWND) {
    unsafe {
        let front = GetForegroundWindow();
        let front_thread = GetWindowThreadProcessId(front, None);
        let me = GetCurrentThreadId();
        let _ = AttachThreadInput(me, front_thread, true);
        let _ = SetForegroundWindow(hwnd);
        let _ = BringWindowToTop(hwnd);
        let _ = AttachThreadInput(me, front_thread, false);
    }
    std::thread::sleep(Duration::from_millis(300));
}

fn text_of(edit: HWND) -> String {
    let mut buf = vec![0u16; 4096];
    let n = unsafe {
        SendMessageW(edit, WM_GETTEXT, Some(WPARAM(buf.len())), Some(LPARAM(buf.as_mut_ptr() as isize)))
    };
    String::from_utf16_lossy(&buf[..n.0 as usize])
}

#[test]
#[ignore]
fn windows_delivery_survives_leaving_notepad_and_is_refused_once_it_closes() {
    let previous = unsafe { GetForegroundWindow() };
    let mut notepad = Command::new("notepad.exe").spawn().expect("Notepad starts");
    let result = std::panic::catch_unwind(|| {
        let window = window_of(notepad.id()).expect("Notepad shows a window");
        bring_forward(window);

        // 1. The keypress: what is in front, then UI Automation's focused element.
        let stamp = FocusStamp::capture().expect("Notepad is in front");
        assert_eq!(stamp.pid(), notepad.id(), "the stamp names Notepad");
        let started = Instant::now();
        let element = windows_uia::capture(&stamp).expect("the text box is captured");
        let dest = windows_uia::validate_captured(element, "Notepad".into()).expect("it is a text field");
        eprintln!("captured {:?} in {} ms", dest.label(), started.elapsed().as_millis());

        // 2. He goes back to what he was reading.
        bring_forward(previous);
        assert_ne!(unsafe { GetForegroundWindow() }, window, "Notepad is no longer in front");
        assert!(stamp.moved().is_some(), "the paste gate sees that focus left");
        assert!(dest.is_alive().is_alive(), "the pinned text box survives focus leaving");

        // 3. Delivery: silent, into Notepad, with the other window still in front.
        let started = Instant::now();
        dest.deliver("Huck was here.").expect("the silent write lands and reads back");
        eprintln!("delivered in {} ms", started.elapsed().as_millis());
        let edit = unsafe { FindWindowExW(Some(window), None, windows::core::w!("Edit"), None) }
            .expect("Notepad's text box");
        assert_eq!(text_of(edit), "Huck was here.");
        assert_ne!(unsafe { GetForegroundWindow() }, window, "delivery never took the foreground");
    });

    // 4. Notepad closes; the pin must die with it. (Killed, so it cannot ask to save.)
    let _ = notepad.kill();
    let _ = notepad.wait();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

/// Is `thread`'s window in menu mode - its menu bar or system menu active?
fn in_menu_mode(window: HWND) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{GetGUIThreadInfo, GUITHREADINFO, GUI_INMENUMODE, GUI_SYSTEMMENUMODE};
    let thread = unsafe { GetWindowThreadProcessId(window, None) };
    let mut info = GUITHREADINFO { cbSize: std::mem::size_of::<GUITHREADINFO>() as u32, ..Default::default() };
    unsafe { GetGUIThreadInfo(thread, &mut info) }.is_ok()
        && (info.flags.contains(GUI_INMENUMODE) || info.flags.contains(GUI_SYSTEMMENUMODE))
}

fn press(vk: u16, up: bool) {
    use windows::Win32::UI::Input::KeyboardAndMouse::{keybd_event, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP};
    unsafe { keybd_event(vk as u8, 0, if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) }, 0) };
}

#[test]
#[ignore]
fn windows_delivery_paste_lands_at_once_with_ctrl_alt_still_held() {
    // Huck's Clipboard's Ctrl+Alt+V: the paste goes out while his fingers are still on Ctrl and
    // Alt - it must arrive as Ctrl+V, and letting go of Alt afterwards must not open the menu.
    use hvtt_desktop::destination::windows_paste::paste_borrowing_clipboard;
    let previous = unsafe { GetForegroundWindow() };
    let mut notepad = Command::new("notepad.exe").spawn().expect("Notepad starts");
    let before = hvtt_desktop::clip::huck::snapshot_general();
    let result = std::panic::catch_unwind(|| {
        let window = window_of(notepad.id()).expect("Notepad shows a window");
        bring_forward(window);
        let edit = unsafe { FindWindowExW(Some(window), None, windows::core::w!("Edit"), None) }
            .expect("Notepad's text box");

        press(0xA2, false); // Ctrl down, as if still held from the shortcut
        press(0xA4, false); // Alt down
        let started = Instant::now();
        paste_borrowing_clipboard("pasted at once").expect("the paste goes out");
        let sent_ms = started.elapsed().as_millis();
        std::thread::sleep(Duration::from_millis(300));
        let landed = text_of(edit);
        press(0xA4, true); // he lets go
        press(0xA2, true);
        std::thread::sleep(Duration::from_millis(300));

        eprintln!("paste sent in {sent_ms} ms with Ctrl+Alt held; Notepad holds {landed:?}");
        assert_eq!(landed, "pasted at once", "the words arrive while the keys are still down");
        assert!(sent_ms < 100, "no waiting for the keys to lift ({sent_ms} ms)");
        assert!(!in_menu_mode(window), "letting go of Alt did not open Notepad's menu");
    });
    press(0xA4, true);
    press(0xA2, true);
    let _ = notepad.kill();
    let _ = notepad.wait();
    bring_forward(previous);
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(hvtt_desktop::clip::huck::snapshot_general(), before, "his clipboard came back");
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[test]
#[ignore]
fn windows_delivery_is_refused_after_the_window_closes() {
    let previous = unsafe { GetForegroundWindow() };
    let mut notepad = Command::new("notepad.exe").spawn().expect("Notepad starts");
    let window = window_of(notepad.id()).expect("Notepad shows a window");
    bring_forward(window);
    let stamp = FocusStamp::capture().expect("Notepad is in front");
    let dest = windows_uia::capture(&stamp)
        .and_then(|e| windows_uia::validate_captured(e, "Notepad".into()))
        .expect("the text box is captured");
    bring_forward(previous);

    let _ = notepad.kill();
    let _ = notepad.wait();
    std::thread::sleep(Duration::from_millis(300));

    assert!(!dest.is_alive().is_alive(), "a closed window's text box is dead");
    assert!(dest.deliver("must land nowhere").is_err(), "and nothing is written anywhere");
}
