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
    use parking_lot::{Condvar, Mutex};
    use std::time::{Duration, Instant};

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

    /// One borrow of his clipboard at a time, from the moment it is snapshotted until it has been
    /// given back. Without that, a second paste inside the half second the first has it on loan
    /// snapshots the first one's *temporary words* as "his clipboard": the first then declines to
    /// restore (the clipboard changed under it) and the second gives those words back - his real
    /// clipboard is gone. (Codex's review of 0.1.6.) It can also be *closed*, for Quit: from then
    /// on no borrow starts and one still waiting for its turn gives up, so that Quit sees "nothing
    /// on loan" once and that stays true - otherwise a queued paste could take the turn in the
    /// instant Quit saw the clipboard idle, and the program would leave with it on loan. (Codex's
    /// second review.)
    struct Gate {
        state: Mutex<GateState>,
        changed: Condvar,
    }

    struct GateState {
        /// A borrow is out: taken, and not yet given back.
        busy: bool,
        /// Quit has begun: nothing may borrow.
        closed: bool,
    }

    /// Held by a `Borrowed` until it is given back or dropped. Not a lock guard, because the
    /// give-back happens on another thread than the borrow.
    struct Turn(&'static Gate);

    impl Gate {
        const fn new() -> Self {
            Gate {
                state: Mutex::new(GateState { busy: false, closed: false }),
                changed: Condvar::new(),
            }
        }

        /// Wait, up to `limit`, for the borrow in flight to end, then take the turn. `None` if the
        /// wait ran out, or the gate is - or becomes, while waiting - closed.
        fn take(&'static self, limit: Duration) -> Option<Turn> {
            let deadline = Instant::now() + limit;
            let mut state = self.state.lock();
            loop {
                if state.closed {
                    return None;
                }
                if !state.busy {
                    state.busy = true;
                    return Some(Turn(self));
                }
                if self.changed.wait_until(&mut state, deadline).timed_out() && state.busy {
                    return None;
                }
            }
        }

        /// Quit: close the gate - atomically with looking at it, so nothing slips in - then wait,
        /// up to `limit`, for the borrow on loan to be given back. `true` when none is on loan.
        fn close(&self, limit: Duration) -> bool {
            let deadline = Instant::now() + limit;
            let mut state = self.state.lock();
            state.closed = true;
            // Wake the paste that is queued, so it gives up now rather than at its own deadline.
            self.changed.notify_all();
            while state.busy {
                if self.changed.wait_until(&mut state, deadline).timed_out() {
                    break;
                }
            }
            !state.busy
        }

        /// Quit could not finish: borrowing works again.
        fn reopen(&self) {
            self.state.lock().closed = false;
        }
    }

    impl Drop for Turn {
        fn drop(&mut self) {
            self.0.state.lock().busy = false;
            self.0.changed.notify_all();
        }
    }

    static GATE: Gate = Gate::new();

    /// How long a paste waits for another paste's give-back before it gives up - and leaves his
    /// clipboard alone, the words still on Huck's. A borrow lasts about half a second.
    const TURN_LIMIT: Duration = Duration::from_secs(3);

    /// Everything on the normal clipboard, every item and every type, so an image or a copied
    /// file comes back exactly as it was - not just its text.
    pub struct Borrowed {
        board: String,
        items: Vec<Vec<(String, Vec<u8>)>>,
        ours: isize,
        /// Released when this is given back (or dropped).
        _turn: Option<Turn>,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Snapshot {
        items: Vec<Vec<(String, Vec<u8>)>>,
        change_count: isize,
    }

    /// Every item and type on `board`, as plain bytes. One unreadable promised/private type makes
    /// the whole snapshot fail; silently keeping the other types would make the later restore
    /// destructive. A copy made while the bytes are being read also invalidates the snapshot.
    fn snapshot(board: &NSPasteboard) -> Result<Snapshot, String> {
        let change_count = board.changeCount();
        let mut items = Vec::new();
        if let Some(list) = board.pasteboardItems() {
            for item in list.iter() {
                let mut types = Vec::new();
                for kind in item.types().iter() {
                    let data = item
                        .dataForType(&kind)
                        .ok_or_else(|| format!("the clipboard type {} could not be read", kind))?;
                    types.push((kind.to_string(), data.to_vec()));
                }
                if types.is_empty() {
                    return Err("the clipboard contained an item with no readable types".into());
                }
                items.push(types);
            }
        } else if board
            .types()
            .map(|types| types.iter().next().is_some())
            .unwrap_or(false)
        {
            return Err("the clipboard listed types but its items could not be read".into());
        }
        if board.changeCount() != change_count {
            return Err("the clipboard changed while it was being read".into());
        }
        Ok(Snapshot { items, change_count })
    }

    /// Every item and type on the normal clipboard, as plain bytes.
    pub fn snapshot_general() -> Result<Vec<Vec<(String, Vec<u8>)>>, String> {
        snapshot(&NSPasteboard::generalPasteboard()).map(|snapshot| snapshot.items)
    }

    fn objects(
        items: &[Vec<(String, Vec<u8>)>],
    ) -> Result<Vec<Retained<ProtocolObject<dyn NSPasteboardWriting>>>, String> {
        items
            .iter()
            .map(|types| {
                if types.is_empty() {
                    return Err("the clipboard snapshot contained an empty item".into());
                }
                let item = NSPasteboardItem::new();
                for (kind, bytes) in types {
                    if !item.setData_forType(
                        &NSData::with_bytes(bytes),
                        &NSString::from_str(kind),
                    ) {
                        return Err(format!("the clipboard type {kind} could not be prepared"));
                    }
                }
                Ok(ProtocolObject::from_retained(item))
            })
            .collect()
    }

    fn restore(board: &NSPasteboard, items: &[Vec<(String, Vec<u8>)>]) -> bool {
        let objects = match objects(items) {
            Ok(objects) => objects,
            Err(why) => {
                eprintln!("[hvtt] {why}; the clipboard was left untouched");
                return false;
            }
        };
        board.clearContents();
        items.is_empty() || board.writeObjects(&NSArray::from_retained_slice(&objects))
    }

    fn borrow_snapshot(
        board: &NSPasteboard,
        text: &str,
        snapshot: Snapshot,
    ) -> Result<Borrowed, String> {
        // A copy made after the snapshot wins. Check immediately before the destructive clear;
        // NSPasteboard offers no transaction or lock, so this is the narrowest possible race.
        if board.changeCount() != snapshot.change_count {
            return Err("the clipboard changed before it could be borrowed".into());
        }
        if !put(board, text) {
            let _ = restore(board, &snapshot.items);
            return Err("the normal clipboard refused the borrowed text".into());
        }
        Ok(Borrowed {
            board: board.name().to_string(),
            items: snapshot.items,
            ours: board.changeCount(),
            _turn: None,
        })
    }

    fn borrow(board: &NSPasteboard, text: &str) -> Result<Borrowed, String> {
        let snapshot = snapshot(board)?;
        borrow_snapshot(board, text, snapshot)
    }

    /// `borrow`, but only once any other borrow through `gate` has been given back - and the turn
    /// is kept until this one is.
    fn borrow_in_turn(gate: &'static Gate, board: &NSPasteboard, text: &str) -> Result<Borrowed, String> {
        let turn = gate
            .take(TURN_LIMIT)
            .ok_or("another paste was still using his clipboard, so his clipboard was left alone")?;
        let mut borrowed = borrow(board, text)?;
        borrowed._turn = Some(turn);
        Ok(borrowed)
    }

    /// Put `text` on the normal clipboard for a moment, remembering what was there. A paste into
    /// an app like VS Code can only come from the normal clipboard. One at a time: a second paste
    /// waits (a moment) for the first to be given back.
    pub fn borrow_general(text: &str) -> Result<Borrowed, String> {
        borrow_in_turn(&GATE, &NSPasteboard::generalPasteboard(), text)
    }

    /// Quit, step one: no paste may borrow his clipboard from now on (one waiting for its turn
    /// gives up), and the borrow already on loan is waited for, up to `limit`. Leaving in the half
    /// second a paste has it would leave the pasted words there instead of what he had copied.
    /// `true` when nothing is on loan - and it stays that way, so the program may exit.
    pub fn stop_borrowing(limit: Duration) -> bool {
        GATE.close(limit)
    }

    /// Quit could not finish (a give-back is stuck): pasting borrows his clipboard again.
    pub fn resume_borrowing() {
        GATE.reopen();
    }

    /// What the normal clipboard holds as text, if anything.
    pub fn general_text() -> Option<String> {
        NSPasteboard::generalPasteboard()
            .stringForType(unsafe { NSPasteboardTypeString })
            .map(|s| s.to_string())
    }

    impl Borrowed {
        /// Put his clipboard back - unless he copied something new in the meantime, which wins.
        pub fn give_back(self) -> bool {
            let board = NSPasteboard::pasteboardWithName(&NSString::from_str(&self.board));
            if board.changeCount() != self.ours {
                return false;
            }
            let restored = restore(&board, &self.items);
            if !restored {
                eprintln!("[hvtt] the borrowed clipboard could not be restored completely");
            }
            restored
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn private_board() -> Retained<NSPasteboard> {
            NSPasteboard::pasteboardWithUniqueName()
        }

        fn seed_two_items(board: &NSPasteboard) {
            let first = NSPasteboardItem::new();
            assert!(first.setData_forType(
                &NSData::with_bytes(b"plain"),
                &NSString::from_str("public.utf8-plain-text"),
            ));
            assert!(first.setData_forType(
                &NSData::with_bytes(b"<b>plain</b>"),
                &NSString::from_str("public.html"),
            ));
            let second = NSPasteboardItem::new();
            assert!(second.setData_forType(
                &NSData::with_bytes(b"file:///tmp/example"),
                &NSString::from_str("public.file-url"),
            ));
            let items: Vec<Retained<ProtocolObject<dyn NSPasteboardWriting>>> = vec![
                ProtocolObject::from_retained(first),
                ProtocolObject::from_retained(second),
            ];
            board.clearContents();
            assert!(board.writeObjects(&NSArray::from_retained_slice(&items)));
        }

        #[test]
        fn a_borrow_restores_every_item_and_type() {
            let board = private_board();
            seed_two_items(&board);
            let before = snapshot(&board).unwrap().items;
            let borrowed = borrow(&board, "borrowed").expect("private pasteboard can be borrowed");
            assert_eq!(
                board
                    .stringForType(unsafe { NSPasteboardTypeString })
                    .map(|s| s.to_string())
                    .as_deref(),
                Some("borrowed")
            );
            assert!(borrowed.give_back());
            assert_eq!(snapshot(&board).unwrap().items, before);
        }

        #[test]
        fn a_copy_between_snapshot_and_write_cancels_the_borrow() {
            let board = private_board();
            seed_two_items(&board);
            let stale = snapshot(&board).unwrap();
            assert!(put(&board, "newer copy"));
            let newer = snapshot(&board).unwrap().items;
            let result = borrow_snapshot(&board, "must not replace it", stale);
            assert!(result.is_err());
            assert_eq!(snapshot(&board).unwrap().items, newer);
        }

        #[test]
        fn an_empty_clipboard_can_be_borrowed_and_restored() {
            let board = private_board();
            board.clearContents();
            assert!(snapshot(&board).unwrap().items.is_empty());
            let borrowed = borrow(&board, "borrowed").expect("an empty pasteboard can be borrowed");
            assert!(borrowed.give_back());
            assert!(snapshot(&board).unwrap().items.is_empty());
        }

        #[test]
        fn two_overlapping_pastes_give_back_his_own_clipboard_not_the_first_ones_words() {
            // Codex's review of 0.1.6: the second paste snapshotted the first one's borrowed words
            // as "his clipboard", the first then declined to restore, and the second handed those
            // words back. Two pastes inside the half second the first has it on loan.
            static TEST_GATE: Gate = Gate::new();
            let board = private_board();
            seed_two_items(&board);
            let name = board.name().to_string();
            let before = snapshot(&board).unwrap().items;

            let paste = |text: &'static str, start_after_ms: u64| {
                let name = name.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(start_after_ms));
                    let board = NSPasteboard::pasteboardWithName(&NSString::from_str(&name));
                    let borrowed = borrow_in_turn(&TEST_GATE, &board, text).expect("borrowed");
                    std::thread::sleep(Duration::from_millis(150)); // the app reads it a moment later
                    assert!(borrowed.give_back(), "{text}: his clipboard went back");
                })
            };
            let first = paste("first paste", 0);
            let second = paste("second paste", 50); // inside the first one's loan
            first.join().unwrap();
            second.join().unwrap();
            assert_eq!(snapshot(&board).unwrap().items, before, "his own clipboard, exactly");
        }

        #[test]
        fn a_second_borrow_waits_for_the_first_to_be_given_back() {
            static TEST_GATE: Gate = Gate::new();
            let first = TEST_GATE.take(Duration::from_millis(100)).expect("nothing is borrowed yet");
            assert!(TEST_GATE.take(Duration::from_millis(50)).is_none(), "one at a time");
            let waiting = std::thread::spawn(|| TEST_GATE.take(Duration::from_secs(2)).is_some());
            std::thread::sleep(Duration::from_millis(100));
            drop(first); // given back
            assert!(waiting.join().unwrap(), "it went ahead the moment the first was given back");
        }

        #[test]
        fn quit_waits_for_a_borrow_on_loan_and_does_not_wait_when_there_is_none() {
            static TEST_GATE: Gate = Gate::new();
            assert!(TEST_GATE.close(Duration::from_millis(10)), "nothing on loan: no wait");
            TEST_GATE.reopen();
            let turn = TEST_GATE.take(Duration::from_millis(10)).unwrap();
            let giving_back = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(60));
                drop(turn);
            });
            assert!(TEST_GATE.close(Duration::from_secs(2)), "waited until it was given back");
            giving_back.join().unwrap();
        }

        #[test]
        fn once_quit_has_begun_no_paste_can_start_and_a_queued_one_gives_up() {
            // Codex's second review of 0.1.6: a second paste queued behind the first could take the
            // turn in the instant Quit saw the clipboard idle - and the program left with it on loan.
            static TEST_GATE: Gate = Gate::new();
            let first = TEST_GATE.take(Duration::from_millis(10)).unwrap();
            let queued = std::thread::spawn(|| TEST_GATE.take(Duration::from_secs(2)).is_some());
            std::thread::sleep(Duration::from_millis(50)); // it is waiting for its turn
            let quitting = std::thread::spawn(|| TEST_GATE.close(Duration::from_secs(2)));
            std::thread::sleep(Duration::from_millis(50)); // Quit has begun
            drop(first); // the give-back finishes
            assert!(quitting.join().unwrap(), "Quit went ahead once his clipboard was back");
            assert!(!queued.join().unwrap(), "the queued paste gave up instead of taking the turn");
            assert!(
                TEST_GATE.take(Duration::from_millis(10)).is_none(),
                "nothing can borrow after Quit saw the clipboard idle"
            );
        }

        #[test]
        fn a_quit_that_cannot_finish_does_not_go_ahead_and_leaves_pasting_working() {
            static TEST_GATE: Gate = Gate::new();
            let turn = TEST_GATE.take(Duration::from_millis(10)).unwrap();
            assert!(!TEST_GATE.close(Duration::from_millis(30)), "still on loan: says so, so no exit");
            assert!(TEST_GATE.take(Duration::from_millis(10)).is_none(), "closed while quitting");
            TEST_GATE.reopen();
            drop(turn);
            assert!(TEST_GATE.take(Duration::from_millis(50)).is_some(), "open again after a failed Quit");
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

    /// Borrows not yet given back. The program must not quit while one is out: his words would
    /// stay on the normal clipboard and what he had there would be gone. (Codex's review of 0.1.5,
    /// 2026-09-29: the update used to guess this from the dictation state.)
    static PENDING: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    /// Whether any borrow of the normal clipboard is still out.
    pub fn borrows_pending() -> bool {
        PENDING.load(std::sync::atomic::Ordering::SeqCst) > 0
    }

    /// Counts one borrow for as long as it lives: from before the clipboard is touched until it
    /// has been given back (or refused).
    struct Pending;

    impl Pending {
        fn new() -> Self {
            PENDING.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Pending
        }
    }

    impl Drop for Pending {
        fn drop(&mut self) {
            PENDING.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// Everything that was on the normal clipboard before a borrow.
    pub struct Borrowed {
        items: Vec<(u32, Vec<u8>)>,
        ours: u32,
        _pending: Pending,
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
        let pending = Pending::new();
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
        Ok(Borrowed { items, ours: unsafe { GetClipboardSequenceNumber() }, _pending: pending })
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
        fn a_borrow_counts_until_it_is_dropped() {
            // Other tests may borrow in parallel, so this compares against its own start.
            let before = PENDING.load(std::sync::atomic::Ordering::SeqCst);
            let one = Pending::new();
            assert_eq!(PENDING.load(std::sync::atomic::Ordering::SeqCst), before + 1);
            assert!(borrows_pending());
            drop(one);
            assert_eq!(PENDING.load(std::sync::atomic::Ordering::SeqCst), before);
        }

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
