//! Huck's Voice to Text — desktop application.
//!
//! Hotkey -> record -> transcribe locally -> clipboard -> the text box that was focused at the
//! keypress. The clipboard copy always happens first; it is the safeguard when the box is gone.

pub mod bridge;
pub mod clip;
pub mod destination;
pub mod engine_whisper;
pub mod login_item;
pub mod recorder;
pub mod update;
#[cfg(windows)]
mod win_hook;
#[cfg(windows)]
mod win_shortcut;
#[cfg(windows)]
mod win_surface;

/// The gated paste rung, whichever platform this is.
#[cfg(target_os = "macos")]
use crate::destination::macos_paste as platform_paste;
#[cfg(windows)]
use crate::destination::windows_paste as platform_paste;

/// The normal clipboard's paste, as the keyboard labels it.
const NORMAL_PASTE: &str = if cfg!(target_os = "macos") { "⌘V" } else { "Ctrl+V" };
const OS_NAME: &str = if cfg!(target_os = "macos") { "macOS" } else { "Windows" };

/// Open a folder or file the way double-clicking it would.
fn open_path(path: std::path::PathBuf) {
    // Waited on, off the main thread: a spawned child that is never waited for lingers as a
    // finished-but-unreaped process until the app quits.
    std::thread::spawn(move || {
        let opener = if cfg!(windows) { "explorer.exe" } else { "/usr/bin/open" };
        let _ = std::process::Command::new(opener).arg(path).status();
    });
}

use crate::bridge::Bridge;
use crate::destination::PinError;
use hvtt_core::drafts::DraftStore;
use hvtt_core::pipeline::Destination;
use hvtt_core::engine::{Transcriber, TranscriptionRequest};
use hvtt_core::session::SessionState;
use hvtt_core::settings::{ClipboardChoice, LiveWords, Settings};
use hvtt_core::transcript::Transcript;
use parking_lot::Mutex;
use serde::Serialize;
use std::str::FromStr;
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_global_shortcut::{Shortcut, ShortcutState};

/// Everything the UI needs to draw itself, in one message.
#[derive(Debug, Clone, Serialize, Default)]
struct Snapshot {
    state: String,
    message: String,
    text: String,
    /// Recognition engine label, or why there isn't one.
    engine: String,
    engine_ready: bool,
    /// Peak microphone level, 0.0..=1.0.
    level: f32,
    shortcut: String,
    elapsed_ms: u128,
    /// The pinned destination's label, or None. Drives the small "sending to X" indicator.
    pinned: Option<String>,
    /// One short line when pinning is not possible. Never a dialog.
    pin_note: Option<String>,
    /// The words arrived in the text box, so the box is about to put itself away.
    delivered: bool,
    /// Show the one-time Accessibility ask. Once per launch, never twice.
    ask_permission: bool,
    /// The shortcut as the keyboard labels it, e.g. "Option + Space".
    shortcut_label: String,
    /// Set when the shortcut could not be registered, so it is never a silent failure.
    shortcut_error: Option<String>,
    /// "system" or "huck": which clipboard dictations are copied to.
    clipboard: ClipboardChoice,
    /// The paste-last-dictation shortcut as the keyboard labels it.
    paste_shortcut_label: String,
    paste_shortcut_error: Option<String>,
    /// Waiting in the box for new keys, chosen from the menu.
    rebinding: Option<Rebinding>,
    rebind_error: Option<String>,
    /// Check for Updates, while it is being shown.
    update: Option<UpdateView>,
    /// Perceived-latency budget, measured rather than assumed.
    timings: Timings,
    /// The words so far, as they will be sent - his fixes included.
    live_text: String,
    /// Words still being recognised: shown fainter, and may still change.
    live_tail: String,
    /// Paused, and the last words are still being finished; the text cannot be fixed yet.
    settling: bool,
    /// Learning from his fixes, on or off.
    learning: bool,
    /// H › Settings › Live Words.
    live_words: LiveWords,
    /// Which dictation this is, so the box's own click and key counts are never mixed up.
    generation: u64,
}

/// The product's latency numbers. Measured every session so a regression shows up in normal use
/// rather than in a benchmark nobody runs.
#[derive(Debug, Clone, Default, Serialize)]
struct Timings {
    /// Shortcut pressed -> indicator on screen. Budget: under 150 ms.
    shortcut_to_visible_ms: u128,
    /// Shortcut pressed -> microphone actually capturing.
    shortcut_to_capture_ms: u128,
    /// Recognition wall time.
    transcribe_ms: u128,
    /// Clipboard + delivery.
    deliver_ms: u128,
}

struct App {
    session: Mutex<SessionState>,
    transcript: Mutex<Transcript>,
    recording: Mutex<Option<recorder::Recording>>,
    engine: Mutex<Option<Arc<dyn Transcriber>>>,
    engine_status: Mutex<String>,
    settings: Mutex<Settings>,
    drafts: Option<DraftStore>,
    message: Mutex<String>,
    level: Mutex<f32>,
    /// Set when the global shortcut could not be registered — almost always another app owns it.
    shortcut_error: Mutex<Option<String>>,
    paste_shortcut_error: Mutex<Option<String>>,
    rebinding: Mutex<Option<Rebinding>>,
    rebind_error: Mutex<Option<String>>,
    /// Windows: the keyboard hook that hears the new shortcut while the prompt is open.
    #[cfg(windows)]
    key_capture: Mutex<Option<win_shortcut::Capture>>,
    update: Mutex<Option<UpdateView>>,
    /// A downloaded, verified update - the DMG on macOS, the installer on Windows - waiting to
    /// be opened.
    update_file: Mutex<Option<std::path::PathBuf>>,
    /// What the menu was last built from; it is rebuilt only when this changes.
    menu_key: Mutex<String>,
    /// Accessibility, as last checked. Asking macOS is a call to its permission service, so it
    /// is refreshed when the pointer reaches the H and per dictation - never per level update.
    ax_trusted: Mutex<bool>,
    elapsed_ms: Mutex<u128>,
    /// The pinned destination. `None` means clipboard-only, which is a normal outcome.
    destination: Mutex<Option<Box<dyn Destination>>>,
    pin_note: Mutex<Option<String>>,
    delivered: Mutex<bool>,
    ask_permission: Mutex<bool>,
    /// Set the first time the permission is found missing, so the ask happens once per launch.
    /// macOS only: Windows asks for no permission.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    permission_asked: Mutex<bool>,
    /// Bumped by every new dictation and every dismiss, so a delayed auto-hide never puts away a
    /// box that is showing something newer than what it was scheduled for.
    generation: Mutex<u64>,
    timings: Mutex<Timings>,
    bridge: Bridge,
    /// The words recognised so far in this dictation, while he talks.
    live: Mutex<Live>,
    /// Pausing and finishing hold this while they settle the last words, and a live pass holds
    /// it to write its result, so the two never interleave.
    live_pass: Mutex<()>,
    /// Bumped by every start, pause, resume, finish and close. A live pass started before one is
    /// abandoned part-way and its result discarded.
    live_epoch: Arc<std::sync::atomic::AtomicU64>,
    /// He clicked into the box to fix words, so it has the keyboard; it goes back before delivery.
    box_has_keyboard: Mutex<bool>,
    /// Where the keyboard goes back to: the window he was in at the keypress.
    #[cfg(windows)]
    workplace: Mutex<Option<win_surface::Remembered>>,
    #[cfg(target_os = "macos")]
    workplace: Mutex<Option<i32>>,
}

/// One dictation's words, recognised a stretch at a time while he talks.
#[derive(Debug, Clone, Default)]
struct Live {
    /// How much of the recording (16 kHz samples) is recognised for good.
    heard_upto: usize,
    /// What recognition produced, untouched - compared with `text` to learn from his fixes.
    recognised: String,
    /// What will be sent: the recognised words, with his fixes.
    text: String,
    /// The stretch he is still in the middle of, recognised provisionally.
    tail: String,
    /// He changed the words in the box.
    edited: bool,
    /// Pausing: the last words are being finished.
    settling: bool,
}

fn join_words(a: &str, b: &str) -> String {
    match (a.trim().is_empty(), b.trim().is_empty()) {
        (true, _) => b.trim().to_string(),
        (_, true) => a.to_string(),
        _ => format!("{} {}", a.trim_end(), b.trim()),
    }
}

impl App {
    fn snapshot(&self) -> Snapshot {
        let session = self.session.lock();
        // ONE settings lock. `parking_lot::Mutex` is not reentrant, so locking it twice while
        // building this struct deadlocks the thread - which silently stopped the engine from
        // loading and made the hotkey look like it was never registered.
        let (shortcut, clipboard, paste_shortcut, learning, live_words) = {
            let s = self.settings.lock();
            (s.shortcut.clone(), s.clipboard, s.paste_shortcut.clone(), s.learning, s.live_words)
        };
        let live = self.live.lock().clone();
        Snapshot {
            state: session.name().to_string(),
            message: self.message.lock().clone(),
            text: self.transcript.lock().text().to_string(),
            engine: self.engine_status.lock().clone(),
            engine_ready: self.engine.lock().is_some(),
            level: *self.level.lock(),
            shortcut: shortcut.clone(),
            elapsed_ms: *self.elapsed_ms.lock(),
            pinned: self.destination.lock().as_ref().map(|d| d.label()),
            pin_note: self.pin_note.lock().clone(),
            delivered: *self.delivered.lock(),
            ask_permission: *self.ask_permission.lock(),
            shortcut_label: hvtt_core::settings::describe_shortcut(&shortcut),
            shortcut_error: self.shortcut_error.lock().clone(),
            clipboard,
            paste_shortcut_label: hvtt_core::settings::describe_shortcut(&paste_shortcut),
            paste_shortcut_error: self.paste_shortcut_error.lock().clone(),
            rebinding: *self.rebinding.lock(),
            rebind_error: self.rebind_error.lock().clone(),
            update: self.update.lock().clone(),
            timings: self.timings.lock().clone(),
            live_text: live.text,
            live_tail: live.tail,
            settling: live.settling,
            learning,
            live_words,
            generation: *self.generation.lock(),
        }
    }

    fn set_state(&self, next: SessionState) {
        let mut cur = self.session.lock();
        match cur.transition_to(next.clone()) {
            Ok(s) => *cur = s,
            Err(e) => {
                // A rejected transition is a bug, not a user problem. Log it and hold.
                eprintln!("[hvtt] illegal transition ignored: {e}");
            }
        }
    }
}

/// One line per dictation, so the latency budgets are checked by using the product rather than by
/// running a benchmark nobody runs.
fn log_latency(state: &App) {
    let t = state.timings.lock();
    eprintln!(
        "[hvtt] latency: shortcut->visible {}ms | shortcut->capture {}ms | transcribe {}ms | deliver {}ms",
        t.shortcut_to_visible_ms, t.shortcut_to_capture_ms, t.transcribe_ms, t.deliver_ms
    );
}

fn push(app: &AppHandle) {
    let state: State<App> = app.state();
    let _ = app.emit("hvtt:update", state.snapshot());
    refresh_menu(app);
}

/// Bring the composer into view without taking the foreground.
///
/// `show()` alone is correct on macOS: the app runs as an accessory, so showing a window does
/// not activate it. Windows has no such policy, so the box is shown without activation by hand.
/// The never-take-the-foreground rule depends on nothing here calling `set_focus()`.
fn reveal_composer(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("composer") {
        #[cfg(windows)]
        win_surface::show(&w);
        #[cfg(not(windows))]
        let _ = w.show();
    }
}

fn hide_composer(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("composer") {
        #[cfg(windows)]
        win_surface::hide(&w);
        #[cfg(not(windows))]
        let _ = w.hide();
    }
}

/// Put the box away after `delay`, unless something newer has happened in it since.
///
/// Used only where the words are already safe elsewhere - typed into the field - or where there
/// were never any words to keep.
fn hide_after(app: &AppHandle, delay: std::time::Duration) {
    let state: State<App> = app.state();
    let scheduled_for = *state.generation.lock();
    let app = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(delay);
        let state: State<App> = app.state();
        // One lock at a time: `snapshot` holds the session while it reads the generation.
        let same = *state.generation.lock() == scheduled_for;
        if same && !state.session.lock().is_dictating() {
            dismiss(app.clone());
        }
    });
}


// ---------------------------------------------------------------------------- pinning


// ---------------------------------------------------------------------------- the shortcut

/// What a global shortcut does.
#[derive(Clone, Copy)]
enum Action {
    /// Start or stop dictation.
    Dictate,
    /// Paste the last dictation from Huck's own clipboard.
    PasteLast,
}

/// Register `accelerator`, reporting a clash instead of failing quietly.
///
/// A shortcut that silently did not register is the worst outcome: the product's entire promise
/// is that one key press starts dictation, so the failure has to be visible and named.
fn register_shortcut(app: &AppHandle, accelerator: &str, action: Action) -> Result<(), String> {
    use hvtt_core::settings::{describe_shortcut, shortcut_looks_valid};
    use tauri_plugin_global_shortcut::GlobalShortcutExt;

    if !shortcut_looks_valid(accelerator) {
        return Err(format!(
            "{} needs a normal key as well as modifiers.",
            describe_shortcut(accelerator)
        ));
    }
    let parsed = Shortcut::from_str(accelerator)
        .map_err(|_| format!("{} isn't a shortcut {OS_NAME} understands.", describe_shortcut(accelerator)))?;

    // `on_shortcut` attaches the handler to this specific binding. The plugin's global
    // `with_handler` did not fire for shortcuts registered separately, which is the kind of
    // silent failure this product must never ship.
    app.global_shortcut()
        .on_shortcut(parsed, move |handle, _shortcut, event| {
            // Windows: a shortcut holding Alt (Alt+Space) would otherwise leave the app
            // underneath opening its menu bar when Alt comes up.
            #[cfg(windows)]
            if event.state() == ShortcutState::Pressed {
                platform_paste::mask_menu_key();
            }
            // Dictation fires on press - the start is the most felt moment in the product.
            // The paste fires as the letter key comes up: macOS drops a synthetic V while the
            // real V is still held, so a paste sent on press never arrived (measured
            // 2026-09-25). Control and Option may still be down; they do not leak.
            match (action, event.state()) {
                (Action::Dictate, ShortcutState::Pressed) => toggle(handle.clone()),
                (Action::PasteLast, ShortcutState::Released) => paste_last(),
                _ => {}
            }
        })
        .map_err(|_| {
        format!(
            "{} is already used by another app. Pick a different one in the H menu › Settings › Shortcuts.",
            describe_shortcut(accelerator)
        )
    })
}


/// Bind every shortcut the settings call for, starting from nothing, and record any clash.
///
/// One place decides what is bound, so switching the clipboard, rebinding and resetting all end
/// the same way.
fn bind_all(app: &AppHandle) {
    use tauri_plugin_global_shortcut::GlobalShortcutExt;
    let state: State<App> = app.state();
    let _ = app.global_shortcut().unregister_all();
    let s = state.settings.lock().clone();
    // The paste gate exempts this key's auto-repeat, and no other key's.
    #[cfg(windows)]
    platform_paste::set_trigger_key(&s.shortcut);
    let dictate = register_shortcut(app, &s.shortcut, Action::Dictate).err();
    // Only with Huck's clipboard: on the normal one, Cmd+V already does the job.
    let paste = match s.clipboard {
        ClipboardChoice::Huck => register_shortcut(app, &s.paste_shortcut, Action::PasteLast).err(),
        ClipboardChoice::System => None,
    };
    if let Some(why) = &dictate {
        eprintln!("[hvtt] {why}");
    }
    *state.shortcut_error.lock() = dictate;
    *state.paste_shortcut_error.lock() = paste;
}

/// Change a setting and save it. The menu rebuilds itself from the next update.
fn update_settings(app: &AppHandle, change: impl FnOnce(&mut Settings)) {
    let state: State<App> = app.state();
    let mut s = state.settings.lock().clone();
    change(&mut s);
    if let Err(e) = s.save() {
        eprintln!("[hvtt] settings not saved: {e}");
    }
    *state.settings.lock() = s;
}

/// Which shortcut is being rebound while the box waits for keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Rebinding {
    Dictation,
    Paste,
}

/// From the menu: open the box and wait for the new keys - Snip 'n' Clip's capture prompt.
///
/// Every global shortcut is released first, or pressing the current one would start a dictation
/// instead of reaching the box. This is the one time the box takes focus: he asked for it, and
/// it cannot hear keys otherwise.
fn begin_rebind(app: &AppHandle, which: Rebinding) {
    use tauri_plugin_global_shortcut::GlobalShortcutExt;
    let state: State<App> = app.state();
    if matches!(*state.session.lock(), SessionState::Recording | SessionState::Paused | SessionState::Transcribing) {
        return;
    }
    let _ = app.global_shortcut().unregister_all();
    *state.rebinding.lock() = Some(which);
    *state.rebind_error.lock() = None;
    if let Some(w) = app.get_webview_window("composer") {
        // Windows: a keyboard hook hears the keys, so the box never needs the keyboard - and
        // Alt+Space, which Windows keeps from any page, can be chosen. If the hook cannot start,
        // the box takes the keyboard as on macOS.
        #[cfg(windows)]
        {
            // A prompt reopened from the menu replaces the old one; the old one goes first.
            drop(state.key_capture.lock().take());
            let handle = app.clone();
            let capture = win_shortcut::start(move |heard| match heard {
                win_shortcut::Heard::Keys(accelerator) => finish_rebind(handle.clone(), accelerator),
                win_shortcut::Heard::Cancel => dismiss(handle.clone()),
            });
            if capture.is_some() {
                win_surface::show(&w);
            } else {
                win_surface::show_for_keys(&w);
            }
            *state.key_capture.lock() = capture;
        }
        #[cfg(not(windows))]
        {
            let _ = w.show();
            let _ = w.set_focus();
        }
    }
    push(app);
}

/// The keys he pressed. A clash keeps the prompt open with the reason, so he can try again.
#[tauri::command]
fn finish_rebind(app: AppHandle, accelerator: String) {
    use hvtt_core::settings::describe_shortcut;
    use tauri_plugin_global_shortcut::GlobalShortcutExt;
    let state: State<App> = app.state();
    let Some(which) = *state.rebinding.lock() else { return };
    let (other, action) = {
        let s = state.settings.lock();
        match which {
            Rebinding::Dictation => (s.paste_shortcut.clone(), Action::Dictate),
            Rebinding::Paste => (s.shortcut.clone(), Action::PasteLast),
        }
    };
    let tried = if accelerator.eq_ignore_ascii_case(&other) {
        Err(format!("{} is Huck's other shortcut. Try another.", describe_shortcut(&accelerator)))
    } else {
        // Registering is the only reliable test for "another app already owns this".
        register_shortcut(&app, &accelerator, action).map(|()| {
            let _ = app.global_shortcut().unregister_all();
        })
    };
    match tried {
        Ok(()) => {
            update_settings(&app, |s| match which {
                Rebinding::Dictation => s.shortcut = accelerator.clone(),
                Rebinding::Paste => s.paste_shortcut = accelerator.clone(),
            });
            // Rebinds everything and puts the box away.
            dismiss(app.clone());
        }
        Err(why) => {
            *state.rebind_error.lock() = Some(why.replace(" Pick a different one in Settings.", ""));
            // The prompt stays open for another try, with its full time again.
            #[cfg(windows)]
            win_shortcut::extend();
            push(&app);
        }
    }
}

/// Paste the last dictation from Huck's clipboard into whatever he is typing in now.
///
/// This one is aimed by him, deliberately, at the field in front of him - so unlike delivery it
/// needs no pin. His normal clipboard is borrowed for the paste and put back afterwards.
fn paste_last() {
    #[cfg(any(target_os = "macos", windows))]
    std::thread::spawn(move || {
        let Some(text) = crate::clip::huck::read().filter(|t| !t.trim().is_empty()) else {
            return;
        };
        // Immediately, with his fingers still on the keys, on both platforms: see each
        // platform's `press_paste`.
        let _ = platform_paste::paste_borrowing_clipboard(&text);
    });
}

// ---------------------------------------------------------------------------- updates

/// What the floating box says during Check for Updates.
#[derive(Debug, Clone, Serialize)]
struct UpdateView {
    /// checking | current | downloading | ready | failed
    stage: &'static str,
    title: String,
    detail: String,
}

fn show_update(app: &AppHandle, stage: &'static str, title: String, detail: String) {
    let state: State<App> = app.state();
    *state.update.lock() = Some(UpdateView { stage, title, detail });
    // Dictation outranks this: it waits in the state and shows when the box is next free.
    if !matches!(*state.session.lock(), SessionState::Recording | SessionState::Paused | SessionState::Transcribing) {
        reveal_composer(app);
    }
    push(app);
}

/// Settings › Check for Updates…. The only network connection the program makes, and only now.
fn check_for_updates(app: &AppHandle) {
    let state: State<App> = app.state();
    if matches!(state.update.lock().as_ref(), Some(v) if matches!(v.stage, "checking" | "downloading" | "installing")) {
        reveal_composer(app);
        return;
    }
    *state.update_file.lock() = None;
    #[cfg(windows)]
    UPDATE_WARNED.store(false, std::sync::atomic::Ordering::SeqCst);
    show_update(app, "checking", "Checking for updates…".into(), "Asking GitHub.".into());
    let app = app.clone();
    std::thread::spawn(move || {
        let current = app.package_info().version.to_string();
        let fail = |why: String| show_update(&app, "failed", "Couldn't update".into(), why);
        match update::fetch_latest(&current) {
            Err(why) => fail(why),
            Ok(update::Check::UpToDate { .. }) => {
                show_update(
                    &app,
                    "current",
                    "You're up to date".into(),
                    format!("Version {current} is the latest."),
                );
                hide_after(&app, std::time::Duration::from_millis(2600));
            }
            Ok(update::Check::Available(offer)) => {
                show_update(
                    &app,
                    "downloading",
                    format!("Downloading version {}…", offer.version),
                    "It is checked against its published SHA-256 before it is offered.".into(),
                );
                match update::download(&offer) {
                    Err(why) => fail(why),
                    Ok(file) => {
                        *app.state::<App>().update_file.lock() = Some(file);
                        let how = if cfg!(windows) {
                            "Open it and the installer takes over; Huck's Voice to Text closes \
                             for it and your settings stay."
                        } else {
                            "Open it, quit Huck's Voice to Text, and drag the new copy onto \
                             Applications."
                        };
                        show_update(
                            &app,
                            "ready",
                            format!("Version {} is ready", offer.version),
                            how.into(),
                        );
                    }
                }
            }
        }
    });
}

/// macOS: open the verified DMG in Finder - it never installs over itself; he drags the new copy
/// across. Windows: start the verified installer and step aside, as Snip 'n' Clip does, because a
/// running program's files cannot be replaced.
#[tauri::command]
fn open_update(app: AppHandle) {
    let file = app.state::<App>().update_file.lock().clone();
    // Snip 'n' Clip's way: an install that closes this copy and starts the new one. Very silent,
    // so no plain installer window appears: the box says it is installing, and the new copy says
    // it is done (the installer starts it with --updated).
    #[cfg(windows)]
    if let Some(installer) = file {
        // With Keep Recovery Drafts off, Huck's Clipboard is the only copy of his last words, and
        // on Windows it lives in this program's memory: say so before an update empties it. The
        // second Open Update goes ahead.
        let held_at_click = crate::clip::huck::read();
        if huck_clipboard_at_risk(&app) && !UPDATE_WARNED.swap(true, std::sync::atomic::Ordering::SeqCst) {
            warn_huck_clipboard(&app);
            return;
        }
        show_update(
            &app,
            "installing",
            "Installing the update…".into(),
            "Back in a moment. Your settings stay as they are.".into(),
        );
        std::thread::spawn(move || {
            // Long enough to read before this copy steps aside.
            std::thread::sleep(std::time::Duration::from_millis(1200));
            // Never step aside mid-dictation - its words are drafted, copied and delivered first -
            // nor while his own clipboard is out on loan for a paste (Codex's review of 0.1.5).
            let dictating = |app: &AppHandle| {
                matches!(
                    *app.state::<App>().session.lock(),
                    SessionState::Recording | SessionState::Paused | SessionState::Transcribing
                )
            };
            while dictating(&app) || crate::clip::huck::borrows_pending() {
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            // New words arrived on Huck's Clipboard while it waited: those he has not been told
            // about. Stop and say so; Open Update again goes ahead.
            if huck_clipboard_at_risk(&app) && crate::clip::huck::read() != held_at_click {
                warn_huck_clipboard(&app);
                return;
            }
            // The installer must not find this copy still "running" while it quits.
            win_surface::release_single_instance();
            match start_installer(&installer) {
                Ok(()) => app.exit(0),
                Err(e) => {
                    let _ = win_surface::claim_single_instance();
                    show_update(&app, "failed", "Couldn't start the update".into(), e.to_string());
                }
            }
        });
        return;
    }
    #[cfg(not(windows))]
    if let Some(dmg) = file {
        open_path(dmg);
    }
    dismiss(app);
}

/// He has been told this once for the update now on offer (reset by each Check for Updates).
#[cfg(windows)]
static UPDATE_WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Would the update lose words? Only when Huck's Clipboard holds some, it is the chosen
/// clipboard, and no recovery draft keeps a copy on disk.
#[cfg(windows)]
fn huck_clipboard_at_risk(app: &AppHandle) -> bool {
    let state: State<App> = app.state();
    let settings = state.settings.lock();
    settings.clipboard == ClipboardChoice::Huck
        && !settings.keep_drafts
        && crate::clip::huck::read().is_some_and(|t| !t.trim().is_empty())
}

#[cfg(windows)]
fn warn_huck_clipboard(app: &AppHandle) {
    show_update(
        app,
        "ready",
        "Paste what you need first".into(),
        "Updating empties Huck's Clipboard, and Keep Recovery Drafts is off. Paste anything you \
         still need from it, then choose Open Update again."
            .into(),
    );
}

/// Start the verified installer so that an update can never leave him without the program: cmd.exe
/// waits for it, and if it did not finish - failed, cancelled, refused - starts this copy again
/// with --update-failed. A path cmd would read as syntax gets the plain start, as before 0.1.5.
#[cfg(windows)]
fn start_installer(installer: &std::path::Path) -> std::io::Result<()> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let exe = std::env::current_exe()?;
    let watched = update::installer_command(&installer.display().to_string(), &exe.display().to_string());
    match watched {
        Some(line) => {
            let root = std::env::var_os("SystemRoot").map(std::path::PathBuf::from);
            let cmd = root.unwrap_or_else(|| r"C:\Windows".into()).join("System32").join("cmd.exe");
            std::process::Command::new(cmd).raw_arg(line).creation_flags(CREATE_NO_WINDOW).spawn()?;
        }
        None => {
            std::process::Command::new(installer).args(["/VERYSILENT", "/CLOSEAPPLICATIONS"]).spawn()?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------- the menu

/// The menu bar H is where every setting lives, the way it does in Huck's Snip 'n' Clip. There is
/// no settings window: a program that is a layer over other work should not open one.
const TRAY: &str = "hvtt";

/// Everything the menu shows. It is rebuilt only when this changes - not on every level update.
fn menu_key(state: &App) -> String {
    let s = state.settings.lock().clone();
    format!(
        "{}|{}|{}|{:?}|{}|{}|{}|{:?}|{:?}|{:?}|{}|{:?}|{:?}",
        state.session.lock().is_dictating(),
        s.shortcut,
        s.paste_shortcut,
        s.clipboard,
        s.keep_drafts,
        *state.ax_trusted.lock(),
        state.engine_status.lock(),
        state.shortcut_error.lock(),
        state.paste_shortcut_error.lock(),
        state.rebinding.lock(),
        s.learning,
        s.fixes,
        s.live_words,
    )
}

fn refresh_menu(app: &AppHandle) {
    let state: State<App> = app.state();
    let key = menu_key(&state);
    {
        let mut last = state.menu_key.lock();
        if *last == key {
            return;
        }
        *last = key;
    }
    let app2 = app.clone();
    // Menus belong to the main thread on macOS.
    let _ = app.run_on_main_thread(move || match build_menu(&app2) {
        Ok(menu) => {
            if let Some(tray) = app2.tray_by_id(TRAY) {
                let _ = tray.set_menu(Some(menu));
            }
        }
        Err(e) => eprintln!("[hvtt] menu not built: {e}"),
    });
}

/// Snip 'n' Clip's layout: name, actions with their keys, then Settings, then Quit.
fn build_menu(app: &AppHandle) -> tauri::Result<tauri::menu::Menu<tauri::Wry>> {
    use hvtt_core::settings::describe_shortcut as keys;
    use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu};

    let state: State<App> = app.state();
    let s = state.settings.lock().clone();
    let recording = state.session.lock().is_dictating();
    let huck = s.clipboard == ClipboardChoice::Huck;
    let taken = |error: &Mutex<Option<String>>| if error.lock().is_some() { " (in use by another app)" } else { "" };
    let item = |id: &str, text: String, enabled: bool| {
        MenuItem::with_id(app, id, text, enabled, None::<&str>)
    };
    let check = |id: &str, text: &str, on: bool| {
        CheckMenuItem::with_id(app, id, text, true, on, None::<&str>)
    };

    let menu = Menu::new(app)?;
    menu.append(&item("heading", "Huck's Voice to Text".into(), false)?)?;
    menu.append(&item("brand", "Powered by Project Playground".into(), false)?)?;
    menu.append(&PredefinedMenuItem::separator(app)?)?;

    if !*state.ax_trusted.lock() {
        menu.append(&item("allow-ax", "Allow Accessibility…".into(), true)?)?;
        menu.append(&PredefinedMenuItem::separator(app)?)?;
    }

    let verb = if recording { "Stop Dictation" } else { "Start Dictation" };
    menu.append(&item("dictate", format!("{verb} — {}", keys(&s.shortcut)), true)?)?;
    menu.append(&PredefinedMenuItem::separator(app)?)?;

    let shortcuts = Submenu::with_id(app, "shortcuts", "Shortcuts", !recording)?;
    shortcuts.append(&item(
        "rebind-dictation",
        format!("Dictation — {}{}", keys(&s.shortcut), taken(&state.shortcut_error)),
        true,
    )?)?;
    // Always listed, so the key is there to see and change; it works while Huck's clipboard is
    // the choice.
    shortcuts.append(&item(
        "rebind-paste",
        format!("Huck's Clipboard — {}{}", keys(&s.paste_shortcut), taken(&state.paste_shortcut_error)),
        true,
    )?)?;
    shortcuts.append(&PredefinedMenuItem::separator(app)?)?;
    shortcuts.append(&item("reset-shortcuts", "Reset Shortcuts to Defaults".into(), true)?)?;

    let clipboard = Submenu::with_id(app, "clipboard", "Clipboard", true)?;
    clipboard.append(&check("clip-system", &format!("Normal Clipboard — {NORMAL_PASTE}"), !huck)?)?;
    clipboard.append(&check(
        "clip-huck",
        &format!("Huck's Clipboard — {}", keys(&s.paste_shortcut)),
        huck,
    )?)?;

    // Plain words, so nobody has to know what processor they have.
    let live = Submenu::with_id(app, "live-words", "Live Words", true)?;
    live.append(&check("live-as-you-talk", "As You Talk", s.live_words == LiveWords::AsYouTalk)?)?;
    live.append(&check(
        "live-lighter",
        "Lighter — easier on older computers and battery",
        s.live_words == LiveWords::Lighter,
    )?)?;
    live.append(&check("live-off", "Off — words appear when you send", s.live_words == LiveWords::Off)?)?;

    let settings = Submenu::with_id(app, "settings", "Settings", true)?;
    settings.append(&shortcuts)?;
    settings.append(&clipboard)?;
    settings.append(&live)?;
    settings.append(&PredefinedMenuItem::separator(app)?)?;
    let login = login_item::state();
    let (login_title, login_on) = login_item::presentation(login);
    settings.append(&CheckMenuItem::with_id(
        app,
        "start-at-login",
        login_title,
        login != login_item::LoginState::Unavailable,
        login_on,
        None::<&str>,
    )?)?;
    settings.append(&check("learning", "Learn From My Fixes", s.learning)?)?;
    let learned = Submenu::with_id(app, "learned", "Learned Fixes", true)?;
    if s.fixes.is_empty() {
        learned.append(&item("learned-none", "Nothing learned yet".into(), false)?)?;
    } else {
        learned.append(&item("learned-how", "Choose one to forget it".into(), false)?)?;
        for (i, fix) in s.fixes.iter().enumerate() {
            learned.append(&item(&format!("forget-{i}"), format!("{} → {}", fix.from, fix.to), true)?)?;
        }
        learned.append(&PredefinedMenuItem::separator(app)?)?;
        learned.append(&item("forget-all", "Forget Everything Learned".into(), true)?)?;
    }
    settings.append(&learned)?;
    settings.append(&PredefinedMenuItem::separator(app)?)?;
    settings.append(&check("keep-drafts", "Keep Recovery Drafts", s.keep_drafts)?)?;
    settings.append(&item("open-drafts", "Open Drafts Folder".into(), true)?)?;
    settings.append(&PredefinedMenuItem::separator(app)?)?;
    // "ggml-base.en.bin" is a file name; "base.en" is the model.
    let model = state.engine_status.lock().trim_start_matches("ggml-").trim_end_matches(".bin").to_string();
    settings.append(&item("model", format!("Speech Model — {model}"), false)?)?;
    settings.append(&PredefinedMenuItem::separator(app)?)?;
    settings.append(&item("check-updates", "Check for Updates…".into(), !recording)?)?;
    settings.append(&item("version", format!("Version {}", app.package_info().version), false)?)?;
    menu.append(&settings)?;

    menu.append(&PredefinedMenuItem::separator(app)?)?;
    menu.append(&MenuItem::with_id(app, "quit", "Quit", true, Some("CmdOrCtrl+Q"))?)?;
    Ok(menu)
}

fn on_menu(app: &AppHandle, id: &str) {
    let state: State<App> = app.state();
    // Windows leaves the menu's hidden window in front; give it back to where he was first.
    #[cfg(windows)]
    if let Some(workplace) = win_surface::take_last_workplace() {
        win_surface::give_back_foreground(workplace);
    }
    match id {
        // Windows: a moment for that window to take its focus back, so the dictation is aimed at
        // the text box he was in - as it is when the menu-bar H starts one on macOS.
        #[cfg(windows)]
        "dictate" => {
            let app = app.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(120));
                let again = app.clone();
                let _ = app.run_on_main_thread(move || toggle(again));
            });
        }
        #[cfg(not(windows))]
        "dictate" => toggle(app.clone()),
        "allow-ax" => open_accessibility_settings(),
        "check-updates" => check_for_updates(app),
        "rebind-dictation" => begin_rebind(app, Rebinding::Dictation),
        "rebind-paste" => begin_rebind(app, Rebinding::Paste),
        "reset-shortcuts" => {
            update_settings(app, |s| {
                s.shortcut = hvtt_core::settings::DEFAULT_SHORTCUT.to_string();
                s.paste_shortcut = hvtt_core::settings::DEFAULT_PASTE_SHORTCUT.to_string();
            });
            bind_all(app);
        }
        "clip-system" | "clip-huck" => {
            let choice = if id == "clip-huck" { ClipboardChoice::Huck } else { ClipboardChoice::System };
            update_settings(app, |s| s.clipboard = choice);
            bind_all(app);
        }
        "start-at-login" => {
            if let Err(e) = login_item::toggle() {
                eprintln!("[hvtt] startup setting: {e}");
                *state.message.lock() = e;
            }
        }
        "keep-drafts" => update_settings(app, |s| s.keep_drafts = !s.keep_drafts),
        "learning" => update_settings(app, |s| s.learning = !s.learning),
        "live-as-you-talk" => update_settings(app, |s| s.live_words = LiveWords::AsYouTalk),
        "live-lighter" => update_settings(app, |s| s.live_words = LiveWords::Lighter),
        "live-off" => update_settings(app, |s| s.live_words = LiveWords::Off),
        "forget-all" => update_settings(app, |s| s.fixes.clear()),
        other if other.starts_with("forget-") => {
            if let Ok(i) = other["forget-".len()..].parse::<usize>() {
                update_settings(app, |s| {
                    if i < s.fixes.len() {
                        s.fixes.remove(i);
                    }
                });
            }
        }
        "open-drafts" => {
            if let Some(dir) = hvtt_core::paths::drafts_dir() {
                let _ = std::fs::create_dir_all(&dir);
                open_path(dir);
            }
        }
        "quit" => app.exit(0),
        _ => return,
    }
    // A check item flips its own tick when clicked; rebuild so it always shows the setting.
    state.menu_key.lock().clear();
    push(app);
}

/// True when Accessibility has been granted. Used to decide whether to show the one-time ask.
#[tauri::command]
fn accessibility_ready() -> bool {
    #[cfg(target_os = "macos")]
    {
        crate::destination::macos_ax::accessibility_trusted()
    }
    #[cfg(not(target_os = "macos"))]
    {
        true
    }
}

/// Open the exact System Settings pane, so the permission ask is one click, not a treasure hunt.
#[tauri::command]
fn open_accessibility_settings() {
    // Without this the app is not in the list at all, and the pane opens with nothing to tick.
    // Windows needs no grant for UI Automation, so there is nothing to open there.
    #[cfg(target_os = "macos")]
    {
        crate::destination::macos_ax::request_accessibility();
        open_path("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility".into());
    }
}

// ---------------------------------------------------------------------------- commands

#[tauri::command]
fn get_snapshot(state: State<App>) -> Snapshot {
    state.snapshot()
}

/// The dictation shortcut: start, or - listening or paused - send.
#[tauri::command]
fn toggle(app: AppHandle) {
    let state: State<App> = app.state();
    if state.rebinding.lock().is_some() {
        return;
    }
    let (dictating, busy) = {
        let session = state.session.lock();
        (session.is_dictating(), matches!(*session, SessionState::Transcribing))
    };
    if dictating {
        crate::destination::box_input::set_stopped_by_key(true);
        finish(app.clone(), true);
    } else if !busy {
        start_recording(app.clone());
    }
}

/// The box's running count of clicks and key presses inside it, for the paste gate.
#[derive(Debug, Clone, Copy, serde::Deserialize)]
struct BoxInput {
    generation: u64,
    clicks: u32,
    keys: u32,
    keys_repeated: u32,
}

fn note_input(input: BoxInput) {
    crate::destination::box_input::record(
        input.generation,
        input.clicks,
        input.keys,
        input.keys_repeated,
    );
}

#[tauri::command]
fn box_input(input: BoxInput) {
    note_input(input);
}

/// The box's Pause / Resume button.
#[tauri::command]
fn pause_resume(app: AppHandle, input: BoxInput) {
    note_input(input);
    let state: State<App> = app.state();
    let now = state.session.lock().clone();
    match now {
        SessionState::Recording => pause(app.clone()),
        SessionState::Paused => resume(app.clone()),
        _ => {}
    }
}

/// macOS: any click on the box - a button, the bar, a drag - makes this the active app, which
/// takes the caret out of his text box. Windows' box never activates, so there it stays put.
/// Once the mouse is let go, give the caret back - to the app he was in, which is not always the
/// one he is dictating into - unless the click was into the words to fix them (`take_keyboard`)
/// or the box is the shortcut prompt.
#[cfg(target_os = "macos")]
fn box_became_key(app: &AppHandle) {
    let app = app.clone();
    let was_in = crate::destination::macos_paste::front_app();
    std::thread::spawn(move || {
        let wait = std::time::Duration::from_millis(20);
        for _ in 0..500 {
            if !crate::destination::macos_paste::mouse_is_down() {
                break;
            }
            std::thread::sleep(wait);
        }
        // Time for the click to reach the page and say whether it wants the keyboard.
        std::thread::sleep(std::time::Duration::from_millis(80));
        let state: State<App> = app.state();
        let dictating = state.session.lock().is_dictating();
        if dictating && !*state.box_has_keyboard.lock() && state.rebinding.lock().is_none() {
            if let Some(pid) = was_in.or(*state.workplace.lock()) {
                crate::destination::macos_paste::activate(pid);
            }
        }
    });
}

/// The box's Send button: the same as pressing the shortcut, without the key press.
#[tauri::command]
fn send(app: AppHandle, input: BoxInput) {
    note_input(input);
    let dictating = app.state::<App>().session.lock().is_dictating();
    if dictating {
        crate::destination::box_input::set_stopped_by_key(false);
        // macOS: the click made this the active app, so the box does hold the keyboard; saying so
        // has `finish` hand it back, and wait for the caret, before anything is pasted.
        #[cfg(target_os = "macos")]
        {
            *app.state::<App>().box_has_keyboard.lock() = true;
        }
        finish(app.clone(), true);
    }
}

/// He fixed the words in the box. Only while paused, and once the last words are in.
#[tauri::command]
fn edit_text(app: AppHandle, text: String, input: BoxInput) {
    note_input(input);
    let state: State<App> = app.state();
    let paused = matches!(*state.session.lock(), SessionState::Paused);
    let mut live = state.live.lock();
    if paused && !live.settling && live.text != text {
        live.text = text;
        live.edited = true;
    }
}

/// He clicked into the words to fix them: the box takes the keyboard until the words are sent.
#[tauri::command]
fn take_keyboard(app: AppHandle, input: BoxInput) {
    note_input(input);
    let state: State<App> = app.state();
    if !state.session.lock().is_dictating() {
        return;
    }
    *state.box_has_keyboard.lock() = true;
    if let Some(w) = app.get_webview_window("composer") {
        #[cfg(windows)]
        win_surface::show_for_keys(&w);
        #[cfg(not(windows))]
        let _ = w.set_focus();
    }
}

/// The Learning switch in the box, the same setting as H › Settings › Learn From My Fixes.
#[tauri::command]
fn set_learning(app: AppHandle, on: bool) {
    update_settings(&app, |s| s.learning = on);
    push(&app);
}

/// Anything worth keeping: words already recognised, or speech not yet recognised.
fn dictation_has_words(state: &App) -> bool {
    let (has_text, from) = {
        let live = state.live.lock();
        (!live.text.trim().is_empty(), live.heard_upto)
    };
    has_text
        || state
            .recording
            .lock()
            .as_ref()
            .is_some_and(|r| hvtt_core::audio::has_speech(&r.peek_from(from)))
}

/// Put the box away. Always available, in every state, so the box can never get stuck on screen.
///
/// Mid-dictation with words already heard, they are kept - recognised, saved as a draft and
/// copied - but not sent anywhere. With nothing heard, the recording is simply dropped.
#[tauri::command]
fn dismiss(app: AppHandle) {
    let state: State<App> = app.state();
    let (busy, dictating) = {
        let session = state.session.lock();
        (matches!(*session, SessionState::Transcribing), session.is_dictating())
    };
    if busy {
        // The words are about to be copied and delivered.
        return;
    }
    if dictating && dictation_has_words(&state) {
        finish(app.clone(), false);
        return;
    }
    if std::mem::take(&mut *state.box_has_keyboard.lock()) {
        hand_back_keyboard(&app, false);
    }
    state.live_epoch.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    *state.live.lock() = Live::default();
    *state.generation.lock() += 1;
    // Leaving the key prompt, by any route, puts every shortcut back.
    // Let go of the keyboard - always, whatever state the prompt is in - before the shortcuts go
    // back on.
    #[cfg(windows)]
    drop(state.key_capture.lock().take());
    if state.rebinding.lock().take().is_some() {
        *state.rebind_error.lock() = None;
        bind_all(&app);
    }
    // Nothing heard worth keeping (checked above): the recording just stops.
    drop(state.recording.lock().take());
    *state.level.lock() = 0.0;
    *state.delivered.lock() = false;
    *state.ask_permission.lock() = false;
    if !matches!(state.update.lock().as_ref(), Some(v) if matches!(v.stage, "checking" | "downloading" | "installing")) {
        *state.update.lock() = None;
    }
    *state.transcript.lock() = Transcript::empty();
    *state.message.lock() = String::new();
    *state.elapsed_ms.lock() = 0;
    state.set_state(SessionState::Idle);
    // The dictation is over, so let go of where it went. On Windows this is also what ends the
    // click and key count behind the paste gate, which runs only while a dictation needs it.
    #[cfg(windows)]
    {
        *state.destination.lock() = None;
    }
    hide_composer(&app);
    push(&app);
}


#[tauri::command]
fn get_settings(state: State<App>) -> Settings {
    state.settings.lock().clone()
}

#[tauri::command]
fn save_settings(app: AppHandle, settings: Settings) -> Result<(), String> {
    let state: State<App> = app.state();
    settings.save().map_err(|e| e.to_string())?;
    *state.settings.lock() = settings;
    push(&app);
    Ok(())
}

/// Where the recovery drafts are, so the user can always find their words.
#[tauri::command]
fn drafts_dir() -> Option<String> {
    hvtt_core::paths::drafts_dir().map(|p| p.display().to_string())
}

// ---------------------------------------------------------------------------- pipeline

fn start_recording(app: AppHandle) {
    let pressed = std::time::Instant::now();
    let state: State<App> = app.state();

    if state.engine.lock().is_none() {
        let why = state.engine_status.lock().clone();
        *state.message.lock() = why.clone();
        state.set_state(SessionState::Error { message: why });
        reveal_composer(&app);
        push(&app);
        return;
    }

    // 0. CAPTURE THE DESTINATION AT THE KEYPRESS — before anything else.
    //
    // The pin must mean "the field that was focused when I pressed the shortcut". Reading focus
    // later reads a different field, because by then the user has clicked away — which is the
    // whole workflow this product is for. The Accessibility capture is a handful of C calls and
    // costs microseconds, so it fits ahead of the indicator; validating it does not, and
    // happens later from the snapshot alone.
    #[cfg(target_os = "macos")]
    let pending = hvtt_core::pinning::PendingPin::capture(
        &crate::destination::macos_ax::AxFocusSource,
    );
    // What was in front and how much he had touched, for the paste rung's "nothing moved" gate.
    // On Windows this is the whole keypress capture: the focused element is read from UI
    // Automation on a worker a moment later, and refused if anything moved in between.
    #[cfg(any(target_os = "macos", windows))]
    let stamp = platform_paste::FocusStamp::capture();
    // Where the keyboard goes back to if he clicks into the box to fix words.
    #[cfg(windows)]
    {
        *state.workplace.lock() = win_surface::front_workplace();
    }
    #[cfg(target_os = "macos")]
    {
        *state.workplace.lock() = stamp.map(|s| s.pid());
    }

    // The browser's own pin has to be taken at the same instant, so the request is sent now and
    // waited for later. The extension pins its focused element the moment this arrives.
    let browser_pin = {
        let bridge = state.bridge.clone();
        std::thread::spawn(move || crate::destination::chromium::ChromiumDestination::pin(bridge))
    };

    // 1. THE INDICATOR, before anything that can block. The budget is 150 ms from keypress to
    //    visible; it is the most felt number in the product.
    // A new dictation starts empty. The previous words are already in their text box, or on
    // the clipboard and in the recovery draft; appending them would send old words somewhere new.
    let generation = {
        let mut g = state.generation.lock();
        *g += 1;
        *g
    };
    crate::destination::box_input::reset(generation);
    state.live_epoch.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    *state.live.lock() = Live::default();
    *state.box_has_keyboard.lock() = false;
    *state.transcript.lock() = Transcript::empty();
    *state.delivered.lock() = false;
    *state.ask_permission.lock() = false;
    // A finished update message gives way; a check still running re-shows itself when done.
    if !matches!(state.update.lock().as_ref(), Some(v) if matches!(v.stage, "checking" | "downloading" | "installing")) {
        *state.update.lock() = None;
    }
    state.set_state(SessionState::Recording);
    *state.message.lock() = "Listening…".into();
    *state.elapsed_ms.lock() = 0;
    *state.pin_note.lock() = None;
    *state.destination.lock() = None;
    reveal_composer(&app);
    state.timings.lock().shortcut_to_visible_ms = pressed.elapsed().as_millis();
    push(&app);

    // 2. Microphone, so the first words are not lost while the destination is worked out.
    let device = state.settings.lock().input_device.clone();
    match recorder::Recording::start(device.as_deref()) {
        Ok(rec) => {
            *state.recording.lock() = Some(rec);
            state.timings.lock().shortcut_to_capture_ms = pressed.elapsed().as_millis();
            spawn_level_pump(app.clone());
            spawn_live(app.clone(), generation);
        }
        Err(e) => {
            let settings = if cfg!(windows) {
                "Settings › Privacy › Microphone"
            } else {
                "System Settings › Privacy"
            };
            let msg = format!("{e} Check microphone access in {settings}.");
            *state.message.lock() = msg.clone();
            state.set_state(SessionState::Error { message: msg });
            push(&app);
            return;
        }
    }

    // 3. Validation, off the critical path, using ONLY what was captured in step 0.
    let app2 = app.clone();
    std::thread::spawn(move || {
        let browser = browser_pin.join().ok();
        #[cfg(target_os = "macos")]
        resolve_pin(&app2, pending, stamp, browser);
        #[cfg(windows)]
        resolve_pin_windows(&app2, stamp, browser);
        #[cfg(not(any(target_os = "macos", windows)))]
        let _ = browser;
        push(&app2);
    });
}

/// Windows' half of `resolve_pin`: the same three ways in, best first - UI Automation (native
/// apps, which works even after he clicks away), the browser extension (Chrome, Edge), and a
/// paste (Electron apps, Chrome without the extension, anything UI Automation cannot write),
/// made only if nothing has moved since the keypress. No permission step: UI Automation needs
/// no grant on Windows.
#[cfg(windows)]
fn resolve_pin_windows(
    app: &AppHandle,
    stamp: Option<crate::destination::windows_paste::FocusStamp>,
    browser: Option<Result<crate::destination::chromium::ChromiumDestination, String>>,
) {
    use crate::destination::windows_paste::PasteDestination;
    use crate::destination::{is_chromium_executable, is_unsupported_executable, windows_uia};

    let state: State<App> = app.state();
    let set = |d: Box<dyn Destination>| {
        *state.message.lock() = format!("Listening — will send to {}", d.label());
        *state.destination.lock() = Some(d);
    };
    let note = |e: PinError| {
        *state.pin_note.lock() = Some(e.message());
        if !e.is_expected() {
            *state.message.lock() = e.message();
        }
    };
    let release_browser = || {
        let _ = state.bridge.request("unpin", None, std::time::Duration::from_millis(300));
    };

    // Nothing to type into was in front: the desktop, the taskbar, or the H's own menu.
    let Some(stamp) = stamp else {
        if let Some(Ok(_)) = browser {
            release_browser();
        }
        note(PinError::NotATextField);
        return;
    };

    let exe = windows_uia::executable_of(stamp.pid());
    let app_label = is_unsupported_executable(&exe).unwrap_or_else(|| windows_uia::app_name(&exe));
    let borrow = state.settings.lock().clipboard == ClipboardChoice::Huck;
    let paste = || PasteDestination::new(stamp.clone(), app_label.clone(), borrow);

    // The extension is the only silent way into Chrome. Without it, a paste, gated.
    // Any text box is a destination, password boxes included (decided with him 2026-09-28).
    // After a click in the same window the paste goes where the caret is, the address bar
    // included (decided with him 2026-09-29): typed there, never sent, and still on the clipboard.
    if is_chromium_executable(&exe) {
        match browser {
            Some(Ok(d)) => set(Box::new(d)),
            _ => set(Box::new(paste().forgiving(crate::destination::windows_paste::SameWindow::Caret))),
        }
        return;
    } else if let Some(Ok(_)) = browser {
        // Not our destination; release it so the extension is not left holding a field.
        release_browser();
    }

    // Desktop apps built on Chromium (VS Code, Slack, Discord, the Claude and ChatGPT apps) are
    // never asked: asking flips VS Code into screen-reader mode. Gated, like every paste.
    // Every Chromium window gets the Mac's rule for VS Code: after a click in the same window the
    // paste goes where the caret is - an address bar included, as in Chrome above.
    if stamp.is_chromium_window() || is_unsupported_executable(&exe).is_some() {
        let rule = if stamp.is_chromium_window() {
            crate::destination::windows_paste::SameWindow::Caret
        } else {
            crate::destination::windows_paste::SameWindow::Strict
        };
        set(Box::new(paste().forgiving(rule)));
        return;
    }

    match windows_uia::capture(&stamp).and_then(|el| windows_uia::validate_captured(el, app_label.clone())) {
        Ok(d) => set(Box::new(d.with_paste_fallback(Some(paste())))),
        // Nothing UI Automation could write: paste, gated. That includes focus having moved
        // before the capture landed - the gate then refuses at delivery, just as it would had he
        // moved later, and the words wait on the clipboard.
        Err(_) => set(Box::new(paste())),
    }
}

/// Decide what the captured candidate actually is, and refuse rather than substitute.
///
/// Three ways in, best first: a silent Accessibility write (native apps and Safari, which works
/// even after he clicks away), the browser extension (Chrome), and a paste (everything else),
/// which is only made if nothing has moved since the keypress. The clipboard already has the
/// words whichever way this goes.
#[cfg(target_os = "macos")]
fn resolve_pin(
    app: &AppHandle,
    pending: hvtt_core::pinning::PendingPin<crate::destination::macos_ax::AxElement>,
    stamp: Option<crate::destination::macos_paste::FocusStamp>,
    browser: Option<Result<crate::destination::chromium::ChromiumDestination, String>>,
) {
    use crate::destination::macos_paste::PasteDestination;
    use crate::destination::{is_chromium_executable, is_unsupported_executable, macos_ax};

    let state: State<App> = app.state();

    // The front window's owner stands in when Accessibility shows no focused element, which is
    // how Electron apps look from outside.
    let exe = pending
        .candidate()
        .and_then(macos_ax::pid_of)
        .or(stamp.map(|s| s.pid()))
        .map(macos_ax::executable_of)
        .unwrap_or_default();
    let app_label = is_unsupported_executable(&exe).unwrap_or_else(|| {
        exe.rsplit('/').next().filter(|n| !n.is_empty()).unwrap_or("that app").to_string()
    });
    let borrow = state.settings.lock().clipboard == ClipboardChoice::Huck;
    // The box itself, when the app names it: lets him click away and come back (2026-09-29).
    let field = pending.candidate().cloned();
    eprintln!(
        "[hvtt] {app_label}: focused {}",
        field.as_ref().map(|f| f.role_name()).unwrap_or_else(|| "nothing".into())
    );
    let paste = || {
        stamp.map(|s| PasteDestination::new(s, app_label.clone(), borrow).with_field(field.clone()))
    };

    let set = |d: Box<dyn Destination>| {
        *state.message.lock() = format!("Listening — will send to {}", d.label());
        *state.destination.lock() = Some(d);
    };
    let note = |e: PinError| {
        *state.pin_note.lock() = Some(e.message());
        if !e.is_expected() {
            *state.message.lock() = e.message();
        }
    };

    // The extension is the only silent way into Chrome, and it needs no macOS permission.
    // Any text box is a destination, password boxes included (decided with him 2026-09-28).
    if is_chromium_executable(&exe) {
        if let Some(Ok(d)) = browser {
            set(Box::new(d));
            return;
        }
    } else if let Some(Ok(_)) = browser {
        // Not our destination; release it so the extension is not left holding a field.
        let _ = state
            .bridge
            .request("unpin", None, std::time::Duration::from_millis(300));
    }

    // Everything below writes or pastes into another app, and macOS allows neither without
    // Accessibility. Its own prompt adds the app to the list - the plain check never did, which
    // left nothing to switch on. Once per launch.
    let trusted = macos_ax::accessibility_trusted();
    *state.ax_trusted.lock() = trusted;
    if !trusted {
        if !std::mem::replace(&mut *state.permission_asked.lock(), true) {
            *state.ask_permission.lock() = true;
            macos_ax::request_accessibility();
        }
        note(PinError::AccessibilityPermissionMissing);
        return;
    }

    // Chrome without the extension has no silent write at all, and desktop Chromium apps change
    // how they behave when Accessibility asks them anything: paste, gated.
    if is_chromium_executable(&exe) || is_unsupported_executable(&exe).is_some() {
        match paste() {
            Some(p) => set(Box::new(p)),
            None => note(PinError::Unsupported { app: app_label.clone() }),
        }
        return;
    }

    match pending.resolve(macos_ax::validate_captured) {
        Ok(d) => set(Box::new(d.with_paste_fallback(paste()))),
        // Nothing Accessibility could read, or not a text field it knows: paste, gated.
        Err(_) => match paste() {
            Some(p) => set(Box::new(p)),
            None => note(PinError::NotATextField),
        },
    }
}

/// Feed the listening animation while the microphone is open. Paused, the H rests.
fn spawn_level_pump(app: AppHandle) {
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_millis(60));
        let state: State<App> = app.state();
        let level = {
            let rec = state.recording.lock();
            match rec.as_ref() {
                Some(r) => r.take_level(),
                None => break,
            }
        };
        let (dictating, listening) = {
            let session = state.session.lock();
            (session.is_dictating(), session.is_capturing())
        };
        if !dictating {
            break;
        }
        let level = if listening { level } else { 0.0 };
        let changed = std::mem::replace(&mut *state.level.lock(), level) != level;
        if listening || changed {
            push(&app);
        }
    });
}

// ---------------------------------------------------------------------------- live words

/// What recognition is told before it listens, and the fixes applied to what it hears.
struct Hints {
    prompt: Option<String>,
    fixes: Vec<hvtt_core::learning::Fix>,
}

/// His vocabulary (with what Learning taught, while it is on), plus the last words already
/// recognised, so each stretch carries on from the one before it.
fn hints(state: &App, before: &str) -> Hints {
    let s = state.settings.lock().clone();
    let fixes = if s.learning { s.fixes.clone() } else { Vec::new() };
    let vocabulary = Settings { fixes: fixes.clone(), ..s }.vocabulary_prompt();
    let words: Vec<&str> = before.split_whitespace().collect();
    let context = words[words.len().saturating_sub(25)..].join(" ");
    let prompt = match (vocabulary, context.is_empty()) {
        (Some(v), false) => Some(format!("{v}. {context}")),
        (Some(v), true) => Some(v),
        (None, false) => Some(context),
        (None, true) => None,
    };
    Hints { prompt, fixes }
}

/// Recognise one stretch. Silence is never sent: Whisper invents words for it.
fn recognise(
    engine: &Arc<dyn Transcriber>,
    samples: &[f32],
    hints: &Hints,
    give_up: Option<hvtt_core::engine::GiveUp>,
    provisional: bool,
) -> Result<Option<String>, String> {
    if samples.len() < hvtt_core::audio::WHISPER_SAMPLE_RATE as usize / 4
        || !hvtt_core::audio::has_speech(samples)
    {
        return Ok(None);
    }
    let req = TranscriptionRequest {
        samples: samples.to_vec(),
        vocabulary_prompt: hints.prompt.clone(),
        provisional,
        give_up,
    };
    let heard = engine.transcribe(&req).map_err(|e| e.to_string())?.text;
    let text = hvtt_core::learning::apply(heard.trim(), &hints.fixes);
    Ok((!text.trim().is_empty()).then_some(text))
}

/// Recognise while he talks, so the box fills in and the stop press has only the last few
/// seconds left to do. Every 0.3 s: a finished stretch, ended at a pause, is recognised for good;
/// the stretch he is still in is recognised provisionally and shown fainter.
///
/// Recognition takes nearly every core, so the rest between passes scales with how long the last
/// one took (H › Settings › Live Words, `LiveWords::rest_after`): a slow machine refreshes less
/// often rather than stuttering. With Live Words off, nothing is recognised until he pauses or
/// sends.
fn spawn_live(app: AppHandle, generation: u64) {
    let mut breather = std::time::Duration::from_millis(300);
    std::thread::spawn(move || loop {
        std::thread::sleep(breather);
        let pass_started = std::time::Instant::now();
        let state: State<App> = app.state();
        // One lock at a time: `snapshot` holds the session while it reads the generation.
        let same = *state.generation.lock() == generation;
        if !same || !state.session.lock().is_dictating() {
            break;
        }
        let live_words = state.settings.lock().live_words;
        if !state.session.lock().is_capturing() || live_words == LiveWords::Off {
            breather = std::time::Duration::from_millis(300);
            continue;
        }
        let Some(engine) = state.engine.lock().clone() else { break };
        let epoch = state.live_epoch.load(std::sync::atomic::Ordering::SeqCst);
        let give_up = || {
            Some(hvtt_core::engine::GiveUp { counter: state.live_epoch.clone(), value: epoch })
        };
        let (from, before) = {
            let live = state.live.lock();
            (live.heard_upto, live.text.clone())
        };
        // Only what is not yet recognised for good.
        let Some(rest) = state.recording.lock().as_ref().map(|r| r.peek_from(from)) else { break };

        let (cut, settled) = match hvtt_core::audio::commit_point(&rest, 3.0, 12.0) {
            Some(cut) => {
                // These words are kept and sent: recognised with full care.
                let words =
                    recognise(&engine, &rest[..cut], &hints(&state, &before), give_up(), false);
                (cut, words.ok().flatten())
            }
            None => (0, None),
        };
        let upto = from + cut;
        let so_far = join_words(&before, settled.as_deref().unwrap_or(""));
        // Only shown, and replaced moments later: one quick attempt.
        let tail = recognise(&engine, &rest[cut..], &hints(&state, &so_far), give_up(), true)
            .ok()
            .flatten();
        breather = live_words
            .rest_after(pass_started.elapsed())
            .unwrap_or(std::time::Duration::from_millis(300));

        // Written only if nothing paused, resumed or finished the dictation in the meantime.
        {
            let _pass = state.live_pass.lock();
            let same = *state.generation.lock() == generation;
            let current = same && state.session.lock().is_capturing();
            let stale = state.live_epoch.load(std::sync::atomic::Ordering::SeqCst) != epoch;
            let mut live = state.live.lock();
            if !current || stale || live.heard_upto != from {
                continue;
            }
            live.heard_upto = upto;
            if let Some(words) = &settled {
                live.recognised = join_words(&live.recognised, words);
                live.text = join_words(&live.text, words);
            }
            live.tail = tail.unwrap_or_default();
        }
        push(&app);
    });
}

/// Pause: the microphone stops, the words he was in the middle of are finished, and the text
/// can be fixed in the box. Nothing said while paused is kept.
fn pause(app: AppHandle) {
    std::thread::spawn(move || {
        let state: State<App> = app.state();
        let _pass = state.live_pass.lock();
        if !state.session.lock().is_capturing() {
            return;
        }
        state.live_epoch.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        // Nothing else moves `heard_upto` while this holds `live_pass`.
        let (from, before) = {
            let live = state.live.lock();
            (live.heard_upto, live.text.clone())
        };
        let rest = {
            let rec = state.recording.lock();
            let Some(r) = rec.as_ref() else { return };
            r.pause();
            r.peek_from(from)
        };
        state.set_state(SessionState::Paused);
        state.live.lock().settling = true;
        *state.level.lock() = 0.0;
        push(&app);

        let engine = state.engine.lock().clone();
        let words = engine
            .and_then(|e| recognise(&e, &rest, &hints(&state, &before), None, false).ok().flatten());
        {
            let mut live = state.live.lock();
            live.heard_upto = from + rest.len();
            if let Some(words) = &words {
                live.recognised = join_words(&live.recognised, words);
                live.text = join_words(&live.text, words);
            }
            live.tail.clear();
            live.settling = false;
        }
        push(&app);
    });
}

/// Resume: the microphone opens again and new words carry on after the ones in the box.
fn resume(app: AppHandle) {
    std::thread::spawn(move || {
        let state: State<App> = app.state();
        let _pass = state.live_pass.lock();
        if !matches!(*state.session.lock(), SessionState::Paused) {
            return;
        }
        if let Some(r) = state.recording.lock().as_ref() {
            r.resume();
        }
        state.live_epoch.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        state.set_state(SessionState::Recording);
        push(&app);
    });
}

/// The keyboard goes back to the window he was dictating into, after he fixed words in the box.
fn hand_back_keyboard(app: &AppHandle, settle: bool) {
    let state: State<App> = app.state();
    #[cfg(windows)]
    {
        if let Some(w) = app.get_webview_window("composer") {
            win_surface::release_keyboard(&w);
        }
        if let Some(to) = *state.workplace.lock() {
            win_surface::give_back_foreground(to);
        }
    }
    #[cfg(target_os = "macos")]
    if let Some(pid) = *state.workplace.lock() {
        crate::destination::macos_paste::activate(pid);
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    let _ = &state;
    // A moment for that app to put its caret back in the field before anything is pasted.
    if settle {
        std::thread::sleep(std::time::Duration::from_millis(150));
    }
}

/// Remember what his fixes in the box say the recogniser gets wrong, while Learning is on.
fn learn_from(app: &AppHandle, recognised: &str, sent: &str) {
    if !app.state::<App>().settings.lock().learning {
        return;
    }
    let found = hvtt_core::learning::learn(recognised, sent);
    if found.is_empty() {
        return;
    }
    eprintln!("[hvtt] learned {} fix(es)", found.len());
    update_settings(app, |s| {
        for fix in found {
            hvtt_core::learning::remember(&mut s.fixes, fix);
        }
    });
}

/// Finish the dictation: recognise the last few seconds, then deliver - or, when he closed the
/// box instead of sending, only keep the words (draft and clipboard) and put the box away.
fn finish(app: AppHandle, deliver: bool) {
    std::thread::spawn(move || {
        let state: State<App> = app.state();
        let (from, before, rest) = {
            // A pause still finishing its words completes first.
            let _pass = state.live_pass.lock();
            if !state.session.lock().is_dictating() {
                return;
            }
            let Some(rec) = state.recording.lock().take() else { return };
            state.live_epoch.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            state.set_state(SessionState::Transcribing);
            let (from, before) = {
                let live = state.live.lock();
                (live.heard_upto, live.text.clone())
            };
            (from, before, rec.finish_from(from))
        };
        // The whole recording's length, for telling a mis-press from a silent one.
        let recorded = from + rest.len();
        *state.level.lock() = 0.0;
        *state.message.lock() = "Transcribing…".into();
        push(&app);

        let engine = state.engine.lock().clone();
        let started = std::time::Instant::now();
        let last = match engine {
            Some(e) => recognise(&e, &rest, &hints(&state, &before), None, false),
            None => Ok(None),
        };
        let elapsed = started.elapsed().as_millis();
        *state.elapsed_ms.lock() = elapsed;
        state.timings.lock().transcribe_ms = elapsed;
        let last = match last {
            Ok(words) => words,
            // The words already in the box are still sent; only the last stretch is missing.
            Err(why) if !before.trim().is_empty() => {
                eprintln!("[hvtt] last words not recognised: {why}");
                None
            }
            Err(why) => {
                *state.message.lock() = why.clone();
                state.set_state(SessionState::Error { message: why });
                push(&app);
                return;
            }
        };
        let (recognised, text, edited) = {
            let mut live = state.live.lock();
            if let Some(words) = &last {
                live.recognised = join_words(&live.recognised, words);
                live.text = join_words(&live.text, words);
            }
            live.tail.clear();
            (live.recognised.clone(), live.text.trim().to_string(), live.edited)
        };

        let had_keyboard = std::mem::take(&mut *state.box_has_keyboard.lock());
        if text.is_empty() {
            *state.message.lock() = if hvtt_core::audio::is_long_enough(
                recorded,
                hvtt_core::audio::WHISPER_SAMPLE_RATE,
            ) {
                "No speech was recognised in that recording.".into()
            } else {
                // A mis-press. Say so plainly and go back to resting without an error state.
                "That was too short to transcribe.".into()
            };
            if had_keyboard {
                hand_back_keyboard(&app, false);
            }
            state.set_state(SessionState::Ready);
            log_latency(&state);
            push(&app);
            hide_after(&app, std::time::Duration::from_millis(1800));
            return;
        }
        if edited {
            learn_from(&app, &recognised, &text);
        }
        if had_keyboard {
            hand_back_keyboard(&app, deliver);
        }
        deliver_words(&app, text, deliver);
    });
}

/// The product rule, in one call: draft to disk, then clipboard, then delivery. Nothing here
/// loses the text, and the paste rung relies on the clipboard copy.
fn deliver_words(app: &AppHandle, text: String, deliver: bool) {
    let state: State<App> = app.state();
    *state.transcript.lock() = Transcript::settled(text);

    let (choice, paste_key) = {
        let s = state.settings.lock();
        (s.clipboard, hvtt_core::settings::describe_shortcut(&s.paste_shortcut))
    };
    // One clipboard or the other, never both.
    let system = clip::SystemClipboard::new(app.clone());
    #[cfg(any(target_os = "macos", windows))]
    let huck = clip::huck::HuckClipboard;
    let clipboard: &dyn hvtt_core::pipeline::Clipboard = match choice {
        #[cfg(any(target_os = "macos", windows))]
        ClipboardChoice::Huck => &huck,
        _ => &system,
    };
    let transcript = state.transcript.lock().clone();
    let keep_drafts = state.settings.lock().keep_drafts;
    let deliver_started = std::time::Instant::now();
    let report = {
        let dest = state.destination.lock();
        hvtt_core::complete_transcription(
            &transcript,
            clipboard,
            dest.as_deref().filter(|_| deliver),
            state.drafts.as_ref().filter(|_| keep_drafts),
        )
    };
    state.timings.lock().deliver_ms = deliver_started.elapsed().as_millis();

    use hvtt_core::pipeline::{DeliveryError, DeliveryOutcome};
    let delivered = matches!(report.delivery, DeliveryOutcome::Delivered { .. });
    if let DeliveryOutcome::Failed { label, error, detail } = &report.delivery {
        eprintln!("[hvtt] not delivered to {label}: {error:?} ({})", detail.as_deref().unwrap_or("-"));
    }
    // Where the words wait, and the key that gets them back.
    let (kept, key) = match choice {
        ClipboardChoice::Huck => ("on Huck's clipboard", paste_key),
        ClipboardChoice::System => ("copied", NORMAL_PASTE.to_string()),
    };
    *state.message.lock() = match &report.delivery {
        _ if !report.clipboard_ok => report.message.clone(),
        DeliveryOutcome::NotAttempted => {
            let mut kept = kept.to_string();
            kept[..1].make_ascii_uppercase();
            format!("{kept} — press {key} to paste.")
        }
        DeliveryOutcome::Failed { label, error: DeliveryError::DestinationLost, .. } => {
            format!("Couldn't reach {label} — {kept}. Press {key} to paste.")
        }
        DeliveryOutcome::Failed { label, .. } => {
            format!("Couldn't type into {label} — {kept}. Press {key} to paste.")
        }
        _ => report.message.clone(),
    };
    *state.delivered.lock() = delivered;
    state.set_state(SessionState::Ready);
    log_latency(&state);
    push(app);
    // Closed rather than sent: the words are kept, and the box goes as he asked.
    if !deliver && report.clipboard_ok {
        dismiss(app.clone());
        return;
    }
    // No text box afterwards: the words are in the field, or on the clipboard. Say which,
    // briefly, and get out of the way. Two things hold the box up: a failed clipboard (the
    // words are only here and in the draft) and the one-time permission ask, which needs a
    // click.
    let hold = !report.clipboard_ok || *state.ask_permission.lock();
    if !hold {
        let ms = if delivered { 1200 } else { 2600 };
        hide_after(app, std::time::Duration::from_millis(ms));
    }
}

// ---------------------------------------------------------------------------- startup

/// Load the recognition model in the background so the window appears immediately.
fn spawn_engine_load(app: AppHandle) {
    std::thread::spawn(move || {
        let state: State<App> = app.state();
        let model_file = state.settings.lock().model.clone();

        let Some(path) = engine_whisper::WhisperEngine::expected_path(&model_file) else {
            *state.engine_status.lock() = "No application data directory available.".into();
            push(&app);
            return;
        };

        *state.engine_status.lock() = format!("Loading {model_file}…");
        push(&app);

        match engine_whisper::WhisperEngine::load(&path) {
            Ok(engine) => {
                *state.engine_status.lock() = engine.name().to_string();
                *state.engine.lock() = Some(Arc::new(engine));
            }
            Err(e) => {
                *state.engine_status.lock() = e.to_string();
            }
        }
        push(&app);
    });
}

pub fn run() {
    #[cfg(windows)]
    if !win_surface::claim_single_instance() {
        return;
    }
    let settings = Settings::load();

    let app_state = App {
        session: Mutex::new(SessionState::Idle),
        transcript: Mutex::new(Transcript::empty()),
        recording: Mutex::new(None),
        engine: Mutex::new(None),
        engine_status: Mutex::new("Starting…".into()),
        settings: Mutex::new(settings),
        drafts: DraftStore::at_default_location(),
        message: Mutex::new(String::new()),
        level: Mutex::new(0.0),
        shortcut_error: Mutex::new(None),
        paste_shortcut_error: Mutex::new(None),
        rebinding: Mutex::new(None),
        rebind_error: Mutex::new(None),
        #[cfg(windows)]
        key_capture: Mutex::new(None),
        update: Mutex::new(None),
        update_file: Mutex::new(None),
        menu_key: Mutex::new(String::new()),
        ax_trusted: Mutex::new(accessibility_ready()),
        elapsed_ms: Mutex::new(0),
        destination: Mutex::new(None),
        pin_note: Mutex::new(None),
        delivered: Mutex::new(false),
        ask_permission: Mutex::new(false),
        permission_asked: Mutex::new(false),
        generation: Mutex::new(0),
        timings: Mutex::new(Timings::default()),
        bridge: Bridge::new(),
        live: Mutex::new(Live::default()),
        live_pass: Mutex::new(()),
        live_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        box_has_keyboard: Mutex::new(false),
        #[cfg(any(windows, target_os = "macos"))]
        workplace: Mutex::new(None),
    };

    tauri::Builder::default()
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .build(),
        )
        .manage(app_state)
        .invoke_handler(tauri::generate_handler![
            get_snapshot,
            toggle,
            dismiss,
            get_settings,
            save_settings,
            drafts_dir,
            accessibility_ready,
            open_accessibility_settings,
            finish_rebind,
            open_update,
            box_input,
            pause_resume,
            send,
            edit_text,
            take_keyboard,
            set_learning,
        ])
        .setup(move |app| {
            // An accessory app has no Dock icon and never steals the foreground when a window
            // is shown. This single line is what makes rule 1 achievable on macOS.
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);
            #[cfg(target_os = "macos")]
            crate::destination::macos_paste::count_own_clicks();
            #[cfg(target_os = "macos")]
            if let Some(w) = app.get_webview_window("composer") {
                let handle = app.handle().clone();
                w.on_window_event(move |event| {
                    if let tauri::WindowEvent::Focused(true) = event {
                        box_became_key(&handle);
                    }
                });
            }
            // Windows has no such policy; the box carries never-activate styles instead.
            #[cfg(windows)]
            if let Some(w) = app.get_webview_window("composer") {
                win_surface::prepare(&w);
            }
            // Windows: know where he was working, for when a menu item hands the foreground back.
            #[cfg(windows)]
            win_surface::follow_foreground();

            {
                let handle = app.handle().clone();
                bind_all(&handle);
                // The box starts hidden, so an error only it can show was a silent failure: the
                // app looked like it had not started at all.
                if app.state::<App>().shortcut_error.lock().is_some() {
                    reveal_composer(&handle);
                }
            }

            // The menu-bar H holds every setting, as in Snip 'n' Clip. A template image, so
            // macOS tints it to match the menu bar like every other item there. Windows does not
            // tint tray icons, so there it is Snip 'n' Clip's tray gray, which reads on a light
            // or a dark taskbar.
            {
                use tauri::tray::{TrayIconBuilder, TrayIconEvent};
                let menu = build_menu(app.handle())?;
                *app.state::<App>().menu_key.lock() = menu_key(&app.state::<App>());
                #[cfg(windows)]
                let icon = tauri::include_image!("icons/tray-win.png");
                #[cfg(not(windows))]
                let icon = tauri::include_image!("icons/tray@2x.png");
                TrayIconBuilder::with_id(TRAY)
                    .menu(&menu)
                    .show_menu_on_left_click(true)
                    .icon(icon)
                    .icon_as_template(true)
                    .tooltip("Huck's Voice to Text")
                    .on_tray_icon_event(|tray, event| {
                        // The pointer reaching the H is the moment to re-ask about Accessibility,
                        // so a grant made in System Settings shows before the menu opens.
                        if let TrayIconEvent::Enter { .. } = event {
                            let app = tray.app_handle();
                            let state: State<App> = app.state();
                            *state.ax_trusted.lock() = accessibility_ready();
                            refresh_menu(app);
                        }
                    })
                    .build(app)?;
                app.handle().on_menu_event(|app, event| on_menu(app, event.id.as_ref()));
            }

            // Listen for the browser extension's native-messaging host. No port, no polling:
            // it is a Unix socket in the app-data directory that only this user can open - on
            // Windows too, which has had them since Windows 10 1803.
            {
                let state: State<App> = app.state();
                if let Err(e) = state.bridge.serve() {
                    eprintln!("[hvtt] browser bridge unavailable: {e}");
                }
            }

            // The window server's first answer costs ~45 ms; pay it now, not on the first keypress.
            #[cfg(target_os = "macos")]
            std::thread::spawn(crate::destination::macos_paste::warm_up);

            // Back from an update this program started (the installer passes --updated): the box
            // says so, then leaves, as it does for "You're up to date".
            if std::env::args().any(|a| a == "--updated") {
                let handle = app.handle().clone();
                let version = handle.package_info().version.to_string();
                show_update(
                    &handle,
                    "current",
                    format!("Updated to version {version}"),
                    "Your settings are as you left them.".into(),
                );
                hide_after(&handle, std::time::Duration::from_millis(3000));
            }
            // The installer did not finish, and the update's watcher started this copy again.
            if std::env::args().any(|a| a == "--update-failed") {
                let handle = app.handle().clone();
                let version = handle.package_info().version.to_string();
                show_update(
                    &handle,
                    "failed",
                    "The update didn't finish".into(),
                    format!("You're still on version {version}. Check for Updates to try again."),
                );
                hide_after(&handle, std::time::Duration::from_millis(6000));
            }

            spawn_engine_load(app.handle().clone());
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running Huck's Voice to Text");
}
