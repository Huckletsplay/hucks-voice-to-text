//! Huck's own clipboard, against the real system clipboard (macOS pasteboards, Windows clipboard).
//!
//! `#[ignore]`d because the borrow test briefly puts text on the user's actual clipboard. Run on
//! purpose: `scripts/dev.sh test -- --ignored` (Windows: `scripts\dev.ps1 test --ignored`).

#![cfg(any(target_os = "macos", windows))]

use hvtt_desktop::clip::huck;

#[test]
#[ignore]
fn huck_clipboard_holds_its_own_text() {
    // His last dictation is on there; the test must leave it exactly as it found it.
    let before = huck::read();
    huck::write("held on Huck's clipboard").unwrap();
    let held = huck::read();
    match before {
        Some(text) => huck::write(&text).unwrap(),
        None => huck::clear(),
    }
    assert_eq!(held.as_deref(), Some("held on Huck's clipboard"));
}

/// Codex's review of 0.1.6: two pastes inside the half second the first has his clipboard on loan
/// used to lose it - the second took the first one's words for "what he had" and gave *those*
/// back. The unit tests prove it on private pasteboards; this is the real one.
#[cfg(target_os = "macos")]
#[test]
#[ignore]
fn two_overlapping_pastes_put_back_his_own_clipboard() {
    let before = huck::snapshot_general().expect("clipboard can be read completely");
    let paste = |words: &'static str, start_after_ms: u64| {
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(start_after_ms));
            let borrowed = huck::borrow_general(words).expect("clipboard refused");
            std::thread::sleep(std::time::Duration::from_millis(150));
            borrowed.give_back()
        })
    };
    let (first, second) = (paste("first paste", 0), paste("second paste", 50));
    let (first, second) = (first.join().unwrap(), second.join().unwrap());
    assert!(first && second, "both pastes gave his clipboard back");
    assert_eq!(
        huck::snapshot_general().expect("restored clipboard can be read completely"),
        before,
        "his own clipboard, not the first paste's words"
    );
}

#[test]
#[ignore]
fn borrowing_the_normal_clipboard_puts_back_exactly_what_was_there() {
    // Whatever he has copied - text, an image, a file - must come back byte for byte.
    let before = huck::snapshot_general().expect("clipboard can be read completely");
    let borrowed = huck::borrow_general("borrowed for a paste").expect("clipboard refused");
    assert_eq!(huck::general_text().as_deref(), Some("borrowed for a paste"));
    assert!(borrowed.give_back(), "the normal clipboard accepted the restore");
    assert_eq!(
        huck::snapshot_general().expect("restored clipboard can be read completely"),
        before,
        "the normal clipboard was not restored"
    );
}
