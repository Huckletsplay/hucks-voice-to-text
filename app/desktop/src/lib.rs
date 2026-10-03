//! Huck's Voice to Text — desktop application.
//!
//! Hotkey -> record -> transcribe locally -> clipboard -> the text box that was focused at the
//! keypress. The clipboard copy always happens first; it is the safeguard when the box is gone.

pub mod bridge;
pub mod clip;
pub mod destination;
pub mod engine_whisper;
pub mod login_item;
#[cfg(target_os = "macos")]
mod mac_exit;
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
    /// Words waiting to be recognised again, said in the box while it lasts.
    live_trouble: Option<String>,
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
    /// Shortcut pressed -> the first sound from the microphone. Anything he said before it is
    /// not in the recording.
    shortcut_to_first_sound_ms: u128,
    #[serde(skip)]
    pressed: Option<std::time::Instant>,
    /// Recognition wall time.
    transcribe_ms: u128,
    /// Clipboard + delivery.
    deliver_ms: u128,
}

struct App {
    session: Mutex<SessionState>,
    transcript: Mutex<Transcript>,
    recording: Mutex<Option<recorder::Recording>>,
    /// The next dictation's microphone, built and waiting (`recorder::Prepared`).
    prepared: Mutex<Option<recorder::Prepared>>,
    engine: Mutex<Option<Arc<dyn Transcriber>>>,
    engine_status: Mutex<String>,
    /// The smaller model the speed check offered, waiting for his answer.
    /// The speed check's offer, with the model change it was measured under (`model_changes`).
    offered_model: Mutex<Option<(&'static hvtt_core::models::Model, u64)>>,
    /// Counts model changes, which happen only while holding it - the speed check holds it too
    /// while it decides and shows its advice, so no change slips in between (Codex's 18th review).
    model_changes: Mutex<u64>,
    /// The speech model being downloaded or loaded - one at a time, reserved the moment it is
    /// chosen and held until it is the one in use and remembered. Two loads at once could leave
    /// one model ticked and another running (Codex's review, 2026-09-30).
    model_work: Mutex<Option<ModelWork>>,
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
    /// The last words are neither on a clipboard nor in a draft - only in the box. Quit waits for
    /// him to take them (Codex's fourth review of 0.1.6). Cleared when a new dictation starts.
    words_unsaved: Mutex<bool>,
    /// Serialises the logout's early copy with the start of the final copy.
    #[cfg(target_os = "macos")]
    final_copy_started: Mutex<bool>,
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
    /// A window here could not be settled for sure (`Settled::Unsure`): no more windows are tried
    /// from this point - the stop press or a pause recognises all of it, so nothing is skipped.
    hold_at: Option<usize>,
    /// Seconds of this dictation that held speech but gave no words, added up from every pause
    /// and the stop - never cleared by a later success, so the end can say so (Codex's eighth
    /// review).
    unrecognised_secs: f32,
    /// Words that could not be recognised yet. They are kept - `heard_upto` does not move past
    /// them - and tried again by the next pass, Resume or the stop press; the box says so
    /// meanwhile (Codex's review, 2026-09-30).
    trouble: Option<String>,
}

/// What the box says while words are waiting to be recognised again.
const TROUBLE: &str = "Some words aren't recognised yet — trying again";

/// What is happening to a speech model: H › Settings › Speech Model.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ModelWork {
    file: String,
    /// Still downloading; otherwise loading.
    downloading: bool,
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
            live_trouble: live.trouble,
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
        "[hvtt] latency: shortcut->visible {}ms | shortcut->capture {}ms | shortcut->first sound {}ms | transcribe {}ms | deliver {}ms",
        t.shortcut_to_visible_ms,
        t.shortcut_to_capture_ms,
        t.shortcut_to_first_sound_ms,
        t.transcribe_ms,
        t.deliver_ms
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
    change_settings(&state.settings, change, |s| {
        if let Err(e) = s.save() {
            eprintln!("[hvtt] settings not saved: {e}");
        }
    });
}

/// Change settings in one turn - from the latest, saved, and kept, all under the lock - so two
/// changes at once can never undo each other. Copying, saving and putting back separately let a
/// model finishing loading put "Keep Last 5 Recordings" back on after he had switched it off, or
/// drop a fix just learned (Codex's fourteenth review).
fn change_settings(settings: &Mutex<Settings>, change: impl FnOnce(&mut Settings), save: impl FnOnce(&Settings)) {
    let mut s = settings.lock();
    change(&mut s);
    save(&s);
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
    /// checking | current | downloading | ready | failed | offer
    stage: &'static str,
    title: String,
    detail: String,
    /// "offer" only: the button's words ("Switch to Quick").
    action: Option<String>,
}

fn show_update(app: &AppHandle, stage: &'static str, title: String, detail: String) {
    let state: State<App> = app.state();
    *state.update.lock() = Some(UpdateView { stage, title, detail, action: None });
    // Dictation outranks this: it waits in the state and shows when the box is next free.
    if !matches!(*state.session.lock(), SessionState::Recording | SessionState::Paused | SessionState::Transcribing) {
        reveal_composer(app);
    }
    push(app);
}

/// Put away an update or speech-model message after `delay` - only if that same message is still
/// the one waiting and nothing else has the box. A dictation that started or ended meanwhile is
/// never dismissed by it, above all one whose words could be kept nowhere but the box: `hide_after`
/// went by the generation alone, so a model download finishing mid-dictation could clear those
/// words later (Codex's review, 2026-09-30).
fn hide_update_after(app: &AppHandle, delay: std::time::Duration) {
    let state: State<App> = app.state();
    let Some(shown) = state.update.lock().as_ref().map(|v| (v.stage, v.title.clone())) else {
        return;
    };
    let app = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(delay);
        let state: State<App> = app.state();
        // One lock at a time: `snapshot` holds the session while it reads the others.
        let idle = matches!(*state.session.lock(), SessionState::Idle);
        let same = state
            .update
            .lock()
            .as_ref()
            .is_some_and(|v| v.stage == shown.0 && v.title == shown.1);
        if idle && same {
            dismiss(app.clone());
        }
    });
}

/// Settings › Check for Updates…. Asks GitHub only now, when he chooses it.
fn check_for_updates(app: &AppHandle) {
    check_for_updates_how(app, false);
}

/// `quiet`: the check made when the program opens (H › Settings › Check for Updates When It
/// Opens) - nothing is shown unless a newer version is found; up to date, offline or failed, it
/// stays silent.
fn check_for_updates_how(app: &AppHandle, quiet: bool) {
    let state: State<App> = app.state();
    // One check (and its download) at a time, whatever the box shows - a quiet check shows nothing
    // (Codex's seventeenth review: two at once shared one download folder).
    if UPDATE_WORK.swap(true, std::sync::atomic::Ordering::SeqCst) {
        if !quiet {
            reveal_composer(app);
        }
        return;
    }
    if matches!(state.update.lock().as_ref(), Some(v) if matches!(v.stage, "checking" | "downloading" | "installing")) {
        UPDATE_WORK.store(false, std::sync::atomic::Ordering::SeqCst);
        if !quiet {
            reveal_composer(app);
        }
        return;
    }
    *state.update_file.lock() = None;
    #[cfg(windows)]
    UPDATE_WARNED.store(false, std::sync::atomic::Ordering::SeqCst);
    if !quiet {
        show_update(app, "checking", "Checking for updates…".into(), "Asking GitHub.".into());
    }
    let app = app.clone();
    std::thread::spawn(move || {
        let _work = Released(&UPDATE_WORK);
        let current = app.package_info().version.to_string();
        let fail = |why: String| {
            if quiet {
                eprintln!("[hvtt] update check on opening: {why}");
            } else {
                show_update(&app, "failed", "Couldn't update".into(), why)
            }
        };
        match update::fetch_latest(&current) {
            Err(why) => fail(why),
            Ok(update::Check::UpToDate { .. }) if quiet => {}
            Ok(update::Check::UpToDate { .. }) => {
                show_update(
                    &app,
                    "current",
                    "You're up to date".into(),
                    format!("Version {current} is the latest."),
                );
                hide_update_after(&app, std::time::Duration::from_millis(2600));
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
        // On Windows Huck's Clipboard lives in this program's memory, and may hold the only copy of
        // his last words: say so before an update empties it. The second Open Update goes ahead.
        let held_at_click = crate::clip::huck::read();
        if huck_clipboard_at_risk() && !UPDATE_WARNED.load(std::sync::atomic::Ordering::SeqCst) {
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
            if huck_clipboard_at_risk() && crate::clip::huck::read() != held_at_click {
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

/// Would the update lose words? Whenever Huck's Clipboard holds some. It lives in this program's
/// memory, and whether a recovery draft of those very words was written cannot be told from the
/// setting (drafts can be switched on afterwards, or a write can fail), so this does not try.
/// (Codex's second review of 0.1.5.)
#[cfg(windows)]
fn huck_clipboard_at_risk() -> bool {
    crate::clip::huck::read().is_some_and(|t| !t.trim().is_empty())
}

#[cfg(windows)]
fn warn_huck_clipboard(app: &AppHandle) {
    UPDATE_WARNED.store(true, std::sync::atomic::Ordering::SeqCst);
    show_update(
        app,
        "ready",
        "Paste what you need first".into(),
        "Updating empties Huck's Clipboard. Paste anything you still need from it, then choose \
         Open Update again."
            .into(),
    );
}

/// Start the verified installer so that an update can never leave him without the program: a
/// hidden PowerShell waits for it, and if it did not finish - any exit code but 0 - starts this copy
/// again with --update-failed. The script travels encoded, so no path is ever parsed as syntax.
#[cfg(windows)]
fn start_installer(installer: &std::path::Path) -> std::io::Result<()> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let exe = std::env::current_exe()?;
    let script = update::installer_script(&installer.display().to_string(), &exe.display().to_string());
    let root = std::env::var_os("SystemRoot").map(std::path::PathBuf::from);
    let powershell = root
        .unwrap_or_else(|| r"C:\Windows".into())
        .join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
    std::process::Command::new(powershell)
        .args(update::powershell_args(&script))
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()?;
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
        "{}|{}|{}|{:?}|{}|{}|{}|{:?}|{:?}|{:?}|{}|{:?}|{:?}|{}|{:?}|{}|{}",
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
        s.model,
        state.model_work.lock(),
        s.careful_stop,
        s.check_updates_on_start,
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
    // Same plain words: what happens, and what it costs.
    let stop = Submenu::with_id(app, "stop-talking", "When I Stop Talking", true)?;
    stop.append(&check("stop-quick", "Finish Quickly", !s.careful_stop)?)?;
    stop.append(&check(
        "stop-careful",
        "Wait a Moment Longer — safest for a soft last word",
        s.careful_stop,
    )?)?;
    settings.append(&stop)?;
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
    // Quick, Better, Best: what each would cost is in its line. One download at a time, and not
    // mid-dictation.
    let work = state.model_work.lock().clone();
    let chosen = hvtt_core::models::find(&s.model);
    let speech = Submenu::with_id(
        app,
        "speech-model",
        format!("Speech Model — {}", chosen.map_or("your own", |m| m.name)),
        !recording,
    )?;
    for (i, m) in hvtt_core::models::MODELS.iter().enumerate() {
        let label = match &work {
            Some(w) if w.file == m.file && !w.downloading => {
                format!("{} — getting ready… · {}", m.name, hvtt_core::models::megabytes(m.bytes))
            }
            Some(w) => hvtt_core::models::menu_label(m, model_on_this_computer(m), w.file == m.file),
            None => hvtt_core::models::menu_label(m, model_on_this_computer(m), false),
        };
        speech.append(&CheckMenuItem::with_id(
            app,
            format!("model-{i}"),
            label,
            work.is_none(),
            chosen == Some(m),
            None::<&str>,
        )?)?;
    }
    if chosen.is_none() {
        // A model he put in the folder himself: shown, so the menu never claims another.
        let own = s.model.trim_start_matches("ggml-").trim_end_matches(".bin");
        speech.append(&CheckMenuItem::with_id(app, "model-own", own, false, true, None::<&str>)?)?;
    }
    settings.append(&speech)?;
    settings.append(&PredefinedMenuItem::separator(app)?)?;
    settings.append(&item("speed-check", "Check This Computer's Speed".into(), !recording && state.model_work.lock().is_none())?)?;
    settings.append(&item("check-updates", "Check for Updates…".into(), !recording)?)?;
    settings.append(&check("updates-on-start", "Check for Updates When It Opens", s.check_updates_on_start)?)?;
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
                let _ = app.run_on_main_thread(move || toggle_by(again, false));
            });
        }
        #[cfg(not(windows))]
        "dictate" => toggle_by(app.clone(), false),
        "allow-ax" => open_accessibility_settings(),
        "check-updates" => check_for_updates(app),
        "speed-check" => speed_check(app, true),
        "updates-on-start" => update_settings(app, |s| s.check_updates_on_start = !s.check_updates_on_start),
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
        "stop-quick" => update_settings(app, |s| s.careful_stop = false),
        "stop-careful" => update_settings(app, |s| s.careful_stop = true),
        "forget-all" => update_settings(app, |s| s.fixes.clear()),
        other if other.starts_with("model-") => {
            if let Some(model) =
                other["model-".len()..].parse::<usize>().ok().and_then(|i| hvtt_core::models::MODELS.get(i))
            {
                choose_model(app, model, None);
            }
        }
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
        "quit" => quit(app.clone()),
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

/// Quit - but on macOS only once his clipboard is not on loan (Codex's reviews of 0.1.6). A paste
/// on Huck's clipboard has his own clipboard for half a second, and leaving in that time would
/// leave the pasted words on it instead. From here on no paste may borrow it (one already waiting
/// gives up), the borrow on loan is waited for - on a worker, so the menu stays alive - and only
/// then does the program exit. If it cannot be given back within a generous ten seconds (a stuck
/// restore), the program does **not** exit and pasting works again: choose Quit once more.
fn quit(app: AppHandle) {
    #[cfg(target_os = "macos")]
    {
        use std::sync::atomic::Ordering;
        // One Quit at a time: a second choice while the first still waits would race its
        // `resume_borrowing`, reopening the gate under the other.
        if QUITTING.swap(true, Ordering::SeqCst) {
            return;
        }
        std::thread::spawn(move || {
            let kept = keep_everything_before_exit(&app, std::time::Duration::from_secs(20));
            // The words could be neither copied nor drafted: they are only in the box, which
            // says so ("copy it before closing"). The first Quit brings the box forward instead
            // of leaving; a second one, after he has seen it, goes. (Codex's fourth review.)
            let state: State<App> = app.state();
            let unsaved = *state.words_unsaved.lock();
            let generation = *state.generation.lock();
            let warned = *QUIT_WARNED_GENERATION.lock();
            match mac_exit::quit_decision(kept, unsaved, warned, generation) {
                mac_exit::QuitDecision::Exit => {
                    EXIT_ALLOWED.store(true, Ordering::SeqCst);
                    app.exit(0);
                    return;
                }
                mac_exit::QuitDecision::Warn => {
                    eprintln!("[hvtt] not quitting yet: his last words are only in the box");
                    reveal_composer(&app);
                    push(&app);
                    *QUIT_WARNED_GENERATION.lock() = Some(generation);
                }
                mac_exit::QuitDecision::Stay => {
                    eprintln!("[hvtt] not quitting yet: a dictation or his clipboard is not safe yet");
                }
            }
            clip::huck::resume_borrowing();
            QUITTING.store(false, Ordering::SeqCst);
        });
    }
    #[cfg(not(target_os = "macos"))]
    app.exit(0);
}

/// A Quit is under way (macOS): no new dictation may start, and the shortcut is ignored.
#[cfg(target_os = "macos")]
static QUITTING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The generation whose unsaved words a Quit has already revealed. A timeout does not warn.
#[cfg(target_os = "macos")]
static QUIT_WARNED_GENERATION: Mutex<Option<u64>> = Mutex::new(None);

/// The end is coming and cannot be put off (a logout): before anything slow, put the words
/// already recognised where the finished ones would go - the chosen clipboard, and a draft if
/// drafts are on. If the last stretch then finishes in time it replaces them; if not, only that
/// last stretch is lost, never the whole dictation. (Codex's fourth review of 0.1.6.) Audio is
/// never written to disk.
#[cfg(target_os = "macos")]
fn keep_words_so_far(app: &AppHandle) {
    let state: State<App> = app.state();
    let order = mac_exit::CopyOrder::new(&state.final_copy_started);
    // Hold through the snapshot and its write: a final copy waits for an early one, and an
    // early copy arriving after the final one has begun is skipped.
    let Some(_early_copy) = order.early() else { return };
    let busy = {
        let session = state.session.lock();
        session.is_dictating() || matches!(*session, SessionState::Transcribing)
    };
    let text = state.live.lock().text.trim().to_string();
    if !busy || text.is_empty() {
        return;
    }
    let (choice, keep_drafts) = {
        let s = state.settings.lock();
        (s.clipboard, s.keep_drafts)
    };
    let system = clip::SystemClipboard::new(app.clone());
    let huck = clip::huck::HuckClipboard;
    let clipboard: &dyn hvtt_core::pipeline::Clipboard = match choice {
        ClipboardChoice::Huck => &huck,
        _ => &system,
    };
    let report = hvtt_core::complete_transcription(
        &Transcript::settled(text),
        clipboard,
        None,
        state.drafts.as_ref().filter(|_| keep_drafts),
    );
    eprintln!("[hvtt] ending: the words so far were kept ({})", report.text_is_safe());
}

/// `keep_everything_before_exit` has finished: an exit request may now go through.
#[cfg(target_os = "macos")]
static EXIT_ALLOWED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Before the program leaves, nothing of his may be only in flight (Codex's third review of
/// 0.1.6; the product rule). In order, within `limit`:
/// 1. a dictation still going - listening or paused - is finished the way closing the box finishes
///    it: the last words recognised, then drafted and copied, **not delivered**;
/// 2. that, or a delivery already under way, is waited for;
/// 3. no paste may borrow his clipboard any more, and one on loan is given back (`stop_borrowing`).
///
/// `true` when all of it is done. Callers set `QUITTING` first, so no new dictation starts.
#[cfg(target_os = "macos")]
fn keep_everything_before_exit(app: &AppHandle, limit: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + limit;
    let state: State<App> = app.state();
    if state.session.lock().is_dictating() {
        finish(app.clone(), false);
    }
    loop {
        let busy = {
            let session = state.session.lock();
            session.is_dictating() || matches!(*session, SessionState::Transcribing)
        };
        if !busy {
            break;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    clip::huck::stop_borrowing(deadline.saturating_duration_since(std::time::Instant::now()))
}

/// The dictation shortcut: start, or - listening or paused - send.
#[tauri::command]
fn toggle(app: AppHandle) {
    toggle_by(app, true);
}

/// `by_key`: a key press ended the dictation, which the paste gate expects to have counted. The H
/// menu's Start/Stop item is a mouse click - no key was pressed - so it says `false`, as the Send
/// button does; otherwise the gate waits for a stop press that never comes and only copies the
/// words (Codex's review of 0.1.6).
fn toggle_by(app: AppHandle, by_key: bool) {
    let state: State<App> = app.state();
    if state.rebinding.lock().is_some() {
        return;
    }
    // Quitting: the dictation, if any, is being finished and kept; nothing new may start.
    #[cfg(target_os = "macos")]
    if QUITTING.load(std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    let (dictating, busy) = {
        let session = state.session.lock();
        (session.is_dictating(), matches!(*session, SessionState::Transcribing))
    };
    if dictating {
        crate::destination::box_input::set_stopped_by_key(by_key);
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
            .is_some_and(|r| hvtt_core::audio::hear_speech(&r.peek_from(from)) != hvtt_core::audio::Heard::Silence)
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
    if state.recording.lock().take().is_some() {
        prepare_microphone(&app);
    }
    *state.level.lock() = 0.0;
    *state.delivered.lock() = false;
    *state.ask_permission.lock() = false;
    if !matches!(state.update.lock().as_ref(), Some(v) if matches!(v.stage, "checking" | "downloading" | "installing")) {
        *state.update.lock() = None;
    }
    *state.transcript.lock() = Transcript::empty();
    *state.words_unsaved.lock() = false;
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
    let mut saved = Ok(());
    change_settings(&state.settings, |s| *s = settings, |s| saved = s.save().map_err(|e| e.to_string()));
    push(&app);
    saved
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

    // The microphone starts opening now, on its own thread, alongside everything below: opening it
    // is the slowest thing here, and whatever he says before it runs is lost. Measured 2026-09-30
    // on quick phrases: missing the first 0.12 s took base.en from 6% of words wrong to 23%.
    let device = state.settings.lock().input_device.clone();
    let prepared = state.prepared.lock().take();
    let microphone = std::thread::spawn(move || open_microphone(prepared, device.as_deref()));

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
    *state.words_unsaved.lock() = false;
    #[cfg(target_os = "macos")]
    {
        *state.final_copy_started.lock() = false;
    }
    *state.ask_permission.lock() = false;
    // A finished update message gives way; a check still running re-shows itself when done.
    if !matches!(state.update.lock().as_ref(), Some(v) if matches!(v.stage, "checking" | "downloading" | "installing")) {
        *state.update.lock() = None;
    }
    state.set_state(SessionState::Recording);
    // Counted again now it shows as recording: a speed check that found nothing happening read the
    // count before this, so it is given up (Codex's eighteenth review).
    state.live_epoch.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    *state.message.lock() = "Listening…".into();
    *state.elapsed_ms.lock() = 0;
    *state.pin_note.lock() = None;
    *state.destination.lock() = None;
    reveal_composer(&app);
    {
        let mut t = state.timings.lock();
        t.pressed = Some(pressed);
        t.shortcut_to_visible_ms = pressed.elapsed().as_millis();
    }
    push(&app);

    // 2. The microphone, opening since the keypress, so the first words are not lost while the
    //    destination is worked out.
    let opened = microphone
        .join()
        .unwrap_or_else(|_| Err("The microphone could not be opened.".to_string()));
    match opened {
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

/// The microphone for a new dictation: the prepared one when it is still the right device and
/// starts giving sound at once, otherwise opened from cold. A prepared stream that gives nothing
/// within a quarter of a second (the computer slept, say) is dropped for a cold one - better a
/// slower start than a dictation that captured nothing.
fn open_microphone(
    prepared: Option<recorder::Prepared>,
    device: Option<&str>,
) -> Result<recorder::Recording, String> {
    if let Some(prepared) = prepared.filter(|p| p.still_current(device)) {
        let started = std::time::Instant::now();
        match prepared.start() {
            Ok(rec) => {
                while rec.first_sound().is_none() && started.elapsed() < std::time::Duration::from_millis(250) {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                if rec.first_sound().is_some() {
                    eprintln!("[hvtt] microphone: prepared, sound after {} ms", started.elapsed().as_millis());
                    return Ok(rec);
                }
                eprintln!("[hvtt] microphone: prepared stream gave no sound; opening it again");
            }
            Err(e) => eprintln!("[hvtt] microphone: prepared stream would not start ({e})"),
        }
    }
    let started = std::time::Instant::now();
    let rec = recorder::Recording::start(device)?;
    eprintln!("[hvtt] microphone: opened cold in {} ms", started.elapsed().as_millis());
    Ok(rec)
}

/// Build the next dictation's microphone now, while nothing is happening, so the keypress only
/// starts it. Only after a dictation (so never before macOS has been asked for the microphone),
/// and not at all with `HVTT_COLD_MIC` set (the comparison, 2026-10-02).
fn prepare_microphone(app: &AppHandle) {
    // Windows shows its own "using your microphone" sign; whether a built-but-stopped stream
    // lights it is not yet checked on the PC, so Windows opens the microphone at the press, as
    // before, until it is.
    if cfg!(windows) || std::env::var_os("HVTT_COLD_MIC").is_some() {
        return;
    }
    let app = app.clone();
    std::thread::spawn(move || {
        let state: State<App> = app.state();
        let device = state.settings.lock().input_device.clone();
        match recorder::Prepared::new(device.as_deref()) {
            Ok(prepared) => *state.prepared.lock() = Some(prepared),
            Err(e) => eprintln!("[hvtt] next microphone not prepared: {e}"),
        }
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
    use crate::destination::macos_paste::{PasteDestination, SameWindow};
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
    // how they behave when Accessibility asks them anything: paste, gated. Every Chromium window
    // gets the caret rule, as on Windows: after a click in the same window the paste goes where
    // the caret is - a browser's address bar included (decided with him 2026-09-29) - whichever
    // box Accessibility happens to name.
    if is_chromium_executable(&exe) || is_unsupported_executable(&exe).is_some() {
        match paste().map(|p| p.forgiving(SameWindow::Caret)) {
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
    let speech = hvtt_core::audio::hear_speech(samples);
    if samples.len() < hvtt_core::audio::WHISPER_SAMPLE_RATE as usize / 4
        || speech == hvtt_core::audio::Heard::Silence
    {
        return Ok(None);
    }
    let req = TranscriptionRequest {
        samples: samples.to_vec(),
        vocabulary_prompt: hints.prompt.clone(),
        provisional,
        give_up,
    };
    let result = engine.transcribe(&req).map_err(|e| e.to_string())?;
    let heard = hvtt_core::engine::without_ellipses(&hvtt_core::engine::confident(&result, speech));
    let text = hvtt_core::learning::apply(heard.trim(), &hints.fixes);
    Ok((!text.trim().is_empty()).then_some(text))
}

/// Recognise one full window for good by the rolling rule (`hvtt_core::engine::window_step`): its
/// words and how far they reach (16 kHz samples), or `None` when the window is held whole for the
/// stop. `hints` already carry the words before it.
fn recognise_window(
    engine: &Arc<dyn Transcriber>,
    samples: &[f32],
    hints: &Hints,
    give_up: Option<hvtt_core::engine::GiveUp>,
) -> Result<Option<(Option<String>, usize)>, String> {
    let mut hear = |audio: &[f32], _before: &str| hear_for_good(engine, audio, hints.prompt.clone(), give_up.clone());
    match hvtt_core::engine::window_step(samples, "", &mut hear)? {
        hvtt_core::engine::Window::Keep { text, upto } => {
            let words = hvtt_core::learning::apply(text.trim(), &hints.fixes);
            Ok(Some(((!words.trim().is_empty()).then_some(words), upto)))
        }
        hvtt_core::engine::Window::Hold => Ok(None),
    }
}

/// One recognition whose words are kept. Too short to be speech is nothing heard, not an error.
fn hear_for_good(
    engine: &Arc<dyn Transcriber>,
    audio: &[f32],
    prompt: Option<String>,
    give_up: Option<hvtt_core::engine::GiveUp>,
) -> Result<hvtt_core::engine::TranscriptionResult, String> {
    if audio.len() < hvtt_core::audio::WHISPER_SAMPLE_RATE as usize / 4 {
        return Ok(hvtt_core::engine::TranscriptionResult { text: String::new(), sentences: Vec::new(), elapsed_ms: 0 });
    }
    let req = TranscriptionRequest { samples: audio.to_vec(), vocabulary_prompt: prompt, provisional: false, give_up };
    engine.transcribe(&req).map_err(|e| e.to_string())
}

/// What the live windows had not kept, at the stop. Up to a window, heard in one piece; longer
/// (Live Words off, or a window held), a window at a time by the same rolling rule
/// (`hvtt_core::engine::recognise_all`) - never handed whole to Whisper's own long-form, which
/// dropped a hundred words of a minute under hiss (2026-10-01).
struct Rest {
    /// The words recognised, his fixes applied.
    words: Option<String>,
    /// Seconds that held speech but gave no words.
    unrecognised_secs: f32,
    /// Recognition failed this far in (16 kHz samples), and why: `words` are everything before.
    failed: Option<(usize, String)>,
}

/// What was not yet recognised for good, at a pause or the stop: window by window
/// (`hvtt_core::engine::recognise_all`), and a failure part-way keeps the words before it and tries
/// only the rest once more (Codex's eighth review).
fn recognise_rest(engine: &Arc<dyn Transcriber>, samples: &[f32], state: &App, before: &str) -> Rest {
    let fixes = hints(state, before).fixes;
    let mut hear = |audio: &[f32], so_far: &str| hear_for_good(engine, audio, hints(state, so_far).prompt, None);
    let (mut all, mut failed) = hvtt_core::engine::recognise_all(samples, before, &mut hear);
    if let (Some(at), Some(why)) = (all.unfinished_from, failed.take()) {
        eprintln!("[hvtt] recognition failed part-way, trying the rest once more: {why}");
        let so_far = join_words(before, &all.text);
        let (again, still) = hvtt_core::engine::recognise_all(&samples[at..], &so_far, &mut hear);
        all.text = join_words(&all.text, &again.text);
        all.unrecognised_secs += again.unrecognised_secs;
        all.unfinished_from = again.unfinished_from.map(|more| at + more);
        failed = still;
    }
    // `recognise_all`'s words begin after `before`.
    let text = hvtt_core::learning::apply(all.text.trim(), &fixes);
    Rest {
        words: (!text.trim().is_empty()).then_some(text),
        unrecognised_secs: all.unrecognised_secs,
        failed: all.unfinished_from.zip(failed),
    }
}

/// What the box says when speech gave no words, even asked twice.
fn unrecognised_note(secs: f32) -> String {
    format!("About {:.0} s of what you said couldn't be recognised.", secs.max(1.0))
}

/// Recognise while he talks, so the box fills in and the stop press has only the last window left
/// to do. Every 0.3 s or so: a full window (`WINDOW_SECS`) waiting is recognised for good by the
/// rolling rule (`recognise_window`); what is not yet kept is recognised provisionally and shown
/// fainter.
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

        // A full window waiting (`WINDOW_SECS`): recognised for good, with full care, up to its
        // last whole sentence. A recognition that fails (rather than hearing nothing) moves
        // nothing on, so the window is tried again.
        let window = (hvtt_core::engine::WINDOW_SECS * hvtt_core::audio::WHISPER_SAMPLE_RATE as f32) as usize;
        let held = state.live.lock().hold_at == Some(from);
        let mut unsure = false;
        let (cut, settled, failed) = if rest.len() >= window && !held {
            match recognise_window(&engine, &rest[..window], &hints(&state, &before), give_up()) {
                Ok(Some((words, cut))) => (cut, words, None),
                Ok(None) => {
                    unsure = true;
                    (0, None, None)
                }
                Err(why) => (0, None, Some(why)),
            }
        } else {
            (0, None, None)
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
            if unsure {
                eprintln!("[hvtt] a window could not be settled for sure; kept whole for the stop");
                live.hold_at = Some(from);
            }
            if let Some(words) = &settled {
                live.recognised = join_words(&live.recognised, words);
                live.text = join_words(&live.text, words);
            }
            live.tail = tail.unwrap_or_default();
            if let Some(why) = &failed {
                eprintln!("[hvtt] a stretch was not recognised, kept to try again: {why}");
                live.trouble = Some(TROUBLE.into());
            } else if cut > 0 {
                live.trouble = None;
            }
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
        // The same bounded way as Send (`recognise_rest`): window by window, never Whisper's
        // long-form; words recognised before a failure are kept, and only the rest waits.
        let heard = match engine {
            Some(e) => recognise_rest(&e, &rest, &state, &before),
            None => Rest { words: None, unrecognised_secs: 0.0, failed: Some((0, "no speech model is loaded".into())) },
        };
        {
            let mut live = state.live.lock();
            live.heard_upto = from + heard.failed.as_ref().map_or(rest.len(), |(at, _)| *at);
            if let Some(words) = &heard.words {
                live.recognised = join_words(&live.recognised, words);
                live.text = join_words(&live.text, words);
            }
            live.unrecognised_secs += heard.unrecognised_secs;
            live.trouble = if let Some((_, why)) = &heard.failed {
                // Not skipped: Resume's next pass, or Send, recognises the rest again.
                eprintln!("[hvtt] the words before the pause were not all recognised, kept: {why}");
                Some(TROUBLE.into())
            } else {
                (live.unrecognised_secs > 0.0).then(|| unrecognised_note(live.unrecognised_secs))
            };
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
    {
        let workplace = *state.workplace.lock();
        if let Some(pid) = workplace {
            crate::destination::macos_paste::activate(pid);
            // Activation is asynchronous, and the paste gate refuses unless that app really holds
            // the keyboard again (Codex's review of 0.1.6): wait for it, briefly, not hope.
            if settle {
                crate::destination::macos_paste::wait_for_keyboard(
                    pid,
                    std::time::Duration::from_millis(500),
                );
            }
        }
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
        let (from, before, rec, listening) = {
            // A pause still finishing its words completes first.
            let _pass = state.live_pass.lock();
            let listening = {
                let session = state.session.lock();
                if !session.is_dictating() {
                    return;
                }
                matches!(*session, SessionState::Recording)
            };
            let Some(rec) = state.recording.lock().take() else { return };
            {
                let mut t = state.timings.lock();
                t.shortcut_to_first_sound_ms = match (t.pressed, rec.first_sound()) {
                    (Some(pressed), Some(first)) => first.saturating_duration_since(pressed).as_millis(),
                    _ => 0,
                };
            }
            state.live_epoch.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            state.set_state(SessionState::Transcribing);
            let (from, before) = {
                let live = state.live.lock();
                (live.heard_upto, live.text.clone())
            };
            (from, before, rec, listening)
        };
        // Pressed while the last word was still being said: the microphone stays open until he
        // has stopped (`wait_for_quiet`). Paused, it is already closed.
        if listening {
            let careful = state.settings.lock().careful_stop;
            wait_for_quiet(&rec, careful);
        }
        let rest = rec.finish_from(from);
        prepare_microphone(&app);
        // The whole recording's length, for telling a mis-press from a silent one.
        let recorded = from + rest.len();
        *state.level.lock() = 0.0;
        *state.message.lock() = "Transcribing…".into();
        push(&app);

        let engine = state.engine.lock().clone();
        let started = std::time::Instant::now();
        // What the windows have not already recognised for good: under `WINDOW_SECS` of
        // dictation, all of it, heard in one piece.
        let rest_heard = match &engine {
            Some(e) => recognise_rest(e, &rest, &state, &before),
            None => Rest { words: None, unrecognised_secs: 0.0, failed: Some((0, "no speech model is loaded".into())) },
        };
        let elapsed = started.elapsed().as_millis();
        *state.elapsed_ms.lock() = elapsed;
        state.timings.lock().transcribe_ms = elapsed;
        let missing_end = rest_heard.failed.is_some();
        if let Some((_, why)) = &rest_heard.failed {
            eprintln!("[hvtt] the last words were not all recognised: {why}");
            // Nothing at all to keep: the problem is the message.
            if before.trim().is_empty() && rest_heard.words.is_none() {
                // Speech that gave no words is still said, beside the reason (Codex's tenth review).
                let missing = state.live.lock().unrecognised_secs + rest_heard.unrecognised_secs;
                let why = if missing > 0.0 { format!("{why} {}", unrecognised_note(missing)) } else { why.clone() };
                *state.message.lock() = why.clone();
                state.set_state(SessionState::Error { message: why.clone() });
                push(&app);
                return;
            }
        }
        let (recognised, text, edited, unrecognised) = {
            let mut live = state.live.lock();
            if let Some(words) = &rest_heard.words {
                live.recognised = join_words(&live.recognised, words);
                live.text = join_words(&live.text, words);
            }
            live.unrecognised_secs += rest_heard.unrecognised_secs;
            live.tail.clear();
            (live.recognised.clone(), live.text.trim().to_string(), live.edited, live.unrecognised_secs)
        };
        // Said in the box, held there: the end missing, or speech that gave no words - anywhere in
        // the dictation, pauses included.
        if missing_end || unrecognised > 0.0 {
            let note = if unrecognised > 0.0 {
                unrecognised_note(unrecognised)
            } else {
                "The last few seconds didn't come through.".to_string()
            };
            state.live.lock().trouble = Some(note);
        } else {
            state.live.lock().trouble = None;
        }

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
            // Speech that gave no words is said, and left up long enough to read.
            let trouble = state.live.lock().trouble.clone();
            if let Some(note) = &trouble {
                *state.message.lock() = note.clone();
            }
            state.set_state(SessionState::Ready);
            log_latency(&state);
            push(&app);
            hide_after(&app, std::time::Duration::from_millis(if trouble.is_some() { 9000 } else { 1800 }));
            return;
        }
        if edited {
            learn_from(&app, &recognised, &text);
        }
        if had_keyboard {
            hand_back_keyboard(&app, deliver);
        }
        deliver_words(&app, text, if deliver { Ending::Deliver } else { Ending::Close });
    });
}

/// The product rule, in one call: draft to disk, then clipboard, then delivery. Nothing here
/// loses the text, and the paste rung relies on the clipboard copy.
/// How words reach `deliver_words`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ending {
    /// Sent: into the text box pinned at the keypress, if it can be.
    Deliver,
    /// He closed the box: kept (draft and clipboard), not delivered, and the box goes.
    Close,
}

fn deliver_words(app: &AppHandle, text: String, ending: Ending) {
    let deliver = ending == Ending::Deliver;
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
    // Mark before the pipeline, but never hold this lock across final delivery.
    #[cfg(target_os = "macos")]
    mac_exit::CopyOrder::new(&state.final_copy_started).start_final();
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
    *state.words_unsaved.lock() = !report.text_is_safe();
    state.set_state(SessionState::Ready);
    log_latency(&state);
    push(app);
    // Closed rather than sent: the words are kept, and the box goes as he asked.
    if ending == Ending::Close && report.clipboard_ok {
        dismiss(app.clone());
        return;
    }
    // No text box afterwards: the words are in the field, or on the clipboard. Say which,
    // briefly, and get out of the way. Two things hold the box up: a failed clipboard (the
    // words are only here and in the draft) and the one-time permission ask, which needs a
    // click.
    let hold = !report.clipboard_ok || *state.ask_permission.lock();
    if !hold {
        // Long enough to read that the last few seconds are missing, when they are.
        let missing_end = state.live.lock().trouble.is_some();
        let ms = if missing_end { 9000 } else if delivered { 1200 } else { 2600 };
        hide_after(app, std::time::Duration::from_millis(ms));
    }
}

// ---------------------------------------------------------------------------- startup

/// Is this model's file here - inside the program, or downloaded?
fn model_on_this_computer(model: &hvtt_core::models::Model) -> bool {
    engine_whisper::WhisperEngine::expected_path(model.file).is_some_and(|p| p.exists())
}

/// Load the recognition model in the background so the window appears immediately: the one
/// settings name or, if its file has gone from this computer, the one inside the program, so there
/// is always a model to dictate with. Holds the model work, so nothing is chosen meanwhile.
fn spawn_engine_load(app: AppHandle) {
    std::thread::spawn(load_voice_detector);
    let state: State<App> = app.state();
    let named = state.settings.lock().model.clone();
    let built_in = hvtt_core::models::built_in().file;
    let here = engine_whisper::WhisperEngine::expected_path(&named).is_some_and(|p| p.exists());
    if !here && named != built_in {
        eprintln!("[hvtt] {named} is not on this computer; using the built-in model");
        update_settings(&app, |s| s.model = built_in.to_string());
    }
    let file = if here { named } else { built_in.to_string() };
    *state.model_work.lock() = Some(ModelWork { file: file.clone(), downloading: false });
    std::thread::spawn(move || {
        let loaded = load_model_now(&app, &file, false).is_ok();
        *app.state::<App>().model_work.lock() = None;
        push(&app);
        // A new version or model: see what this computer manages, once.
        let key = format!("{} {file}", app.package_info().version);
        if loaded && app.state::<App>().settings.lock().speed_checked != key {
            speed_check(&app, false);
        }
    });
}

/// The voice detector (whisper.cpp's Silero model, `hvtt_core::models::VOICE_DETECTOR`), for
/// telling quiet speech from noise and finding where speech ends (`hvtt_core::audio::use_detector`).
/// Without its file, loudness alone decides, as before 2026-10-02.
/// Load the voice detector if it is not already, and say whether one is in use.
pub fn load_voice_detector_loaded() -> bool {
    static LOADED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *LOADED.get_or_init(|| {
        load_voice_detector();
        hvtt_core::audio::detector_in_use()
    })
}

pub fn load_voice_detector() {
    use whisper_rs::{WhisperVadContext, WhisperVadContextParams, WhisperVadParams};
    let file = hvtt_core::models::VOICE_DETECTOR.file;
    let Some(path) = engine_whisper::WhisperEngine::expected_path(file).filter(|p| p.exists()) else {
        eprintln!("[hvtt] no voice detector ({file}); loudness alone decides");
        return;
    };
    let mut params = WhisperVadContextParams::new();
    params.set_n_threads(2);
    let Ok(context) = WhisperVadContext::new(&path.to_string_lossy(), params) else {
        eprintln!("[hvtt] the voice detector did not load");
        return;
    };
    let context = Mutex::new(context);
    hvtt_core::audio::use_detector(Box::new(move |samples: &[f32]| {
        // Too little to judge: let loudness decide.
        if samples.len() < 8_000 {
            return None;
        }
        let mut vad = WhisperVadParams::new();
        // Speech parted by 0.1 s of quiet is two stretches; no padding, so gaps are true gaps.
        vad.set_min_silence_duration(100);
        vad.set_speech_pad(0);
        let segments = context.lock().segments_from_samples(vad, samples).ok()?;
        // whisper.cpp gives these in hundredths of a second.
        Some(segments.map(|s| (s.start / 100.0, s.end / 100.0)).collect())
    }));
}

/// Load `file` and make it the one dictation uses. Until it is ready, the model already loaded
/// keeps working. One chosen from the menu (`chosen`) is remembered only once it has loaded; if it
/// cannot be, the one in use stays and the reason comes back. At startup (`chosen` false) there is
/// no other, so the reason is what the menu and the box show.
fn load_model_now(app: &AppHandle, file: &str, chosen: bool) -> Result<(), String> {
    let state: State<App> = app.state();
    let Some(path) = engine_whisper::WhisperEngine::expected_path(file) else {
        let why = "No application data directory available.".to_string();
        *state.engine_status.lock() = why.clone();
        push(app);
        return Err(why);
    };
    let before = state.engine_status.lock().clone();
    *state.engine_status.lock() = format!("Loading {file}…");
    push(app);

    let loaded = engine_whisper::WhisperEngine::load(&path);
    let result = match loaded {
        Ok(engine) => {
            let mut changes = state.model_changes.lock();
            *changes += 1;
            *state.engine_status.lock() = engine.name().to_string();
            *state.engine.lock() = Some(Arc::new(engine));
            // An offer made for the model before no longer applies.
            if state.offered_model.lock().take().is_some() {
                let mut update = state.update.lock();
                if update.as_ref().is_some_and(|v| v.stage == "offer") {
                    *update = None;
                }
            }
            if chosen {
                update_settings(app, |s| s.model = file.to_string());
            }
            drop(changes);
            Ok(())
        }
        Err(e) if chosen && state.engine.lock().is_some() => {
            *state.engine_status.lock() = before;
            Err(e.to_string())
        }
        Err(e) => {
            *state.engine_status.lock() = e.to_string();
            Err(e.to_string())
        }
    };
    push(app);
    result
}

/// H › Settings › Speech Model: use one that is here, or download it first - once, and kept only
/// if it is exactly the published file. The work is reserved here, before anything starts, and
/// released only once the model is in use and remembered, so two choices can never overlap.
///
/// `offer`: chosen from the speed check's offer, made under that count of model changes. It is
/// followed only if no model changed since - checked and the work reserved in one turn, so a model
/// he chose meanwhile is never undone by it (Codex's nineteenth review) - and the model is timed
/// once it is ready. Returns whether the work was taken on.
fn choose_model(app: &AppHandle, model: &'static hvtt_core::models::Model, offer: Option<u64>) -> bool {
    let state: State<App> = app.state();
    if state.session.lock().is_dictating() {
        return false;
    }
    let here = model_on_this_computer(model);
    if here && state.settings.lock().model == model.file && state.engine.lock().is_some() {
        return false;
    }
    // An update being checked or fetched has the box: one download at a time.
    if !here
        && matches!(state.update.lock().as_ref(), Some(v) if matches!(v.stage, "checking" | "downloading" | "installing"))
    {
        reveal_composer(app);
        return false;
    }
    let dir = hvtt_core::paths::models_dir();
    if !here && dir.is_none() {
        return false;
    }
    {
        let changes = offer.map(|_| state.model_changes.lock());
        if changes.as_deref().zip(offer).is_some_and(|(now, then)| *now != then) {
            return false;
        }
        let mut work = state.model_work.lock();
        if work.is_some() {
            return false;
        }
        *work = Some(ModelWork { file: model.file.to_string(), downloading: !here });
    }
    push(app);

    let size = hvtt_core::models::megabytes(model.bytes);
    if !here {
        show_update(
            app,
            "downloading",
            format!("Downloading the {} speech model…", model.name),
            format!("0% of {size}. You can keep dictating meanwhile."),
        );
    }
    let app = app.clone();
    std::thread::spawn(move || {
        let failed = |title: &str, why: String| show_update(&app, "failed", title.into(), why);
        let mut fetched = true;
        if let Some(dir) = dir.filter(|_| !here) {
            let progress = {
                let app = app.clone();
                let size = size.clone();
                move |got: u64| {
                    let state: State<App> = app.state();
                    if let Some(view) = state.update.lock().as_mut().filter(|v| v.stage == "downloading") {
                        view.detail = format!(
                            "{}% of {size}. You can keep dictating meanwhile.",
                            got.saturating_mul(100) / model.bytes
                        );
                    }
                    push(&app);
                }
            };
            match update::download_model(model, &dir, progress) {
                Err(why) => {
                    failed("Couldn't download the speech model", why);
                    fetched = false;
                }
                Ok(_) => {
                    let state: State<App> = app.state();
                    if let Some(view) = state.update.lock().as_mut() {
                        view.detail = "Downloaded and checked. Getting it ready…".into();
                    }
                    // Still reserved: from downloading to loading, never free in between.
                    *state.model_work.lock() = Some(ModelWork { file: model.file.to_string(), downloading: false });
                    push(&app);
                }
            }
        }
        if fetched {
            match load_model_now(&app, model.file, true) {
                Err(why) => failed("Couldn't switch the speech model", why),
                // A download is announced, and the announcement leaves by itself; a model that
                // was already here just switches - the menu shows it.
                Ok(()) if !here => {
                    show_update(
                        &app,
                        "current",
                        format!("The {} speech model is ready", model.name),
                        "Your words use it from now on.".into(),
                    );
                    hide_update_after(&app, std::time::Duration::from_millis(2600));
                }
                Ok(()) => {}
            }
        }
        let switched = fetched && app.state::<App>().settings.lock().model == model.file;
        *app.state::<App>().model_work.lock() = None;
        push(&app);
        // Chosen from the speed check's offer: measure it, rather than trust the estimate.
        if offer.is_some() && switched {
            speed_check(&app, true);
        }
    });
    true
}

/// After the stop press, keep the microphone open until he has stopped talking: at least 0.1 s
/// (sound still on its way from the device), then until the last 0.25 s are back down to the
/// room, at most 1.2 s. A quick dictation is often stopped on its last syllable; cutting the last
/// 0.2 s doubled the words wrong (measured 2026-10-01, `hvtt_core::audio::voice_has_stopped`).
/// Widened from 0.15 s / 0.6 s after Codex's sixth review found a soft ending cut: a little more
/// time at the stop, for the last consonant - his priority.
fn wait_for_quiet(rec: &recorder::Recording, careful: bool) {
    let started = std::time::Instant::now();
    std::thread::sleep(std::time::Duration::from_millis(100));
    while started.elapsed() < std::time::Duration::from_millis(1200) {
        // The last few seconds: enough to know the room and how he was speaking.
        let recent = rec.peek_from(rec.recorded_len().saturating_sub(6 * 16_000));
        if hvtt_core::audio::voice_has_stopped(&recent, 0.25, careful) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    eprintln!("[hvtt] listened {} ms past the stop press", started.elapsed().as_millis());
}

/// One update check, and one speed check, at a time.
static UPDATE_WORK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static SPEED_WORK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// A speed check was asked for, so its result is said even if all is well.
static SPEED_ANNOUNCE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Clears a reservation when its work ends, however it ends.
struct Released(&'static std::sync::atomic::AtomicBool);

impl Drop for Released {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

/// The speed check: one short test recognition with the model in use, timed - and settings
/// adjusted to it (`models::speed_advice`). Run by itself when the program opens on a new version
/// or model, and from H › Settings › Check This Computer's Speed (`announce`: always say the
/// result). No microphone: three seconds of faint made-up sound, twice - the first readies the
/// graphics processor, the second is timed (from when it has the engine, not while waiting).
///
/// Dictation comes first (Codex's seventeenth review): it waits while he dictates or a model
/// loads, gives up part-way if a dictation starts (the live epoch), and applies nothing if the
/// model changed meanwhile; Live Words is changed only if it is still "As You Talk".
fn speed_check(app: &AppHandle, announce: bool) {
    // Asked for while one runs: that one says its result (Codex's nineteenth review).
    if announce {
        SPEED_ANNOUNCE.store(true, std::sync::atomic::Ordering::SeqCst);
    }
    if SPEED_WORK.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    let app = app.clone();
    std::thread::spawn(move || {
        let _work = Released(&SPEED_WORK);
        let state: State<App> = app.state();
        let busy = |state: &App| {
            let session = state.session.lock();
            session.is_dictating() || matches!(*session, SessionState::Transcribing)
        };
        let asked = || SPEED_ANNOUNCE.load(std::sync::atomic::Ordering::SeqCst);
        let current_epoch = || state.live_epoch.load(std::sync::atomic::Ordering::SeqCst);
        let sound: Vec<f32> = (0..48_000u32).map(|i| 0.002 * (((i.wrapping_mul(2_654_435_761)) >> 16) as f32 / 65_536.0 - 0.5)).collect();
        // Waits at most two minutes for the program to be free; the check made on opening is
        // tried again next time (Codex's eighteenth review). Interrupted by a dictation or a model
        // change, it waits and measures again - whatever model is then in use.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        let (result, model_file, measured_under, changes) = loop {
            // Read before looking: a dictation that starts after this counts again once it shows
            // as recording, so it can never go unnoticed (`start_recording`).
            let mut epoch = current_epoch();
            loop {
                let free = !busy(&state) && state.model_work.lock().is_none();
                if free && std::time::Instant::now() <= deadline {
                    break;
                }
                if std::time::Instant::now() > deadline {
                    eprintln!("[hvtt] speed check: never free, skipped");
                    if asked() {
                        SPEED_ANNOUNCE.store(false, std::sync::atomic::Ordering::SeqCst);
                        speed_notice(&app, notice("Couldn't check the speed just now",
                            "It waits for dictation and model changes to finish. Try again in a moment.".into()), Some(8000));
                    }
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(500));
                epoch = current_epoch();
            }
            let measured_under = *state.model_changes.lock();
            let Some(engine) = state.engine.lock().clone() else { return };
            let model_file = state.settings.lock().model.clone();
            let give_up = Some(hvtt_core::engine::GiveUp { counter: state.live_epoch.clone(), value: epoch });
            let request = TranscriptionRequest { samples: sound.clone(), vocabulary_prompt: None, give_up, provisional: false };
            let warm = engine.transcribe(&request);
            let timed = if current_epoch() == epoch && warm.is_ok() { engine.transcribe(&request) } else { warm };
            if current_epoch() != epoch {
                continue;
            }
            let Ok(result) = timed else {
                if asked() {
                    SPEED_ANNOUNCE.store(false, std::sync::atomic::Ordering::SeqCst);
                    speed_notice(&app, notice("Couldn't check the speed",
                        "The speech model could not run the test. Try again, or choose another model.".into()), Some(8000));
                }
                return;
            };
            // Decided and shown while no model can change: a dictation begun, or another model in
            // use, and this measurement means nothing now.
            let changes = state.model_changes.lock();
            let same_engine = state.engine.lock().as_ref().is_some_and(|now| Arc::ptr_eq(now, &engine));
            if current_epoch() != epoch || *changes != measured_under || !same_engine || state.settings.lock().model != model_file {
                continue;
            }
            break (result, model_file, measured_under, changes);
        };
        let announce = SPEED_ANNOUNCE.swap(false, std::sync::atomic::Ordering::SeqCst);
        let ms = result.elapsed_ms;
        let secs = |ms: u128| format!("{:.1} s", ms as f32 / 1000.0);
        let key = format!("{} {model_file}", app.package_info().version);
        update_settings(&app, |s| s.speed_checked = key);
        let current = hvtt_core::models::find(&model_file);
        let name = current.map_or("this model", |m| m.name);
        eprintln!("[hvtt] speed check: {name} takes {ms} ms here");
        let lighter = |app: &AppHandle| {
            update_settings(app, |s| {
                if s.live_words == LiveWords::AsYouTalk {
                    s.live_words = LiveWords::Lighter;
                }
            });
        };
        use hvtt_core::models::SpeedAdvice;
        let (view, hide_after) = match hvtt_core::models::speed_advice(ms) {
            SpeedAdvice::Fine if announce => (Some(notice("This computer is quick enough",
                format!("A short test with {name} took {}. Nothing needs changing.", secs(ms)))), Some(5000)),
            SpeedAdvice::Fine => (None, None),
            SpeedAdvice::Lighter => {
                lighter(&app);
                (Some(notice("Set up for this computer", format!(
                    "A short test with {name} took {}, so the words shown while you talk refresh less often \
                     (H › Settings › Live Words). Your words are just as accurate.", secs(ms)))), Some(8000))
            }
            SpeedAdvice::Smaller => match current.and_then(|c| hvtt_core::models::faster_choice(c, ms)) {
                Some((offer, estimate)) => {
                    let download = if model_on_this_computer(offer) {
                        String::new()
                    } else {
                        format!(" ({} download)", hvtt_core::models::megabytes(offer.bytes))
                    };
                    // Forgotten again below if the message cannot be shown.
                    *state.offered_model.lock() = Some((offer, measured_under));
                    (Some(UpdateView {
                        stage: "offer",
                        title: "This computer is slow for this model".into(),
                        detail: format!("A short test with {name} took {}. {} should take roughly {} here \
                            (an estimate, checked once you switch) - {}{download}.",
                            secs(ms), offer.name, secs(estimate), offer.note),
                        action: Some(format!("Switch to {}", offer.name)),
                    }), None)
                }
                None => {
                    lighter(&app);
                    (announce.then(|| notice("This computer is slow", format!(
                        "A short test with {name} took {}. The words shown while you talk now refresh less often.", secs(ms)))), Some(8000))
                }
            },
        };
        let Some(view) = view else {
            drop(changes);
            push(&app);
            return;
        };
        let offering = view.stage == "offer";
        if !speed_notice(&app, view, hide_after) && offering {
            *state.offered_model.lock() = None;
        }
        drop(changes);
    });
}

/// Show a speed-check message: checked and shown in one turn of the update lock, never over an
/// update being checked, fetched or offered (Codex's eighteenth review). Whether it was shown.
fn speed_notice(app: &AppHandle, view: UpdateView, hide_after: Option<u64>) -> bool {
    let state: State<App> = app.state();
    let shown = {
        let mut update = state.update.lock();
        let busy = update.as_ref().is_some_and(|v| matches!(v.stage, "checking" | "downloading" | "installing" | "ready"));
        if !busy {
            *update = Some(view);
        }
        !busy
    };
    if shown {
        if !state.session.lock().is_dictating() {
            reveal_composer(app);
        }
        if let Some(ms) = hide_after {
            hide_update_after(app, std::time::Duration::from_millis(ms));
        }
    }
    push(app);
    shown
}

/// A speed-check message with no button.
fn notice(title: &str, detail: String) -> UpdateView {
    UpdateView { stage: "current", title: title.into(), detail, action: None }
}

/// The speed check's offer, accepted: the smaller model, downloaded if need be.
#[tauri::command]
fn accept_offer(app: AppHandle) {
    let state: State<App> = app.state();
    // Only while the offer is what the box shows: a message that replaced it (a ready update) is
    // never cleared by a late click (Codex's nineteenth review).
    if !state.update.lock().as_ref().is_some_and(|v| v.stage == "offer") {
        *state.offered_model.lock() = None;
        return;
    }
    let Some((model, measured_under)) = *state.offered_model.lock() else { return };
    // Made for a model no longer in use, it is not followed (Codex's eighteenth review).
    if choose_model(&app, model, Some(measured_under)) {
        *state.offered_model.lock() = None;
        let mut update = state.update.lock();
        if update.as_ref().is_some_and(|v| v.stage == "offer") {
            *update = None;
        }
    }
    push(&app);
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
        prepared: Mutex::new(None),
        engine: Mutex::new(None),
        model_work: Mutex::new(None),
        offered_model: Mutex::new(None),
        model_changes: Mutex::new(0),
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
        words_unsaved: Mutex::new(false),
        #[cfg(target_os = "macos")]
        final_copy_started: Mutex::new(false),
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
            accept_offer,
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
                hide_update_after(&handle, std::time::Duration::from_millis(3000));
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
                hide_update_after(&handle, std::time::Duration::from_millis(6000));
            }

            spawn_engine_load(app.handle().clone());
            // Recordings are no longer kept (his decision, 2026-10-03): the ones it made while it
            // kept the last five are deleted - at most five files, so done here, before anything
            // could close the program - and nothing else in that folder.
            // Not filtered first: a folder that cannot even be looked at is reported (Codex's 23rd review).
            if let Some(dir) = hvtt_core::paths::recordings_dir() {
                match hvtt_core::paths::remove_old_recordings(&dir) {
                    Ok((0, 0)) => {}
                    Ok((deleted, 0)) => eprintln!("[hvtt] old recordings deleted: {deleted}"),
                    Ok((deleted, stuck)) => eprintln!(
                        "[hvtt] old recordings: {deleted} deleted, {stuck} could not be - may remain in {}",
                        dir.display()
                    ),
                    Err(why) => eprintln!("[hvtt] old recordings not cleaned up, some may remain: {why}"),
                }
            }
            // Once, quietly, a little after opening - out of the way of loading the model.
            if app.state::<App>().settings.lock().check_updates_on_start {
                let handle = app.handle().clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_secs(20));
                    let state: State<App> = handle.state();
                    // Switched off meanwhile: no request at all (Codex's seventeenth review).
                    let wanted = state.settings.lock().check_updates_on_start;
                    if wanted && !state.session.lock().is_dictating() {
                        check_for_updates_how(&handle, true);
                    }
                });
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building Huck's Voice to Text")
        .run(|app, event| {
            #[cfg(target_os = "macos")]
            match event {
                // A request to leave that can be refused (`app.exit`, the last window closing):
                // refused until his words and clipboard are safe, then let through by `quit`
                // (Codex's third review of 0.1.6).
                tauri::RunEvent::ExitRequested { api, .. } => {
                    if !EXIT_ALLOWED.load(std::sync::atomic::Ordering::SeqCst) {
                        api.prevent_exit();
                        quit(app.clone());
                    }
                }
                // The end, which cannot be refused. After a Quit everything is already safe;
                // after a logout or shutdown - which macOS delivers straight here, never as a
                // request (tao's `applicationWillTerminate`) - keep what can be kept in the few
                // seconds macOS allows, then leave.
                tauri::RunEvent::Exit => {
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(4);
                    if !EXIT_ALLOWED.load(std::sync::atomic::Ordering::SeqCst) {
                        QUITTING.store(true, std::sync::atomic::Ordering::SeqCst);
                        keep_words_so_far(app);
                        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                        if !keep_everything_before_exit(app, remaining) {
                            eprintln!("[hvtt] leaving before everything was kept (the system is ending the session)");
                        }
                    }
                    leave_now(app);
                }
                _ => {}
            }
            #[cfg(not(target_os = "macos"))]
            let _ = (app, event);
        });
}

#[cfg(target_os = "macos")]
extern "C" {
    fn _exit(status: i32) -> !;
}

/// Leave without running the C++ static destructors (macOS). The speech engine's Metal backend
/// (ggml) frees its device in one and, with the model still loaded, calls `abort()`
/// (`ggml_metal_rsets_free`): **every normal Quit produced a crash report** - found 2026-09-29
/// testing Quit for real, on the debug build (which names the frames) and on the first 0.1.6
/// candidate, so it is older than any of that work and very likely in the published builds too.
///
/// Tauri calls the run callback's `Exit` *before* its own `cleanup_before_exit` (tauri 2.11.6
/// `app.rs`), so that cleanup is called here first - the tray icon, the resource tables - and only
/// the C++ static destructors are skipped. Nothing of his is only in memory by now: the caller has
/// kept any dictation and given back his clipboard, settings and drafts are written as they change,
/// and Huck's clipboard is a named pasteboard that outlives the program. (Windows has no such
/// backend and is untouched.)
#[cfg(target_os = "macos")]
fn leave_now(app: &AppHandle) -> ! {
    app.cleanup_before_exit();
    unsafe { _exit(0) }
}

#[cfg(test)]
mod settings_tests {
    use super::*;

    /// Codex's fourteenth review: a slow save of one change let another change made meanwhile be
    /// undone. In one turn, both stay.
    #[test]
    fn two_settings_changed_at_once_both_stay() {
        let settings = std::sync::Arc::new(Mutex::new(Settings::default()));
        let model = {
            let settings = settings.clone();
            std::thread::spawn(move || {
                change_settings(&settings, |s| s.model = "ggml-small.en.bin".into(), |_| {
                    std::thread::sleep(std::time::Duration::from_millis(80));
                });
            })
        };
        std::thread::sleep(std::time::Duration::from_millis(20));
        change_settings(&settings, |s| s.keep_drafts = false, |_| {});
        model.join().unwrap();
        let s = settings.lock();
        assert_eq!(s.model, "ggml-small.en.bin");
        assert!(!s.keep_drafts, "switched off, and stays off");
    }
}
