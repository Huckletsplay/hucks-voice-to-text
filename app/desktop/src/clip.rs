//! The system clipboard, behind core's trait.
//!
//! This is the safety net the whole product rests on: if delivery fails, the words are still
//! one paste away.

use hvtt_core::pipeline::Clipboard;
use tauri::AppHandle;
use tauri_plugin_clipboard_manager::ClipboardExt;

pub struct SystemClipboard {
    app: AppHandle,
}

impl SystemClipboard {
    pub fn new(app: AppHandle) -> Self {
        SystemClipboard { app }
    }
}

impl Clipboard for SystemClipboard {
    fn set_text(&self, text: &str) -> Result<(), String> {
        self.app
            .clipboard()
            .write_text(text.to_string())
            .map_err(|e| e.to_string())
    }
}

/// Huck's own clipboard: a named macOS pasteboard. (Windows: see the second `huck` below.)
///
/// Not a new program or a background task - a private, named slot in the clipboard service the
/// Mac already runs. Written once per dictation, read when he presses the paste shortcut, and
/// idle otherwise. The name is shared on purpose, so Huck's other programs can use the same one.
#[cfg(target_os = "macos")]
pub mod huck {
    use objc2::rc::Retained;
    use objc2::runtime::ProtocolObject;
    use objc2_app_kit::{NSPasteboard, NSPasteboardItem, NSPasteboardTypeString, NSPasteboardWriting};
    use objc2_foundation::{NSArray, NSData, NSString};

    pub const NAME: &str = "com.huck.clipboard";

    fn board() -> Retained<NSPasteboard> {
        NSPasteboard::pasteboardWithName(&NSString::from_str(NAME))
    }

    fn put(board: &NSPasteboard, text: &str) -> bool {
        board.clearContents();
        board.setString_forType(&NSString::from_str(text), unsafe { NSPasteboardTypeString })
    }

    pub fn write(text: &str) -> Result<(), String> {
        put(&board(), text).then_some(()).ok_or_else(|| "Huck's clipboard refused the text".into())
    }

    pub fn clear() {
        board().clearContents();
    }

    pub fn read() -> Option<String> {
        board().stringForType(unsafe { NSPasteboardTypeString }).map(|s| s.to_string())
    }

    /// The pipeline's safety-net copy, sent to Huck's clipboard instead of the normal one.
    pub struct HuckClipboard;

    impl hvtt_core::pipeline::Clipboard for HuckClipboard {
        fn set_text(&self, text: &str) -> Result<(), String> {
            write(text)
        }
    }

    /// Everything on the normal clipboard, every item and every type, so an image or a copied
    /// file comes back exactly as it was - not just its text.
    pub struct Borrowed {
        items: Vec<Vec<(String, Vec<u8>)>>,
        ours: isize,
    }

    /// Every item and type on the normal clipboard, as plain bytes.
    pub fn snapshot_general() -> Vec<Vec<(String, Vec<u8>)>> {
        NSPasteboard::generalPasteboard()
            .pasteboardItems()
            .map(|list| {
                list.iter()
                    .map(|item| {
                        item.types()
                            .iter()
                            .filter_map(|t| {
                                let data = item.dataForType(&t)?;
                                Some((t.to_string(), data.to_vec()))
                            })
                            .collect()
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Put `text` on the normal clipboard for a moment, remembering what was there. A paste into
    /// an app like VS Code can only come from the normal clipboard.
    pub fn borrow_general(text: &str) -> Option<Borrowed> {
        let items = snapshot_general();
        let general = NSPasteboard::generalPasteboard();
        put(&general, text).then(|| Borrowed { items, ours: general.changeCount() })
    }

    /// What the normal clipboard holds as text, if anything.
    pub fn general_text() -> Option<String> {
        NSPasteboard::generalPasteboard()
            .stringForType(unsafe { NSPasteboardTypeString })
            .map(|s| s.to_string())
    }

    impl Borrowed {
        /// Put his clipboard back - unless he copied something new in the meantime, which wins.
        pub fn give_back(self) {
            let general = NSPasteboard::generalPasteboard();
            if general.changeCount() != self.ours {
                return;
            }
            general.clearContents();
            if self.items.is_empty() {
                return;
            }
            let items: Vec<Retained<ProtocolObject<dyn NSPasteboardWriting>>> = self
                .items
                .iter()
                .map(|types| {
                    let item = NSPasteboardItem::new();
                    for (t, bytes) in types {
                        item.setData_forType(&NSData::with_bytes(bytes), &NSString::from_str(t));
                    }
                    ProtocolObject::from_retained(item)
                })
                .collect();
            general.writeObjects(&NSArray::from_retained_slice(&items));
        }
    }
}

/// Huck's own clipboard on Windows.
///
/// Windows has no named clipboards, so this one lives in the running program's memory - which is
/// always there, because the program stays resident for the hotkey. Deliberately not a file: with
/// Keep Recovery Drafts off he has asked for no dictation to touch the disk. The same functions as
/// the macOS module, so the rest of the app does not care which it is talking to.
#[cfg(windows)]
pub mod huck {
    use parking_lot::Mutex;
    use windows::core::w;
    use windows::Win32::Foundation::{GetLastError, SetLastError, HANDLE, HGLOBAL, HWND, WIN32_ERROR};
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, HWND_MESSAGE, WINDOW_EX_STYLE, WINDOW_STYLE,
    };
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, EnumClipboardFormats, GetClipboardData,
        GetClipboardSequenceNumber, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
    };
    use windows::Win32::Graphics::Gdi::{GetEnhMetaFileBits, SetEnhMetaFileBits, HENHMETAFILE};
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE};

    static HELD: Mutex<Option<String>> = Mutex::new(None);

    const CF_UNICODETEXT: u32 = 13;

    pub fn write(text: &str) -> Result<(), String> {
        *HELD.lock() = Some(text.to_string());
        Ok(())
    }

    pub fn clear() {
        *HELD.lock() = None;
    }

    pub fn read() -> Option<String> {
        HELD.lock().clone()
    }

    /// The pipeline's safety-net copy, sent to Huck's clipboard instead of the normal one.
    pub struct HuckClipboard;

    impl hvtt_core::pipeline::Clipboard for HuckClipboard {
        fn set_text(&self, text: &str) -> Result<(), String> {
            write(text)
        }
    }

    /// The Windows clipboard, open for as long as this lives. Another program may hold it for a
    /// moment, so opening retries briefly rather than failing the paste.
    ///
    /// Opened from a hidden window of our own, made on this thread for the purpose: a clipboard
    /// opened with no window locks nobody out (measured 2026-09-26), so another program could
    /// change it between reading it and replacing it. With a window, others are refused until it
    /// closes.
    struct Open {
        owner: HWND,
    }

    impl Open {
        fn new() -> Option<Open> {
            let owner = unsafe {
                CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    w!("STATIC"),
                    None,
                    WINDOW_STYLE(0),
                    0,
                    0,
                    0,
                    0,
                    Some(HWND_MESSAGE),
                    None,
                    None,
                    None,
                )
            }
            .ok()?;
            for _ in 0..20 {
                if unsafe { OpenClipboard(Some(owner)) }.is_ok() {
                    return Some(Open { owner });
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            let _ = unsafe { DestroyWindow(owner) };
            None
        }
    }

    impl Drop for Open {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseClipboard();
                let _ = DestroyWindow(self.owner);
            }
        }
    }

    const CF_BITMAP: u32 = 2;
    const CF_METAFILEPICT: u32 = 3;
    const CF_DIB: u32 = 8;
    const CF_PALETTE: u32 = 9;
    const CF_ENHMETAFILE: u32 = 14;
    const CF_DIBV5: u32 = 17;

    /// How one clipboard format can be carried across a borrow.
    #[derive(Debug, PartialEq, Eq)]
    enum Carry {
        /// Plain memory, copied byte for byte.
        Memory,
        /// An enhanced metafile: a GDI handle, carried as its bytes.
        Metafile,
        /// A GDI handle Windows makes again by itself from another format being kept (a bitmap
        /// from its DIB, a metafile picture from its enhanced metafile).
        Rebuilt,
        /// A handle that cannot be copied at all - owner-drawn or private GDI data. Its presence
        /// stops the borrow before anything is touched.
        Impossible,
    }

    fn carry(format: u32, present: &[u32]) -> Carry {
        let has = |f: u32| present.contains(&f);
        match format {
            CF_ENHMETAFILE => Carry::Metafile,
            CF_BITMAP | CF_PALETTE if has(CF_DIB) || has(CF_DIBV5) => Carry::Rebuilt,
            CF_METAFILEPICT if has(CF_ENHMETAFILE) => Carry::Rebuilt,
            CF_BITMAP | CF_PALETTE | CF_METAFILEPICT => Carry::Impossible,
            0x80 | 0x82 | 0x83 | 0x8E | 0x300..=0x3FF => Carry::Impossible,
            _ => Carry::Memory,
        }
    }

    fn global_from(bytes: &[u8]) -> Option<HGLOBAL> {
        unsafe {
            let memory = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1)).ok()?;
            let target = GlobalLock(memory) as *mut u8;
            if target.is_null() {
                return None;
            }
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), target, bytes.len());
            let _ = GlobalUnlock(memory);
            Some(memory)
        }
    }

    /// Put one format on the (open, emptied) clipboard. True only if Windows took it.
    fn put(format: u32, bytes: &[u8]) -> bool {
        unsafe {
            if format == CF_ENHMETAFILE {
                let metafile = SetEnhMetaFileBits(bytes);
                return !metafile.is_invalid()
                    && SetClipboardData(format, Some(HANDLE(metafile.0))).is_ok();
            }
            match global_from(bytes) {
                // Once set, the memory belongs to the clipboard.
                Some(memory) => SetClipboardData(format, Some(HANDLE(memory.0))).is_ok(),
                None => false,
            }
        }
    }

    fn utf16z(text: &str) -> Vec<u8> {
        text.encode_utf16().chain(std::iter::once(0)).flat_map(u16::to_le_bytes).collect()
    }

    fn memory_bytes(format: u32) -> Result<Vec<u8>, String> {
        let unreadable = || format!("clipboard format {format:#x} could not be read");
        unsafe {
            let memory = HGLOBAL(GetClipboardData(format).map_err(|_| unreadable())?.0);
            let size = GlobalSize(memory);
            if size == 0 {
                return Ok(Vec::new());
            }
            let source = GlobalLock(memory) as *const u8;
            if source.is_null() {
                return Err(unreadable());
            }
            let bytes = std::slice::from_raw_parts(source, size).to_vec();
            let _ = GlobalUnlock(memory);
            Ok(bytes)
        }
    }

    fn metafile_bytes() -> Result<Vec<u8>, String> {
        let unreadable = || "the copied picture could not be read".to_string();
        unsafe {
            let metafile = HENHMETAFILE(GetClipboardData(CF_ENHMETAFILE).map_err(|_| unreadable())?.0);
            let size = GetEnhMetaFileBits(metafile, None);
            if size == 0 {
                return Err(unreadable());
            }
            let mut bytes = vec![0u8; size as usize];
            if GetEnhMetaFileBits(metafile, Some(&mut bytes)) != size {
                return Err(unreadable());
            }
            Ok(bytes)
        }
    }

    /// Everything on the normal clipboard, every format, as bytes - so an image or a copied file
    /// comes back exactly as it was, not just its text.
    ///
    /// All or nothing. A busy clipboard, an unreadable format, or one that cannot be copied is an
    /// error, and then the clipboard is not borrowed at all (Codex's review, 2026-09-26: the first
    /// version returned whatever it could, and a borrow could then wipe the rest).
    pub fn snapshot_general() -> Result<Vec<(u32, Vec<u8>)>, String> {
        let _open = Open::new().ok_or("the clipboard is busy")?;
        read_all()
    }

    /// Every format on the clipboard, which the caller has open.
    fn read_all() -> Result<Vec<(u32, Vec<u8>)>, String> {
        let mut formats = Vec::new();
        let mut format = 0u32;
        loop {
            unsafe { SetLastError(WIN32_ERROR(0)) };
            format = unsafe { EnumClipboardFormats(format) };
            if format == 0 {
                // The end of the list, or a failure part-way through - which is not a snapshot.
                if unsafe { GetLastError() } != WIN32_ERROR(0) {
                    return Err("the clipboard's formats could not be listed".into());
                }
                break;
            }
            formats.push(format);
        }
        let mut items = Vec::new();
        for &format in &formats {
            match carry(format, &formats) {
                Carry::Rebuilt => {}
                Carry::Impossible => {
                    return Err(format!("clipboard format {format:#x} cannot be copied"))
                }
                Carry::Metafile => items.push((format, metafile_bytes()?)),
                Carry::Memory => items.push((format, memory_bytes(format)?)),
            }
        }
        Ok(items)
    }

    /// Put a snapshot back onto the open, emptied clipboard. True only if every format went back.
    fn restore(items: &[(u32, Vec<u8>)]) -> bool {
        items.iter().fold(true, |ok, (format, bytes)| put(*format, bytes) && ok)
    }

    /// What the normal clipboard holds as text, if anything.
    pub fn general_text() -> Option<String> {
        let _open = Open::new()?;
        unsafe {
            let memory = HGLOBAL(GetClipboardData(CF_UNICODETEXT).ok()?.0);
            let source = GlobalLock(memory) as *const u16;
            if source.is_null() {
                return None;
            }
            let units = GlobalSize(memory) / 2;
            let slice = std::slice::from_raw_parts(source, units);
            let end = slice.iter().position(|&u| u == 0).unwrap_or(units);
            let text = String::from_utf16_lossy(&slice[..end]);
            let _ = GlobalUnlock(memory);
            Some(text)
        }
    }

    /// Everything that was on the normal clipboard before a borrow.
    pub struct Borrowed {
        items: Vec<(u32, Vec<u8>)>,
        ours: u32,
    }

    /// Put `text` on the normal clipboard for a moment, remembering exactly what was there. A
    /// paste into an app like VS Code can only come from the normal clipboard.
    ///
    /// Refused - with the clipboard untouched - unless all of it could be remembered. Marked so
    /// Windows' clipboard history (Win+V) does not keep the borrowed copy: it is his words on loan
    /// for half a second, not something he copied.
    ///
    /// Read, emptied and replaced under one open (Codex's second review, 2026-09-26: between two
    /// opens another program could copy something new, which the borrow then replaced and never
    /// put back).
    pub fn borrow_general(text: &str) -> Result<Borrowed, String> {
        let items;
        {
            let _open = Open::new().ok_or("the clipboard is busy")?;
            items = read_all()?;
            unsafe { EmptyClipboard() }.map_err(|_| "the clipboard could not be emptied")?;
            if !put(CF_UNICODETEXT, &utf16z(text)) {
                // Put his things straight back rather than leave the clipboard empty.
                restore(&items);
                return Err("the clipboard refused the text".into());
            }
            let private = unsafe { RegisterClipboardFormatW(w!("ExcludeClipboardContentFromMonitorProcessing")) };
            if private != 0 {
                let _ = put(private, &[0]);
            }
        }
        // Read only once the clipboard is closed: closing it is itself a change Windows counts.
        Ok(Borrowed { items, ours: unsafe { GetClipboardSequenceNumber() } })
    }

    impl Borrowed {
        /// Put his clipboard back - unless he copied something new in the meantime, which wins.
        /// True when his clipboard is as it was (or newer); false if any of it could not go back.
        pub fn give_back(self) -> bool {
            if unsafe { GetClipboardSequenceNumber() } != self.ours {
                return true;
            }
            let Some(_open) = Open::new() else {
                eprintln!("[hvtt] the clipboard stayed busy; the borrowed words are still on it");
                return false;
            };
            if unsafe { EmptyClipboard() }.is_err() {
                return false;
            }
            let ok = restore(&self.items);
            if !ok {
                eprintln!("[hvtt] part of the clipboard could not be put back");
            }
            ok
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn handles_that_cannot_be_copied_stop_the_borrow() {
            assert_eq!(carry(0x80, &[0x80]), Carry::Impossible, "owner-drawn");
            assert_eq!(carry(0x300, &[0x300]), Carry::Impossible, "private GDI");
            assert_eq!(carry(CF_BITMAP, &[CF_BITMAP]), Carry::Impossible, "a bitmap with no DIB");
            assert_eq!(carry(CF_METAFILEPICT, &[CF_METAFILEPICT]), Carry::Impossible);
        }

        #[test]
        fn pictures_travel_as_something_windows_can_rebuild() {
            assert_eq!(carry(CF_BITMAP, &[CF_BITMAP, CF_DIB]), Carry::Rebuilt);
            assert_eq!(carry(CF_METAFILEPICT, &[CF_METAFILEPICT, CF_ENHMETAFILE]), Carry::Rebuilt);
            assert_eq!(carry(CF_ENHMETAFILE, &[CF_ENHMETAFILE]), Carry::Metafile);
            for format in [1, CF_DIB, 13, 15, CF_DIBV5, 0xC0FF] {
                assert_eq!(carry(format, &[format]), Carry::Memory, "{format:#x}");
            }
        }

        #[test]
        fn text_goes_on_as_nul_terminated_utf16() {
            assert_eq!(utf16z("Hi"), vec![b'H', 0, b'i', 0, 0, 0]);
        }

        const CF_HDROP: u32 = 15;

        /// A DROPFILES list naming one file, as Explorer puts it on the clipboard.
        fn copied_file(path: &str) -> Vec<u8> {
            let mut bytes = Vec::new();
            for field in [20u32, 0, 0, 0, 1] {
                bytes.extend(field.to_le_bytes()); // pFiles, pt.x, pt.y, fNC, fWide
            }
            bytes.extend(path.encode_utf16().chain([0, 0]).flat_map(u16::to_le_bytes));
            bytes
        }

        /// A 2 x 2, 32-bit DIB.
        fn picture() -> Vec<u8> {
            let mut bytes = Vec::new();
            bytes.extend(40u32.to_le_bytes()); // biSize
            bytes.extend(2i32.to_le_bytes()); // biWidth
            bytes.extend(2i32.to_le_bytes()); // biHeight
            bytes.extend(1u16.to_le_bytes()); // biPlanes
            bytes.extend(32u16.to_le_bytes()); // biBitCount
            bytes.extend([0u8; 24]); // compression, size, resolution, colours
            bytes.extend([0x10, 0x20, 0x30, 0xFF].repeat(4));
            bytes
        }

        fn sorted(mut items: Vec<(u32, Vec<u8>)>) -> Vec<(u32, Vec<u8>)> {
            items.sort();
            items
        }

        /// Borrows his real clipboard and puts it back. Run on purpose:
        /// `scripts\dev.ps1 test --ignored clip::huck`.
        #[test]
        #[ignore]
        fn every_kind_of_copy_comes_back_from_a_borrow() {
            let his = snapshot_general().expect("his clipboard can be read");
            let html = unsafe { RegisterClipboardFormatW(w!("HTML Format")) };
            {
                let _open = Open::new().expect("clipboard");
                unsafe { EmptyClipboard() }.expect("emptied");
                assert!(put(CF_UNICODETEXT, &utf16z("plain words")));
                assert!(put(html, b"Version:0.9\r\n<b>bold words</b>\0"));
                assert!(put(CF_HDROP, &copied_file(r"C:\Windows\win.ini")));
                assert!(put(CF_DIB, &picture()));
            }
            let before = snapshot_general().expect("text, HTML, a file and a picture read back");
            let borrowed = borrow_general("borrowed for a paste").expect("borrowed");
            let during = general_text();
            let back = borrowed.give_back();
            let after = snapshot_general().expect("read after");
            {
                let _open = Open::new().expect("clipboard");
                unsafe { EmptyClipboard() }.expect("emptied");
                assert!(restore(&his), "his own clipboard went back");
            }
            assert_eq!(during.as_deref(), Some("borrowed for a paste"));
            assert!(back, "every format went back");
            for format in [CF_UNICODETEXT, html, CF_HDROP, CF_DIB] {
                assert!(after.iter().any(|(f, _)| *f == format), "{format:#x} came back");
            }
            assert_eq!(sorted(after), sorted(before), "byte for byte");
        }

        /// While Huck has the clipboard open, another program cannot open it - so nothing can
        /// change between reading it and replacing it.
        #[test]
        #[ignore]
        fn while_huck_holds_the_clipboard_nobody_else_can_open_it() {
            let result = std::env::temp_dir().join(format!("hvtt-clip-try-{}", std::process::id()));
            let _ = std::fs::remove_file(&result);
            let script = format!(
                "Add-Type -Name C -Namespace H -MemberDefinition '[DllImport(\"user32.dll\")] public static extern bool OpenClipboard(IntPtr h); [DllImport(\"user32.dll\")] public static extern bool CloseClipboard();'; \
                 $ok = [H.C]::OpenClipboard([IntPtr]::Zero); if ($ok) {{ [void][H.C]::CloseClipboard() }}; Set-Content -Path '{}' -Value $ok",
                result.display()
            );
            let other = {
                let _open = Open::new().expect("Huck opens the clipboard");
                std::process::Command::new("powershell")
                    .args(["-NoProfile", "-NonInteractive", "-Command", &script])
                    .status()
                    .expect("a second program runs");
                std::fs::read_to_string(&result).unwrap_or_default()
            };
            let _ = std::fs::remove_file(&result);
            assert_eq!(other.trim(), "False", "another program opened the clipboard Huck was holding");
        }

        /// Another program holds the clipboard for a few seconds, the way programs do: from its own
        /// window. Measured 2026-09-26: a clipboard opened with *no* owner window locks nobody
        /// out, in this process or any other, so a holder without a window would prove nothing.
        #[test]
        #[ignore]
        fn a_busy_clipboard_is_never_borrowed() {
            let before = snapshot_general().expect("his clipboard can be read");
            let marker = std::env::temp_dir().join(format!("hvtt-clip-held-{}", std::process::id()));
            let _ = std::fs::remove_file(&marker);
            let script = format!(
                "Add-Type -AssemblyName System.Windows.Forms; \
                 Add-Type -Name C -Namespace H -MemberDefinition '[DllImport(\"user32.dll\")] public static extern bool OpenClipboard(IntPtr h); [DllImport(\"user32.dll\")] public static extern bool CloseClipboard();'; \
                 $f = New-Object System.Windows.Forms.Form; \
                 if ([H.C]::OpenClipboard($f.Handle)) {{ New-Item -ItemType File -Path '{}' | Out-Null; Start-Sleep -Milliseconds 3000; [void][H.C]::CloseClipboard() }}",
                marker.display()
            );
            let mut holder = std::process::Command::new("powershell")
                .args(["-NoProfile", "-NonInteractive", "-Command", &script])
                .spawn()
                .expect("a second program starts");
            let started = std::time::Instant::now();
            while !marker.exists() && started.elapsed() < std::time::Duration::from_secs(20) {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            let held = marker.exists();
            let tried = if held { Some(borrow_general("must land nowhere")) } else { None };
            let _ = holder.wait();
            let _ = std::fs::remove_file(&marker);
            // Whatever happened, his clipboard goes back before anything is judged.
            let mut leaked = false;
            if let Some(Ok(borrowed)) = tried {
                leaked = true;
                borrowed.give_back();
            }
            assert!(held, "the other program never got hold of the clipboard");
            assert!(!leaked, "a busy clipboard was borrowed");
            assert_eq!(snapshot_general().expect("read after"), before, "and nothing changed");
        }
    }
}
