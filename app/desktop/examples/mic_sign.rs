//! Does a microphone that is prepared but not started count as "in use"? And how much sooner
//! does a prepared one give its first sound?
//!
//!   cargo run --release -p hvtt-desktop --example mic_sign      (through scripts/dev.sh or dev.ps1's
//!                                                                environment)
//!
//! The program prepares the next dictation's microphone ahead of the keypress
//! (`recorder::Prepared`), so the press only has to start it. That is only acceptable if a prepared
//! microphone does not light the system's "microphone in use" sign between dictations. On macOS
//! that was checked by eye (the orange dot, 2026-10-02). On Windows this asks Windows itself: the
//! privacy record it keeps for every program that opens the microphone
//! (`CapabilityAccessManager\ConsentStore\microphone`), which is what the notification-area
//! microphone sign and Settings › Privacy › Microphone are drawn from - a use that has started and
//! not stopped is "in use".
//!
//! Opens the microphone for a few seconds. Nothing it hears is kept.

use hvtt_desktop::recorder::{Prepared, Recording};
use std::time::{Duration, Instant};

/// Windows' own record for this program: `Some(true)` while it counts the microphone in use,
/// `None` when it has no record at all (the microphone was never opened by it).
#[cfg(windows)]
fn in_use() -> Option<bool> {
    use windows::core::HSTRING;
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_QWORD};
    let exe = std::env::current_exe().ok()?.display().to_string().replace('\\', "#");
    let key = HSTRING::from(format!(
        "Software\\Microsoft\\Windows\\CurrentVersion\\CapabilityAccessManager\\ConsentStore\\microphone\\NonPackaged\\{exe}"
    ));
    let read = |name: &str| -> Option<u64> {
        let (mut value, mut size) = (0u64, std::mem::size_of::<u64>() as u32);
        unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                &key,
                &HSTRING::from(name),
                RRF_RT_REG_QWORD,
                None,
                Some(&mut value as *mut u64 as *mut _),
                Some(&mut size),
            )
        }
        .is_ok()
        .then_some(value)
    };
    let started = read("LastUsedTimeStart")?;
    Some(started != 0 && read("LastUsedTimeStop")? == 0)
}

#[cfg(not(windows))]
fn in_use() -> Option<bool> {
    None
}

fn say(what: &str) {
    let sign = match in_use() {
        Some(true) => "IN USE",
        Some(false) => "not in use",
        None => "no record",
    };
    println!("{what:<58} microphone sign: {sign}");
}

/// How long from asking for sound to the first of it.
fn first_sound(rec: &Recording, asked: Instant) -> String {
    while rec.first_sound().is_none() && asked.elapsed() < Duration::from_secs(3) {
        std::thread::sleep(Duration::from_millis(1));
    }
    match rec.first_sound() {
        Some(at) => format!("{} ms", at.saturating_duration_since(asked).as_millis()),
        None => "none within 3 s".into(),
    }
}

fn main() {
    say("before anything");
    let prepared = Prepared::new(None).expect("a microphone");
    std::thread::sleep(Duration::from_millis(500));
    say("prepared, not started (0.5 s)");
    std::thread::sleep(Duration::from_millis(3500));
    say("prepared, not started (4 s)");

    let asked = Instant::now();
    let rec = prepared.start().expect("it starts");
    let took = first_sound(&rec, asked);
    std::thread::sleep(Duration::from_millis(1500));
    say(&format!("started from prepared - first sound after {took}"));
    drop(rec);
    std::thread::sleep(Duration::from_millis(1500));
    say("stopped");

    // The comparison: opened from cold at the "press", as Windows did until this was checked.
    for round in 1..=3 {
        let asked = Instant::now();
        let rec = Recording::start(None).expect("it opens");
        let cold = first_sound(&rec, asked);
        drop(rec);
        std::thread::sleep(Duration::from_millis(700));

        let prepared = Prepared::new(None).expect("a microphone");
        std::thread::sleep(Duration::from_millis(700));
        let asked = Instant::now();
        let rec = prepared.start().expect("it starts");
        let warm = first_sound(&rec, asked);
        drop(rec);
        std::thread::sleep(Duration::from_millis(700));
        println!("round {round}: first sound from cold {cold}, from prepared {warm}");
    }
    say("at the end");
}
