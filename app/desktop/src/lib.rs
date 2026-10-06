//! Huck's Voice to Text — desktop application.
//!
//! Hotkey -> record -> transcribe locally -> clipboard -> the text box that was focused at the
//! keypress. The clipboard copy always happens first; it is the safeguard when the box is gone.

pub mod bridge;
pub mod clip;
pub mod destination;
pub mod engine_whisper;
#[cfg(any(target_os = "macos", windows))]
mod exit;
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
use hvtt_core::settings::{ClipboardChoice, DictationStyle, LiveWords, Settings};
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
    /// The finished words could not be copied: the box shows them (`text`), with Copy.
    not_copied: bool,
    /// ...and no draft holds them either, so the box also offers Discard and will not close
    /// without one or the other.
    words_unsaved: bool,
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
    /// This dictation is a held one (Dictation Style > Hold to Talk): no words, Pause or Send.
    hold: bool,
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
    /// The last words are neither on a clipboard nor in a draft - only in the box, which shows
    /// them with Copy and Discard. Nothing throws them away without his say: closing the box,
    /// starting another dictation and Quit each try the copy again first (`keep_unsaved_words`).
    /// (Codex's fourth review of 0.1.6, and its fifth of the 0.1.8 candidate.)
    words_unsaved: Mutex<bool>,
    /// The clipboard this dictation uses (`DictationClipboard`), fixed when it starts.
    clipboard_in_use: DictationClipboard,
    /// The last words could not be copied to the clipboard he chose. The box shows them, with
    /// Copy to try again. They may still be in a draft; `words_unsaved` says when they are not.
    not_copied: Mutex<bool>,
    /// Serialises the logout's early copy with the start of the final copy.
    #[cfg(any(target_os = "macos", windows))]
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

/// The clipboard one dictation uses, from its keypress to its last retry: **chosen once, when
/// it starts**. Two things must agree on it - how the paste rung is built (does it borrow the
/// normal clipboard for the paste, or rely on the copy just made there?) and where the finished
/// words are copied. Each used to read the setting when it ran, and the setting can be changed
/// from the H while he dictates: started on the normal clipboard and switched to Huck's, the
/// words went to Huck's Clipboard, the copy counted as made, and the paste - built not to borrow
/// - sent whatever the normal clipboard had held before, as "Sent" (Codex's seventh review of
/// the 0.1.8 candidate; both platforms, older than that work). A change in the menu now applies
/// from the next dictation.
struct DictationClipboard(Mutex<ClipboardChoice>);

impl DictationClipboard {
    fn new(choice: ClipboardChoice) -> Self {
        DictationClipboard(Mutex::new(choice))
    }

    /// A dictation starts: whatever the setting says now is its clipboard until it is over.
    fn start(&self, settings: &Settings) {
        *self.0.lock() = settings.clipboard;
    }

    fn choice(&self) -> ClipboardChoice {
        *self.0.lock()
    }

    /// Does this dictation's paste borrow the normal clipboard (Huck's Clipboard is in use)?
    #[cfg_attr(not(any(target_os = "macos", windows)), allow(dead_code))]
    fn paste_borrows(&self) -> bool {
        self.choice() == ClipboardChoice::Huck
    }
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
        // Read once: a lock taken twice in the one expression below never comes back (it froze
        // the program on opening, 2026-10-05).
        let generation = *self.generation.lock();
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
            not_copied: *self.not_copied.lock(),
            words_unsaved: *self.words_unsaved.lock(),
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
            generation,
            hold: held(generation),
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

/// **The box is changed on the main thread, one thing at a time.** Putting it away
/// (`dismiss_now`), starting a dictation in it (`toggle_now`), and showing a warning that must be
/// seen before the program leaves with something of his all run there, so none of them can land
/// in the middle of another. Until 2026-10-03 each ran on whatever thread asked for it: a box
/// being put away by one thread could hide a warning another had just shown and counted as seen
/// - it set the box idle, and only then hid it - and a shortcut press could start a dictation
/// that a put-away already under way then reset (Codex's fifth review of the 0.1.8 candidate).
/// The main thread is the one place the box's window is changed anyway, so it is the turn-taker:
/// no lock is held across a window call, which a lock shared with the main thread could not
/// survive.
///
/// Run `f` there and wait for its answer. Called on the main thread it simply runs
/// (tauri-runtime-wry's `send_user_message`). `None` when the main thread did not take it in
/// time - the program is ending, or a menu is open - which every caller treats as "not shown".
fn on_main<T: Send + 'static>(
    app: &AppHandle,
    f: impl FnOnce(&AppHandle) -> T + Send + 'static,
) -> Option<T> {
    let (done, answer) = std::sync::mpsc::channel();
    let handle = app.clone();
    app.run_on_main_thread(move || {
        let _ = done.send(f(&handle));
    })
    .ok()?;
    answer.recv_timeout(std::time::Duration::from_secs(5)).ok()
}

/// Held while which dictation is the current one changes - one starting (`start_recording`), or
/// the box put away with nothing kept (`dismiss_now`) - and while a `finish` worker makes a
/// dictation its own. Those changes are several steps on the main thread, and a worker looking
/// in between them could check the dictation it was asked about and then take the *next* one's
/// recording (Codex's second review of hold to talk, 2026-10-05). The main thread holds it only
/// across its own steps and the worker only across its few; neither waits on the other
/// meanwhile, so this is not the kind of lock `on_main` warns about.
static DICTATION_CHANGES: Mutex<()> = Mutex::new(());

/// `on_main` for work nobody waits on: it takes its turn on the main thread and the caller goes on.
fn post_to_main(app: &AppHandle, f: impl FnOnce(&AppHandle) + Send + 'static) {
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || f(&handle));
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
        // Looked at when it is this request's turn on the main thread, not before: looked at
        // here, a dictation started in between was put away by it.
        dismiss_if(&app, move |state| {
            // One lock at a time: `snapshot` holds the session while it reads the generation.
            let same = *state.generation.lock() == scheduled_for;
            same && !state.session.lock().is_dictating() && !warning_up(state)
        });
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
                (Action::Dictate, ShortcutState::Pressed) => dictate_pressed(handle.clone()),
                (Action::Dictate, ShortcutState::Released) => post_to_main(handle, hold_released),
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

/// Is the box showing the message with this title right now? The box's own rule (`ui/app.js`
/// `render`), which this must keep step with: a message takes the box only while nothing else is
/// going on (`Idle`); a **warning** - stage `"warning"`, something of his that leaving would lose -
/// is also said beside whatever a finished dictation or a problem has left on the box (`Ready`,
/// `Error`). Never while he dictates or words are being recognised, and never under the shortcut
/// prompt.
///
/// Setting a message is not the same as showing it: a warning must not be counted as given unless
/// this says so (Codex's third review of the 0.1.8 candidate).
#[cfg_attr(not(any(target_os = "macos", windows)), allow(dead_code))]
fn message_on_screen(session: &SessionState, rebinding: bool, view: Option<&UpdateView>, title: &str) -> bool {
    let Some(view) = view.filter(|v| v.title == title) else { return false };
    if rebinding {
        return false;
    }
    match session {
        SessionState::Idle => true,
        SessionState::Ready | SessionState::Error { .. } => view.stage == "warning",
        SessionState::Recording | SessionState::Paused | SessionState::Transcribing => false,
    }
}

/// `message_on_screen`, for the program as it stands.
#[cfg_attr(not(any(target_os = "macos", windows)), allow(dead_code))]
fn on_screen(state: &App, title: &str) -> bool {
    // One lock at a time: `snapshot` holds the session while it reads the others.
    let rebinding = state.rebinding.lock().is_some();
    let session = state.session.lock().clone();
    let view = state.update.lock().clone();
    message_on_screen(&session, rebinding, view.as_ref(), title)
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
        // Looked at when it is this request's turn on the main thread (`hide_after`).
        dismiss_if(&app, move |state| {
            // One lock at a time: `snapshot` holds the session while it reads the others.
            let idle = matches!(*state.session.lock(), SessionState::Idle);
            let same = state
                .update
                .lock()
                .as_ref()
                .is_some_and(|v| v.stage == shown.0 && v.title == shown.1);
            idle && same
        });
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
    UPDATE_TOLD.lock().forget();
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
        // The ask, taken in one turn on the main thread (`on_main`), before anything below
        // changes what the box shows. He may be answering the box's warning that his own
        // clipboard is not back. And on Windows Huck's Clipboard lives in this program's memory,
        // and may hold the only copy of his last words: say so before an update empties it; Open
        // Update chosen again, with those same words on it, goes ahead.
        let asked = on_main(&app, |app| {
            let agreed = agreed_to_leave_without_clipboard(&app.state::<App>());
            if update_would_take_untold_words() {
                warn_huck_clipboard(app);
                return None;
            }
            show_update(
                app,
                "installing",
                "Installing the update…".into(),
                "Back in a moment. Your settings stay as they are.".into(),
            );
            Some(agreed)
        });
        let Some(Some(agreed)) = asked else { return };
        std::thread::spawn(move || {
            use std::sync::atomic::Ordering;
            // Long enough to read before this copy steps aside.
            std::thread::sleep(std::time::Duration::from_millis(1200));
            // Never step aside mid-dictation - its words are drafted, copied and delivered first -
            // nor while words are only in the box, copied nowhere (`words_unsaved`: it waits for
            // him to copy or discard them), nor while his own clipboard is out on loan for a
            // paste (Codex's review of 0.1.5).
            let dictating = |app: &AppHandle| {
                let state: State<App> = app.state();
                let busy = matches!(
                    *state.session.lock(),
                    SessionState::Recording | SessionState::Paused | SessionState::Transcribing
                );
                busy || *state.words_unsaved.lock()
            };
            // And once neither is so, neither may begin: no dictation (the way out is taken -
            // `Leaving::begin`, which a Quit under way holds too, so the two take turns - in the
            // same turn of the lock that lets a dictation in) and no borrow (`stop_borrowing`
            // closes the gate in the same turn as seeing it idle). Looking and then leaving, as
            // it did until 2026-10-03, left an instant for either to start - the Mac's finding 7.
            loop {
                while dictating(&app) {
                    std::thread::sleep(std::time::Duration::from_millis(200));
                }
                if LEAVING.lock().begin() {
                    // One let in just before, not yet showing as a dictation, counts as one -
                    // and is looked for first, since it shows before it stops counting.
                    let on_its_way = LEAVING.lock().starting > 0;
                    if !on_its_way && !dictating(&app) {
                        // A give-back already known to be stuck gets a moment, not the whole wait.
                        let wait = if crate::clip::huck::unreturned().is_some() { 2 } else { 10 };
                        if crate::clip::huck::stop_borrowing(std::time::Duration::from_secs(wait)) {
                            break;
                        }
                        // His clipboard cannot be given back. Said once, in the update's own
                        // message so Open Update is still there to choose; chosen again with that
                        // on the box, what he had copied is let go and the update goes ahead.
                        if left_without_his_clipboard(&app, "ready", "Open Update", "update", agreed) {
                            break;
                        }
                        if crate::clip::huck::unreturned().is_some() {
                            crate::clip::huck::resume_borrowing();
                            LEAVING.lock().stay();
                            return;
                        }
                    }
                    // A dictation began in that instant, or a give-back needs longer: as it was.
                    crate::clip::huck::resume_borrowing();
                    LEAVING.lock().stay();
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            // Not leaving after all: dictation and pasting work again.
            let stay = || {
                crate::clip::huck::resume_borrowing();
                LEAVING.lock().stay();
            };
            // New words arrived on Huck's Clipboard while it waited: those he has not been told
            // about. Stop and say so; Open Update again goes ahead.
            if update_would_take_untold_words() {
                stay();
                warn_huck_clipboard(&app);
                return;
            }
            // The installer must not find this copy still "running" while it quits.
            win_surface::release_single_instance();
            match start_installer(&installer) {
                Ok(()) => {
                    EXIT_ALLOWED.store(true, Ordering::SeqCst);
                    app.exit(0)
                }
                Err(e) => {
                    let _ = win_surface::claim_single_instance();
                    stay();
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

/// The words on Huck's Clipboard that the box has told him an update would empty.
///
/// Would the update lose words? Whenever Huck's Clipboard holds some: it lives in this program's
/// memory, and whether a recovery draft of those very words was written cannot be told from the
/// setting (drafts can be switched on afterwards, or a write can fail), so this does not try
/// (Codex's second review of 0.1.5). He is told once for the words that are there - and again for
/// any that arrive afterwards.
///
/// **Which words he was told about is remembered, not merely that he was told.** Until 2026-10-03
/// it was a yes-or-no, with new words looked for only against what was there when Open Update was
/// last chosen: words that arrived while an update waited, when that update then backed out for
/// another reason, were "already there" at the next ask and went unsaid - the fault Codex's fourth
/// review found in Quit, here in the update.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, Default, PartialEq, Eq)]
struct ToldWords(Option<String>);

#[cfg_attr(not(windows), allow(dead_code))]
impl ToldWords {
    /// Does Huck's Clipboard hold words he has not been told about?
    fn untold(&self, held: &Option<String>) -> bool {
        held.is_some() && self.0 != *held
    }

    /// The box has shown the warning with these words on Huck's Clipboard.
    fn told(&mut self, held: Option<String>) {
        self.0 = held;
    }

    /// A new update is on offer: he is told afresh.
    fn forget(&mut self) {
        self.0 = None;
    }
}

#[cfg(windows)]
static UPDATE_TOLD: Mutex<ToldWords> = Mutex::new(ToldWords(None));

/// Huck's Clipboard holds words he has not been told the update would empty.
#[cfg(windows)]
fn update_would_take_untold_words() -> bool {
    let held = held_on_huck_clipboard();
    UPDATE_TOLD.lock().untold(&held)
}

#[cfg(windows)]
fn warn_huck_clipboard(app: &AppHandle) {
    // Shown and counted in one turn on the main thread (`on_main`), so no put-away can land
    // between the two.
    on_main(app, |app| {
        let held = held_on_huck_clipboard();
        show_update(
            app,
            "ready",
            PASTE_FIRST.into(),
            "Updating empties Huck's Clipboard. Paste anything you still need from it, then \
             choose Open Update again."
                .into(),
        );
        // Told only if the box is showing it (`message_on_screen`): otherwise the next Open
        // Update says it again rather than going ahead.
        if on_screen(&app.state::<App>(), PASTE_FIRST) {
            UPDATE_TOLD.lock().told(held);
        }
    });
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
        "{:?}|{}|{}|{}|{:?}|{}|{}|{}|{:?}|{:?}|{:?}|{}|{:?}|{:?}|{}|{:?}|{}|{}",
        s.dictation_style,
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

    // How he dictates: it decides whether Live Words, just below it, applies.
    let hold = s.dictation_style == DictationStyle::HoldToTalk;
    let style = Submenu::with_id(app, "dictation-style", "Dictation Style", !recording)?;
    style.append(&check("style-press", "Press to Start, Press to Send", !hold)?)?;
    style.append(&check("style-hold", "Hold to Talk — let go to send", hold)?)?;

    // Plain words, so nobody has to know what processor they have. Greyed with Hold to Talk,
    // which shows no words while he holds - and saying so, so the grey explains itself.
    let live_title = if hold { "Live Words — only with Press to Start, Press to Send" } else { "Live Words" };
    let live = Submenu::with_id(app, "live-words", live_title, !hold)?;
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
    // Right above Live Words (his placing, 2026-10-05): it decides whether Live Words applies.
    settings.append(&style)?;
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
        "style-press" => update_settings(app, |s| s.dictation_style = DictationStyle::PressToSend),
        "style-hold" => update_settings(app, |s| s.dictation_style = DictationStyle::HoldToTalk),
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

/// Quit - but only once his words and his clipboard are safe (Codex's reviews of 0.1.6; on
/// Windows too since 2026-10-03, where Quit used to leave at once). A dictation in progress is
/// finished and kept first (`keep_everything_before_exit`). A paste on Huck's clipboard has his
/// own clipboard for half a second, and leaving in that time would leave the pasted words on it
/// instead: from here on no paste may borrow it (one already waiting gives up), the borrow on loan
/// is waited for - on a worker, so the menu stays alive - and only then does the program exit. If
/// it cannot be given back within a generous ten seconds (a stuck restore), the program does
/// **not** exit and pasting works again: choose Quit once more.
fn quit(app: AppHandle) {
    #[cfg(any(target_os = "macos", windows))]
    {
        use std::sync::atomic::Ordering;
        // One Quit at a time: a second choice while the first still waits would race its
        // `resume_borrowing`, reopening the gate under the other.
        if !LEAVING.lock().begin() {
            return;
        }
        // The ask: what the box is showing him as he chooses Quit is what he is answering. Read
        // in one turn on the main thread (`on_main`), before this Quit changes anything.
        let asked = on_main(&app, Asked::now).unwrap_or_default();
        std::thread::spawn(move || {
            let kept = keep_everything_before_exit(&app, std::time::Duration::from_secs(20));
            // Words that could be neither copied nor drafted are only in the box, which shows
            // them. One more try at keeping them, before anything below looks at what this Quit
            // has put where: the try writes the clipboard he chose, and on Windows that may be
            // Huck's Clipboard, which leaves with the program.
            let unsaved = !keep_unsaved_words(&app);
            // Read at once: anything on Huck's Clipboard now that was not there when Quit was
            // chosen arrived while this Quit was keeping everything.
            #[cfg(windows)]
            let held_now = held_on_huck_clipboard();
            // His clipboard is out on loan and cannot be given back. Said once; chosen again with
            // that on the box, what he had copied is let go, and the way is clear. (The Mac too
            // since 2026-10-04.)
            let kept = kept || left_without_his_clipboard(&app, "warning", "Quit", "leave", asked.clipboard);
            // Windows: words a Quit has put on Huck's Clipboard leave with the program
            // (`HuckUntold`). The box says so and this Quit stays; chosen again with that
            // warning in front of him, it goes.
            #[cfg(windows)]
            let said =
                huck_untold_at_quit(&HUCK_UNTOLD, &asked.held, &held_now, kept, asked.words.as_deref(), || {
                    paste_first_shown(&app)
                });
            #[cfg(windows)]
            let kept = kept && said.is_none();
            // Words still unsaved after that try: the first Quit brings the box forward instead
            // of leaving, and a second one - chosen with those words in front of him - goes.
            // (Codex's fourth review of 0.1.6, and its fifth of 0.1.8: the box used to say "copy
            // it before closing" and show nothing to copy.)
            let state: State<App> = app.state();
            let generation = *state.generation.lock();
            let warned = (*QUIT_WARNED_GENERATION.lock()).filter(|_| asked.unsaved_seen);
            match exit::quit_decision(kept, unsaved, warned, generation) {
                exit::QuitDecision::Exit => {
                    EXIT_ALLOWED.store(true, Ordering::SeqCst);
                    app.exit(0);
                    return;
                }
                exit::QuitDecision::Warn => {
                    eprintln!("[hvtt] not quitting yet: his last words would leave with the program");
                    // Brought forward and looked at in one turn (`on_main`): he counts as warned
                    // only if the box is showing the words and this sentence over them.
                    let seen = on_main(&app, |app| {
                        let state: State<App> = app.state();
                        *state.message.lock() = UNSAVED_AT_QUIT.into();
                        reveal_composer(app);
                        push(app);
                        quit_confirmation_showing(&state)
                    });
                    if seen == Some(true) {
                        *QUIT_WARNED_GENERATION.lock() = Some(generation);
                    }
                }
                exit::QuitDecision::Stay => {
                    eprintln!("[hvtt] not quitting yet: a dictation or his clipboard is not safe yet");
                    // Still recognising after the whole wait: said, so the box does not just go
                    // on saying "Transcribing" with Quit apparently ignored.
                    if matches!(*state.session.lock(), SessionState::Transcribing) {
                        *state.message.lock() = STILL_TRANSCRIBING_AT_QUIT.into();
                        push(&app);
                    }
                }
            }
            clip::huck::resume_borrowing();
            LEAVING.lock().stay();
        });
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    app.exit(0);
}

/// What the box says when a Quit has waited its twenty seconds and recognition is not done.
#[cfg(any(target_os = "macos", windows))]
const STILL_TRANSCRIBING_AT_QUIT: &str =
    "Still working on your last words, so Quit is waiting. Choose Quit again once they are kept.";

/// What the box says when a Quit finds words that are saved nowhere else.
const UNSAVED_AT_QUIT_TEXT: &str =
    "These words are only here. Copy them, or choose Quit again to leave without them.";
#[cfg(any(target_os = "macos", windows))]
const UNSAVED_AT_QUIT: &str = UNSAVED_AT_QUIT_TEXT;

/// What the box was showing him as he chose Quit - read then, on the main thread, because the
/// Quit itself changes it (it finishes a dictation, and puts the box away). Choosing Quit with a
/// warning in front of him is his answer to that warning, and to no other.
#[cfg(any(target_os = "macos", windows))]
#[derive(Debug, Default)]
struct Asked {
    /// The box was showing Quit's sentence over words that are saved nowhere else
    /// (`quit_confirmation_showing`).
    unsaved_seen: bool,
    /// Windows: what Huck's Clipboard held.
    #[cfg(windows)]
    held: Option<String>,
    /// The borrow whose "not back yet" warning was on the box.
    clipboard: Option<u64>,
    /// Windows: the words whose "paste what you need first" warning was on the box.
    #[cfg(windows)]
    words: Option<String>,
}

#[cfg(any(target_os = "macos", windows))]
impl Asked {
    fn now(app: &AppHandle) -> Asked {
        let state: State<App> = app.state();
        // One lock at a time: what the box shows first, then what was noted.
        #[cfg(windows)]
        let paste_first = on_screen(&state, PASTE_FIRST);
        Asked {
            unsaved_seen: quit_confirmation_showing(&state),
            #[cfg(windows)]
            held: held_on_huck_clipboard(),
            clipboard: agreed_to_leave_without_clipboard(&state),
            #[cfg(windows)]
            words: HUCK_UNTOLD.lock().agreed(paste_first),
        }
    }
}

/// The two things the box says before the program leaves with something of his. The first is
/// Windows' alone: there Huck's Clipboard leaves with the program.
#[cfg(windows)]
const PASTE_FIRST: &str = "Paste what you need first";
#[cfg(any(target_os = "macos", windows))]
const CLIPBOARD_NOT_BACK: &str = "Your clipboard isn't back yet";

/// What Huck's Clipboard holds, if it holds any words.
#[cfg(windows)]
fn held_on_huck_clipboard() -> Option<String> {
    crate::clip::huck::read().filter(|t| !t.trim().is_empty())
}

/// Words on Huck's Clipboard that the program put there while it was being asked to leave, and
/// that he has not yet answered a warning about.
///
/// On Windows Huck's Clipboard lives in this program's memory, so leaving empties it. Words that
/// a Quit put there - the dictation it finished, or one it found still being recognised - are
/// ones he has had no chance to paste, and the box went away without saying where they are.
/// Before the program leaves with them the box says so and that Quit stays; chosen again with
/// that warning in front of him, it goes. An ordinary Quit, with nothing of the kind on Huck's
/// Clipboard, leaves at once as it always has. (The update's rule is wider - any words he has
/// not been told about: `ToldWords`.)
///
/// Three ways such words used to get past this, each found by Codex:
/// - **noted only by a Quit that was otherwise free to go** (its fourth review): a first Quit
///   finished the dictation and stayed for a stuck clipboard; the second found the words
///   "already there", said nothing, and left with them. They are noted whether or not that Quit
///   can leave (`due`).
/// - **noted only if they arrived while the Quit was still waiting** (its fifth): recognition
///   that outlasted the twenty-second wait put them there afterwards, the box went, and the next
///   Quit left with them. They are noted where they are written (`deliver_words`), for any
///   dictation a Quit found unfinished (`QUIT_WAITED_FOR`), however much later that is.
/// - **counted as said the moment the warning was put up** (its third and fifth): he counts as
///   having answered only if the warning for these very words is on the box as he chooses Quit
///   (`Asked`), so one that something else replaced, or that he closed, is said again.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, Default, PartialEq, Eq)]
struct HuckUntold {
    /// The words, as they stand on Huck's Clipboard.
    words: Option<String>,
    /// The box has shown the warning for these words.
    told: bool,
}

#[cfg_attr(not(windows), allow(dead_code))]
impl HuckUntold {
    /// These words have just been put on Huck's Clipboard for a dictation a Quit asked for.
    fn note(&mut self, words: &str) {
        if self.words.as_deref() != Some(words) {
            *self = HuckUntold { words: Some(words.to_string()), told: false };
        }
    }

    /// The words he is agreeing to leave without: he has been told about them, and that warning
    /// is on the box as he chooses Quit.
    fn agreed(&self, warning_on_screen: bool) -> Option<String> {
        self.words.clone().filter(|_| self.told && warning_on_screen)
    }

    /// One Quit's look at them. `before` and `now`: what Huck's Clipboard held when Quit was
    /// chosen and holds once Quit has kept everything. `leaving`: nothing else is keeping this
    /// Quit from going. `agreed`: the words whose warning was in front of him when he chose it.
    ///
    /// The words he must be warned of now, if this Quit has to stay for them.
    fn due(
        &mut self,
        before: &Option<String>,
        now: &Option<String>,
        leaving: bool,
        agreed: Option<&str>,
    ) -> Option<String> {
        if let Some(arrived) = now.as_deref().filter(|_| now != before) {
            self.note(arrived);
        }
        if !leaving {
            return None;
        }
        let words = self.words.clone()?;
        // Replaced since by a dictation of his own: those words are no longer there to lose.
        let replaced = now.as_deref() != Some(words.as_str());
        // Or he was told about these very words, and chose Quit with that in front of him.
        let answered = self.told && agreed == Some(words.as_str());
        if replaced || answered {
            *self = HuckUntold::default();
            return None;
        }
        Some(words)
    }

    /// The box was asked to show the warning for `words`; `seen` says whether it is showing it.
    fn shown(&mut self, words: &str, seen: bool) {
        if self.words.as_deref() == Some(words) {
            self.told = seen;
        }
    }
}

/// One Quit's dealing with them (`HuckUntold`). `None`: nothing of the kind keeps this Quit.
/// `Some(seen)`: the warning was asked for, so this Quit stays, and `seen` says whether the box
/// is showing it. The lock is not held while the box is waited on, so words noted meanwhile
/// (`deliver_words`) are never overwritten.
#[cfg_attr(not(windows), allow(dead_code))]
fn huck_untold_at_quit(
    untold: &Mutex<HuckUntold>,
    before: &Option<String>,
    now: &Option<String>,
    leaving: bool,
    agreed: Option<&str>,
    show: impl FnOnce() -> bool,
) -> Option<bool> {
    let words = untold.lock().due(before, now, leaving, agreed)?;
    let seen = show();
    untold.lock().shown(&words, seen);
    Some(seen)
}

#[cfg(windows)]
static HUCK_UNTOLD: Mutex<HuckUntold> = Mutex::new(HuckUntold { words: None, told: false });

/// The dictation a Quit found unfinished (`keep_everything_before_exit`), by its generation.
/// When its words are put on Huck's Clipboard they are noted (`HuckUntold`) - during that Quit,
/// or long after it gave up waiting.
#[cfg(windows)]
static QUIT_WAITED_FOR: Mutex<Option<u64>> = Mutex::new(None);

/// A dictation's words have just been written to the clipboard he chose. If that is Huck's
/// Clipboard (`on_huck`) and a Quit asked for this dictation, they are noted.
#[cfg_attr(not(windows), allow(dead_code))]
fn note_words_a_quit_asked_for(
    untold: &Mutex<HuckUntold>,
    waited_for: Option<u64>,
    generation: u64,
    on_huck: bool,
    words: &str,
) {
    if on_huck && waited_for == Some(generation) {
        untold.lock().note(words);
    }
}

/// Put the warning that Huck's Clipboard holds words quitting would lose on the box, and say
/// whether the box is showing it.
#[cfg(windows)]
fn paste_first_shown(app: &AppHandle) -> bool {
    let state: State<App> = app.state();
    // The dictation Quit finished is still being put away, which clears whatever the box says
    // (`dismiss_now`). Waited for here, off the main thread, where the putting away takes its
    // turn: once the box is idle, that turn is over.
    let until = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while !matches!(*state.session.lock(), SessionState::Idle) && std::time::Instant::now() < until {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    // Shown and looked at in one turn on the main thread (`on_main`). A put-away that had set the
    // box idle and not yet hidden it used to hide this after it had been counted as seen.
    on_main(app, |app| {
        show_update(
            app,
            "warning",
            PASTE_FIRST.into(),
            "Your words are on Huck's Clipboard, and quitting empties it. Paste them where you \
             want them, then choose Quit again."
                .into(),
        );
        on_screen(&app.state::<App>(), PASTE_FIRST)
    })
    .unwrap_or(false)
}

/// The borrow whose stuck give-back the box has told him about (`left_without_his_clipboard`).
#[cfg(any(target_os = "macos", windows))]
static CLIPBOARD_TOLD: Mutex<Option<u64>> = Mutex::new(None);

/// The borrow he is agreeing to leave without: the box's warning about it is in front of him as
/// he asks to leave. **Taken at the ask itself**, before the asking changes what the box shows -
/// Open Update puts up "Installing…", and a Quit that finishes a dictation puts the box away; read
/// afterwards, the warning was always already gone, and Open Update could only ever say it again
/// (Codex's fourth review).
#[cfg(any(target_os = "macos", windows))]
fn agreed_to_leave_without_clipboard(state: &App) -> Option<u64> {
    let (borrow, _) = crate::clip::huck::unreturned()?;
    let told = *CLIPBOARD_TOLD.lock() == Some(borrow);
    (told && on_screen(state, CLIPBOARD_NOT_BACK)).then_some(borrow)
}

/// The program is asked to leave while his clipboard is out on loan for a paste and cannot be
/// given back - another program is holding the clipboard (Windows), or the clipboard will not
/// take his things (both; the Mac since 2026-10-04). What he had copied is never dropped silently: the first time, the box says so
/// and the program stays (the give-back keeps trying meanwhile, and a new copy of his own ends
/// it); asked again, it is let go, and `true` says the way is clear. (Codex's second review of
/// the 0.1.8 candidate.)
///
/// **He counts as told only while the box is showing it** (`on_screen`; Codex's third review). It
/// used to count the moment the message was set - and the box shows such a message only when idle,
/// so with *Copied* or a microphone problem on it the first ask said nothing he could see and the
/// second let his clipboard go. Now a message the box cannot show is not a telling, and one he
/// has closed, or that something else replaced, is said again rather than acted on.
///
/// `stage`: how the box shows it - `"warning"` is said in every resting state; `"ready"` keeps
/// Open Update's button, and shows only when the box is idle. `agreed`: the borrow whose warning
/// was in front of him when he asked (`agreed_to_leave_without_clipboard`).
#[cfg(any(target_os = "macos", windows))]
fn left_without_his_clipboard(
    app: &AppHandle,
    stage: &'static str,
    again: &str,
    to: &str,
    agreed: Option<u64>,
) -> bool {
    use crate::clip::huck::{self, Unreturned};
    let state: State<App> = app.state();
    // Only once nothing else stands in the way: a dictation still being kept is not this.
    if LEAVING.lock().starting > 0 {
        return false;
    }
    {
        let session = state.session.lock();
        if session.is_dictating() || matches!(*session, SessionState::Transcribing) {
            return false;
        }
    }
    let Some((borrow, why)) = huck::unreturned() else { return false };
    if agreed == Some(borrow) {
        eprintln!("[hvtt] leaving without his clipboard given back: he was told, and chose {again} again");
        huck::let_go(borrow);
        return huck::stop_borrowing(std::time::Duration::from_secs(2));
    }
    let what = match why {
        Unreturned::Busy => "goes back as soon as another program lets go of the clipboard",
        Unreturned::Refused => "could not be put back yet, and is being tried again",
    };
    let detail =
        format!("What you had copied before the last paste {what}. Choose {again} again to {to} without it.");
    // Shown and counted in one turn on the main thread (`on_main`), so no put-away can land
    // between the two.
    on_main(app, move |app| {
        show_update(app, stage, CLIPBOARD_NOT_BACK.into(), detail);
        let seen = on_screen(&app.state::<App>(), CLIPBOARD_NOT_BACK);
        *CLIPBOARD_TOLD.lock() = seen.then_some(borrow);
    });
    false
}

/// Is the box showing words that are saved nowhere else, where he can see them and copy them?
/// The box's own rule (`ui/app.js` `render`): a finished dictation whose copy failed shows its
/// words, with Copy and Discard - unless the shortcut prompt has the box.
#[cfg_attr(not(any(target_os = "macos", windows)), allow(dead_code))]
fn unsaved_on_screen(session: &SessionState, rebinding: bool, unsaved: bool) -> bool {
    unsaved && !rebinding && matches!(session, SessionState::Ready)
}

/// Is the box showing Quit's own sentence over those words - "choose Quit again to leave
/// without them"? That, and not the words alone, is what a second Quit answers (Codex's sixth
/// review: the words were on the box, the sentence was not - a shortcut error had the line - and
/// the second Quit left with them). `message` is the box's line; it shows whenever the words do
/// (`ui/app.js`: a box whose words were not copied always says its message, whatever else it
/// has to say). Anything that has since replaced the sentence - a Copy that failed again - means
/// he is asked again.
#[cfg_attr(not(any(target_os = "macos", windows)), allow(dead_code))]
fn quit_confirmation_on_screen(session: &SessionState, rebinding: bool, unsaved: bool, message: &str) -> bool {
    unsaved_on_screen(session, rebinding, unsaved) && message == UNSAVED_AT_QUIT_TEXT
}

/// `quit_confirmation_on_screen`, for the program as it stands.
#[cfg_attr(not(any(target_os = "macos", windows)), allow(dead_code))]
fn quit_confirmation_showing(state: &App) -> bool {
    // One lock at a time: `snapshot` holds the session while it reads the others.
    let rebinding = state.rebinding.lock().is_some();
    let unsaved = *state.words_unsaved.lock();
    let message = state.message.lock().clone();
    let session = state.session.lock().clone();
    quit_confirmation_on_screen(&session, rebinding, unsaved, &message)
}

/// What the box says when a finished dictation could not be copied. Its words are shown under
/// it, with Copy to try again. `only_here`: they are in no draft and no text box either.
/// `again`: this is not the first try.
fn not_copied_message(only_here: bool, again: bool) -> String {
    let start = if again { "Still couldn't copy" } else { "Couldn't copy" };
    let place = if only_here { "They are only here, below" } else { "They are below" };
    let next = match (again, only_here) {
        (false, _) => "Copy tries again.",
        (true, true) => "try Copy again in a moment, or Discard them.",
        (true, false) => "try Copy again in a moment.",
    };
    format!("{start} your words to the clipboard. {place} — {next}")
}

/// Try again to copy a finished dictation the clipboard would not take - and to draft it, when
/// drafts are on and no draft was written the first time. The box's Copy button, and what
/// closing the box, starting another dictation, and Quit do first when the words are saved
/// nowhere else (`keep_unsaved_words`). True when this call put the words on the clipboard.
///
/// **The whole of it is one turn on the main thread** (`on_main`): which words, the write, and
/// what the box then says. Discarding them and starting another dictation take their turns
/// there too, so the words written are always the ones the box is showing. Until Codex's sixth
/// review the write was made on the thread that asked and only the result was applied in turn:
/// a retry that was waiting for a busy clipboard while he discarded those words and dictated
/// again wrote the old words over the new dictation's copy - its only copy, with drafts off.
/// The price is that the main thread waits while another program holds the clipboard, up to a
/// second.
fn copy_again(app: &AppHandle) -> bool {
    on_main(app, copy_again_now).unwrap_or(false)
}

/// `copy_again`, in its turn on the main thread.
fn copy_again_now(app: &AppHandle) -> bool {
    let state: State<App> = app.state();
    // One lock at a time: `snapshot` holds the session while it reads the others.
    let ready = matches!(*state.session.lock(), SessionState::Ready);
    let not_copied = *state.not_copied.lock();
    if !ready || !not_copied {
        return false;
    }
    let transcript = state.transcript.lock().clone();
    let unsaved = *state.words_unsaved.lock();
    let delivered = *state.delivered.lock();
    let (choice, keep_drafts, paste_key) = {
        let s = state.settings.lock();
        (state.clipboard_in_use.choice(), s.keep_drafts, hvtt_core::settings::describe_shortcut(&s.paste_shortcut))
    };
    let system = clip::SystemClipboard::new(app.clone());
    #[cfg(any(target_os = "macos", windows))]
    let huck = clip::huck::HuckClipboard;
    let clipboard: &dyn hvtt_core::pipeline::Clipboard = match choice {
        #[cfg(any(target_os = "macos", windows))]
        ClipboardChoice::Huck => &huck,
        _ => &system,
    };
    // A draft only if none was written the first time: `unsaved` says neither copy exists.
    let report = hvtt_core::complete_transcription(
        &transcript,
        clipboard,
        None,
        state.drafts.as_ref().filter(|_| keep_drafts && unsaved),
    );
    let (copied, drafted) = (report.clipboard_ok, report.draft_path.is_some() || !unsaved);
    if copied {
        *state.not_copied.lock() = false;
    }
    if copied || drafted {
        *state.words_unsaved.lock() = false;
    }
    *state.message.lock() = if copied {
        match choice {
            ClipboardChoice::Huck => format!("On Huck's clipboard — press {paste_key} to paste."),
            ClipboardChoice::System => format!("Copied — press {NORMAL_PASTE} to paste."),
        }
    } else {
        not_copied_message(!drafted && !delivered, true)
    };
    copied
}

/// Words that are only in the box - copied nowhere, no draft - are not thrown away by closing
/// it, by starting another dictation, or by leaving: each makes one more try at keeping them
/// first (`copy_again`). True when they are safe now, or there were none.
///
/// Until 2026-10-03 the box said "copy it before closing" and showed nothing to copy, and each
/// of those three threw the only copy away (Codex's fifth review of the 0.1.8 candidate; in
/// every version since the transcript box was taken out on 2026-09-25, on both platforms).
fn keep_unsaved_words(app: &AppHandle) -> bool {
    let state: State<App> = app.state();
    if *state.words_unsaved.lock() {
        copy_again(app);
    }
    let still_unsaved = *state.words_unsaved.lock();
    !still_unsaved
}

/// The box's Copy button, shown when a finished dictation could not be copied.
#[tauri::command]
fn copy_words(app: AppHandle) {
    // Asked from a worker, done in its turn on the main thread (`copy_again`).
    std::thread::spawn(move || {
        let copied = copy_again(&app);
        push(&app);
        // They are on the clipboard now: said, briefly, and the box goes as it does for any
        // finished dictation.
        if copied {
            hide_after(&app, std::time::Duration::from_millis(2600));
        }
    });
}

/// The box's Discard button: he does not want the words that could not be copied. The one way,
/// besides a second Quit with them in front of him, that such words are let go.
#[tauri::command]
fn discard_words(app: AppHandle, generation: u64) {
    post_to_main(&app, move |app| {
        let state: State<App> = app.state();
        // Only the box it was pressed in, by its dictation's number: a late click must not let
        // go of a later dictation's words, which may be unsaved too.
        if !from_this_box(&state, generation) {
            return;
        }
        let ready = matches!(*state.session.lock(), SessionState::Ready);
        if !ready || !*state.not_copied.lock() {
            return;
        }
        eprintln!("[hvtt] words that could not be copied were discarded, at his choice");
        *state.words_unsaved.lock() = false;
        *state.not_copied.lock() = false;
        dismiss_now(app);
    });
}

/// Starting a dictation and leaving the program, decided under one lock. With a flag for leaving
/// alone, a shortcut press could find the program not leaving and then - before its dictation
/// showed as one - a Quit or an update could find nothing in progress and go: the dictation
/// started behind it and went with the program, nothing kept (Codex's review of the Windows 0.1.8
/// candidate; the Mac had the same gap since 0.1.6).
struct Leaving {
    /// The program is on its way out - a Quit, an update stepping aside, or the session ending:
    /// no new dictation may start, and the shortcut is ignored.
    leaving: bool,
    /// Dictations the shortcut has let in that do not yet show as one (`Admitted`).
    starting: u32,
}
static LEAVING: Mutex<Leaving> = Mutex::new(Leaving { leaving: false, starting: 0 });

#[cfg_attr(not(any(target_os = "macos", windows)), allow(dead_code))]
impl Leaving {
    /// Take the way out. False when a Quit or an update already has it.
    fn begin(&mut self) -> bool {
        !std::mem::replace(&mut self.leaving, true)
    }

    /// Not leaving after all: dictation works again.
    fn stay(&mut self) {
        self.leaving = false;
    }

    /// Let a dictation start - unless the program is leaving. Counted until it shows as one.
    fn admit(&mut self) -> bool {
        if !self.leaving {
            self.starting += 1;
        }
        !self.leaving
    }

    /// An admitted dictation now shows as one, or could not start.
    fn started(&mut self) {
        self.starting = self.starting.saturating_sub(1);
    }
}

/// A dictation the shortcut let in, for as long as it does not yet show as one. Leaving waits
/// for it (`keep_everything_before_exit`), then finishes and keeps it like any other.
struct Admitted(&'static Mutex<Leaving>);

impl Admitted {
    fn new() -> Option<Admitted> {
        Admitted::of(&LEAVING)
    }

    /// The lock is let go before an `Admitted` exists, and none exists unless the dictation was
    /// let in - because dropping one takes the lock again. Built inside the lock's own statement
    /// (`lock().admit().then_some(Admitted)`), a refused one was made, dropped with the lock still
    /// held, and stopped the program dead: the shortcut pressed while a Quit or an update waited
    /// froze it. It also counted down a start that was never counted up. (Codex's second review
    /// of the 0.1.8 candidate.)
    fn of(life: &'static Mutex<Leaving>) -> Option<Admitted> {
        let let_in = life.lock().admit();
        let_in.then(|| Admitted(life))
    }
}

impl Drop for Admitted {
    fn drop(&mut self) {
        self.0.lock().started();
    }
}

/// The generation whose unsaved words a Quit has already revealed. A timeout does not warn.
#[cfg(any(target_os = "macos", windows))]
static QUIT_WARNED_GENERATION: Mutex<Option<u64>> = Mutex::new(None);

/// The end is coming and cannot be put off (a logout; on Windows a sign-out or shutdown): before
/// anything slow, put the words already recognised where the finished ones would go - the chosen
/// clipboard, and a draft if drafts are on. If the last stretch then finishes in time it replaces
/// them; if not, only that last stretch is lost, never the whole dictation. (Codex's fourth review
/// of 0.1.6.) Audio is never written to disk.
#[cfg(any(target_os = "macos", windows))]
fn keep_words_so_far(app: &AppHandle) {
    let state: State<App> = app.state();
    let order = exit::CopyOrder::new(&state.final_copy_started);
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
        (state.clipboard_in_use.choice(), s.keep_drafts)
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
#[cfg(any(target_os = "macos", windows))]
static EXIT_ALLOWED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Before the program leaves, nothing of his may be only in flight (Codex's third review of
/// 0.1.6; the product rule). In order, within `limit`:
/// 1. a dictation still going - listening or paused - is finished the way closing the box finishes
///    it: the last words recognised, then drafted and copied, **not delivered**;
/// 2. that, or a delivery already under way, is waited for;
/// 3. no paste may borrow his clipboard any more, and one on loan is given back (`stop_borrowing`).
///
/// `true` when all of it is done. Callers take the way out first (`Leaving::begin`), so no new
/// dictation is let in.
#[cfg(any(target_os = "macos", windows))]
fn keep_everything_before_exit(app: &AppHandle, limit: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + limit;
    let state: State<App> = app.state();
    // A dictation let in a moment before the way out was taken does not show as one yet. It is
    // waited for - milliseconds - and only then looked for, so it is finished and kept like any
    // other. (Counted first, looked for second: it shows as a dictation before it stops counting.)
    while LEAVING.lock().starting > 0 {
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    // Windows: the dictation this leaving finds unfinished is marked, so that its words are
    // noted when they reach Huck's Clipboard - even if that is after the wait below has given
    // up (`HuckUntold`). The session is held while the generation is read, as `snapshot` does.
    #[cfg(windows)]
    {
        let session = state.session.lock();
        if session.is_dictating() || matches!(*session, SessionState::Transcribing) {
            *QUIT_WAITED_FOR.lock() = Some(*state.generation.lock());
        }
    }
    if state.session.lock().is_dictating() {
        finish(app.clone(), false, None);
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
    let left = deadline.saturating_duration_since(std::time::Instant::now());
    // A give-back already known to be stuck gets a moment, not the whole wait - the caller
    // tells him instead (`left_without_his_clipboard`).
    let left = if clip::huck::unreturned().is_some() { left.min(std::time::Duration::from_secs(2)) } else { left };
    clip::huck::stop_borrowing(left)
}

/// The dictation a held shortcut started (Dictation Style > Hold to Talk), by its number; 0 for
/// none. Fixed when the dictation starts, as its clipboard is: a change in the menu while he
/// holds applies from the next one.
static HELD_DICTATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// The dictation about to start is a held one: set around `toggle_now`, read by
/// `start_recording`. Both on the main thread.
static HOLD_NEXT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// A held dictation that letting go has not ended yet - so the two ways of noticing the release
/// (the shortcut's own report, and on Windows the keyboard hook) end it once.
static HOLD_OPEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The number the paste gate gave the held dictation's keys; 0 for none. A report that a held
/// key came up names its hold, and is acted on only if it is still this one.
static HOLD_KEYS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// What the box says while he holds.
const HOLD_LISTENING: &str = "Listening — let go to send";

/// Is dictation number `generation` a held one?
fn held(generation: u64) -> bool {
    generation != 0 && HELD_DICTATION.load(std::sync::atomic::Ordering::SeqCst) == generation
}

/// The dictation shortcut going down. Press style: start, or send. Hold style: start, and
/// letting go sends (`hold_released`).
fn dictate_pressed(app: AppHandle) {
    post_to_main(&app, |app| {
        use std::sync::atomic::Ordering::SeqCst;
        let state: State<App> = app.state();
        // A shortcut whose key the paste gate cannot watch is not held: it could neither end on
        // a modifier let go nor keep its key from typing. Press to start, press to send.
        let hold = state.settings.lock().dictation_style == DictationStyle::HoldToTalk
            && platform_paste::hold_key_known();
        HOLD_NEXT.store(hold, SeqCst);
        toggle_now(app, true);
        HOLD_NEXT.store(false, SeqCst);
    });
}

/// The dictation shortcut coming up - or any one of its keys (the paste gate's watch on the
/// keyboard, `hold_started`). Ends a held dictation and sends it; does nothing for any other. No stop *press* was
/// made, so the paste gate is told to expect none.
fn hold_released(app: &AppHandle) {
    use std::sync::atomic::Ordering::SeqCst;
    let state: State<App> = app.state();
    // One lock, let go, then the other: both in one condition would hold the dictation's number
    // while waiting for the session - the other way round from `snapshot`, which any thread
    // may be in the middle of (found in Claude's own re-read, 2026-10-05, before it froze anything).
    let generation = *state.generation.lock();
    let dictating = state.session.lock().is_dictating();
    if !held(generation) || !dictating {
        return;
    }
    // Leaving: the dictation is being finished and kept, as with the shortcut pressed again.
    if LEAVING.lock().leaving || !HOLD_OPEN.swap(false, SeqCst) {
        return;
    }
    finish(app.clone(), true, Some(false));
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
    // On the main thread, where the box is also put away (`on_main`): a put-away already under
    // way can never reset the dictation this starts. The shortcut and the menu arrive there
    // anyway, so this runs at once.
    post_to_main(&app, move |app| toggle_now(app, by_key));
}

fn toggle_now(app: &AppHandle, by_key: bool) {
    let state: State<App> = app.state();
    if state.rebinding.lock().is_some() {
        return;
    }
    let (dictating, busy) = {
        let session = state.session.lock();
        (session.is_dictating(), matches!(*session, SessionState::Transcribing))
    };
    if dictating {
        // Leaving: the dictation is being finished and kept, and the shortcut does nothing.
        if LEAVING.lock().leaving {
            return;
        }
        finish(app.clone(), true, Some(by_key));
    } else if !busy {
        // Words that are only in the box are not thrown away by starting again. One more try at
        // copying them; if the clipboard still will not take them the box says so, and no
        // dictation starts until he has copied or discarded them.
        if !keep_unsaved_words(app) {
            reveal_composer(app);
            push(app);
            return;
        }
        // Let in, or not, in the same turn of the lock that a Quit or an update takes the way
        // out in - so leaving can never look past a dictation that is on its way.
        let Some(admitted) = Admitted::new() else { return };
        start_recording(app.clone(), admitted);
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

/// Is a command from the box about the dictation that is the current one now? The box sends the
/// number of the dictation it was showing when the button was pressed. A click that arrives late -
/// that dictation over, another in the box - must do nothing to the other: until 2026-10-05 a
/// late Discard could let go of the next dictation's unsaved words, and a late edit could write
/// one dictation's words over another's (Codex's fourth review of hold to talk; both platforms,
/// older than that work).
fn from_this_box(state: &App, generation: u64) -> bool {
    *state.generation.lock() == generation
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
    if !from_this_box(&state, input.generation) {
        return;
    }
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
    if !from_this_box(&app.state::<App>(), input.generation) {
        return;
    }
    let dictating = app.state::<App>().session.lock().is_dictating();
    // Leaving: the dictation is being finished and kept, and Send - like the shortcut - does
    // nothing; it could otherwise get in ahead of the keeping and deliver (Codex's second review
    // of hold to talk).
    if dictating && !LEAVING.lock().leaving {
        // macOS: the click made this the active app, so the box does hold the keyboard; saying so
        // has `finish` hand it back, and wait for the caret, before anything is pasted.
        #[cfg(target_os = "macos")]
        {
            *app.state::<App>().box_has_keyboard.lock() = true;
        }
        finish(app.clone(), true, Some(false));
    }
}

/// He fixed the words in the box. Only while paused, and once the last words are in.
#[tauri::command]
fn edit_text(app: AppHandle, text: String, input: BoxInput) {
    note_input(input);
    let state: State<App> = app.state();
    if !from_this_box(&state, input.generation) {
        return;
    }
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
    if !from_this_box(&state, input.generation) {
        return;
    }
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
    // In its turn on the main thread (`on_main`), whoever asks.
    post_to_main(&app, dismiss_now);
}

/// Put the box away - if `still` holds once it is this request's turn on the main thread. For
/// the box's own timers and for a dictation closing its own box: what they looked at before
/// asking may have changed by the time the box is theirs to change, and a dictation started in
/// between must never be put away by a request that was meant for the one before it.
fn dismiss_if(app: &AppHandle, still: impl FnOnce(&App) -> bool + Send + 'static) {
    post_to_main(app, move |app| {
        if still(&app.state::<App>()) {
            dismiss_now(app);
        }
    });
}

/// Is a warning he has yet to answer on the box (stage `"warning"`)? Nothing that puts the box
/// away by itself - a timer, a dictation closing its own box - takes such a warning with it: it
/// stays until he closes it, answers it, or dictates again. (His answer is only ever counted
/// while it is on the box, so one that was put away would be said again; this keeps it from
/// being put away under him in the first place - Codex's sixth review.)
fn warning_up(state: &App) -> bool {
    state.update.lock().as_ref().is_some_and(|v| v.stage == "warning")
}

/// Leave the shortcut prompt, by any route, which puts every shortcut back. The keyboard is let
/// go of - always, whatever state the prompt is in - before the shortcuts go back on. Whether the
/// prompt was up.
fn end_rebinding(app: &AppHandle) -> bool {
    let state: State<App> = app.state();
    #[cfg(windows)]
    drop(state.key_capture.lock().take());
    let was_up = state.rebinding.lock().take().is_some();
    if was_up {
        *state.rebind_error.lock() = None;
        bind_all(app);
    }
    was_up
}

/// `dismiss`, in its turn on the main thread - the only place the box is put away (`on_main`).
fn dismiss_now(app: &AppHandle) {
    let state: State<App> = app.state();
    // Everything this decides, it decides with nothing able to change underneath
    // (`DICTATION_CHANGES`), and once. Looked at before the lock, a dictation could move on
    // before the reset below: a live pass settles words and a `finish` already asked for takes
    // the dictation - or goes all the way to "finished, and its words saved nowhere" - and the
    // reset then wiped words it had judged not to be there (Codex's third and fourth reviews of
    // hold to talk, 2026-10-05). Under the lock no `finish` can begin; one that has begun shows
    // as Transcribing, and one that is done shows what it left. Let go before the box is hidden.
    let changing = DICTATION_CHANGES.lock();
    let (busy, dictating) = {
        let session = state.session.lock();
        (matches!(*session, SessionState::Transcribing), session.is_dictating())
    };
    if busy {
        // The words are about to be copied and delivered.
        return;
    }
    if dictating {
        // No pass settles anything after this, so what is looked at next is all there will be.
        state.live_epoch.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if dictation_has_words(&state) {
            drop(changing);
            finish(app.clone(), false, None);
            return;
        }
    }
    // Words that are saved nowhere else - the clipboard would not take them, and no draft was
    // written - are not thrown away by closing the box. It shows them, with Copy and Discard.
    if !dictating && *state.words_unsaved.lock() {
        // The shortcut prompt over them goes first, and they come back into view.
        if end_rebinding(app) {
            push(app);
            return;
        }
        // Closing keeps a dictation's words, so it tries the copy once more; if the clipboard
        // still will not take them the box stays, and says so.
        if !keep_unsaved_words(app) {
            reveal_composer(app);
            push(app);
            return;
        }
    }
    if std::mem::take(&mut *state.box_has_keyboard.lock()) {
        hand_back_keyboard(app, false);
    }
    state.live_epoch.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    *state.live.lock() = Live::default();
    *state.generation.lock() += 1;
    end_rebinding(app);
    // Nothing heard worth keeping (checked above): the recording just stops.
    if state.recording.lock().take().is_some() {
        prepare_microphone(app);
    }
    *state.level.lock() = 0.0;
    *state.delivered.lock() = false;
    *state.ask_permission.lock() = false;
    if !matches!(state.update.lock().as_ref(), Some(v) if matches!(v.stage, "checking" | "downloading" | "installing")) {
        *state.update.lock() = None;
    }
    *state.transcript.lock() = Transcript::empty();
    *state.words_unsaved.lock() = false;
    *state.not_copied.lock() = false;
    *state.message.lock() = String::new();
    *state.elapsed_ms.lock() = 0;
    state.set_state(SessionState::Idle);
    drop(changing);
    // The dictation is over, so let go of where it went. On Windows this is also what ends the
    // click and key count behind the paste gate, which runs only while a dictation needs it.
    #[cfg(windows)]
    {
        *state.destination.lock() = None;
    }
    hide_composer(app);
    push(app);
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

/// `admitted`: the shortcut's leave to start (`toggle_by`), held until the dictation shows as
/// one - or until this returns without one.
fn start_recording(app: AppHandle, admitted: Admitted) {
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
        // Named here, at the keypress, so names are in the order dictations started.
        let token = crate::destination::chromium::next_pin_token();
        std::thread::spawn(move || crate::destination::chromium::ChromiumDestination::pin(bridge, token))
    };

    // 1. THE INDICATOR, before anything that can block. The budget is 150 ms from keypress to
    //    visible; it is the most felt number in the product.
    // A new dictation starts empty. The previous words are already in their text box, or on
    // the clipboard and in the recovery draft; appending them would send old words somewhere new.
    // From here until this dictation's recording is stored, one piece as far as a `finish`
    // worker can tell (`DICTATION_CHANGES`).
    let changing = DICTATION_CHANGES.lock();
    let generation = {
        let mut g = state.generation.lock();
        *g += 1;
        *g
    };
    crate::destination::box_input::reset(generation);
    // Held (Hold to Talk), or not: decided here, for the whole dictation.
    let hold = HOLD_NEXT.swap(false, std::sync::atomic::Ordering::SeqCst);
    HELD_DICTATION.store(if hold { generation } else { 0 }, std::sync::atomic::Ordering::SeqCst);
    HOLD_OPEN.store(hold, std::sync::atomic::Ordering::SeqCst);
    // From here until the shortcut's own key comes up, its auto-repeat reaches no program, and
    // any one of the shortcut's keys coming up ends the dictation - this hold's keys, by its
    // own number, so a late word about an earlier hold ends nothing.
    HOLD_KEYS.store(if hold { platform_paste::hold_started() } else { 0 }, std::sync::atomic::Ordering::SeqCst);
    state.live_epoch.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    *state.live.lock() = Live::default();
    *state.box_has_keyboard.lock() = false;
    *state.transcript.lock() = Transcript::empty();
    *state.delivered.lock() = false;
    // This dictation's clipboard, for its paste and its copy alike, whatever the menu says later.
    state.clipboard_in_use.start(&state.settings.lock());
    // Nothing is thrown away here: `toggle_now` lets no dictation start over words that are
    // saved nowhere else.
    *state.words_unsaved.lock() = false;
    *state.not_copied.lock() = false;
    #[cfg(any(target_os = "macos", windows))]
    {
        *state.final_copy_started.lock() = false;
    }
    *state.ask_permission.lock() = false;
    // A finished update message gives way; a check still running re-shows itself when done.
    if !matches!(state.update.lock().as_ref(), Some(v) if matches!(v.stage, "checking" | "downloading" | "installing")) {
        *state.update.lock() = None;
    }
    state.set_state(SessionState::Recording);
    // It shows as a dictation now, so leaving finds it by looking; it need not be counted.
    drop(admitted);
    // Counted again now it shows as recording: a speed check that found nothing happening read the
    // count before this, so it is given up (Codex's eighteenth review).
    state.live_epoch.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    *state.message.lock() = if hold { HOLD_LISTENING.into() } else { "Listening…".into() };
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
            drop(changing);
            state.timings.lock().shortcut_to_capture_ms = pressed.elapsed().as_millis();
            // Held: a key of the shortcut let go while the microphone was opening. Looked for
            // here, not where the hold began - only now is there a recording to end.
            if hold && platform_paste::hold_already_let_go(HOLD_KEYS.load(std::sync::atomic::Ordering::SeqCst)) {
                hold_released(&app);
            }
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

    // 3. Validation, off the critical path, using ONLY what was captured in step 0 - this
    //    dictation's number and its clipboard included, by value. What it finds is applied in
    //    one turn on the main thread, and only if this is still the dictation in the box
    //    (`place_pin`): the worker can outlive its dictation (an app slow to answer
    //    Accessibility), and used to write its destination into whichever dictation was running
    //    by then - whose words then went, silently, into the first one's text box (Codex's
    //    eighth review of the 0.1.8 candidate; both platforms, older than that work).
    let app2 = app.clone();
    #[cfg(any(target_os = "macos", windows))]
    let borrow = state.clipboard_in_use.paste_borrows();
    std::thread::spawn(move || {
        let browser = browser_pin.join().ok();
        let found = std::cell::RefCell::new(Pinned::default());
        #[cfg(target_os = "macos")]
        resolve_pin(pending, stamp, browser, borrow, &found);
        #[cfg(windows)]
        resolve_pin_windows(&app2, stamp, browser, borrow, &found);
        #[cfg(not(any(target_os = "macos", windows)))]
        let _ = browser;
        let found = found.into_inner();
        post_to_main(&app2, move |app| {
            let state: State<App> = app.state();
            // macOS: whether Accessibility is granted is a fact about the program, not about
            // this dictation, so it is kept up even when the dictation is over - asked afresh
            // here rather than taken from a worker that may have asked long ago.
            #[cfg(target_os = "macos")]
            if found.accessibility.is_some() {
                *state.ax_trusted.lock() = accessibility_ready();
            }
            let session = state.session.lock().clone();
            let now = *state.generation.lock();
            let Some(found) = place_pin(&state.destination, found, generation, now, &session) else {
                eprintln!("[hvtt] a destination found for a dictation that is over was dropped");
                return;
            };
            if let Some(label) = found.label {
                // Still listening or paused: say where it will go. Later, the line is busy.
                if session.is_dictating() {
                    *state.message.lock() = if held(generation) {
                        format!("{HOLD_LISTENING} to {label}")
                    } else {
                        format!("Listening — will send to {label}")
                    };
                }
            }
            if let Some(e) = found.note {
                *state.pin_note.lock() = Some(e.message());
                if !e.is_expected() {
                    *state.message.lock() = e.message();
                }
            }
            // macOS: the one-time Accessibility ask, once per launch.
            #[cfg(target_os = "macos")]
            if found.ask_permission && !std::mem::replace(&mut *state.permission_asked.lock(), true) {
                *state.ask_permission.lock() = true;
                crate::destination::macos_ax::request_accessibility();
            }
            push(app);
        });
    });
}

/// What a pin worker found for one dictation (`resolve_pin`, `resolve_pin_windows`). The worker
/// changes nothing itself: this is handed to the main thread and applied there (`place_pin`).
#[derive(Default)]
struct Pinned {
    destination: Option<Box<dyn Destination>>,
    /// Why there is none, in one line for the box.
    note: Option<PinError>,
    /// macOS: Accessibility has not been granted, and the one-time ask is due.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    ask_permission: bool,
    /// macOS: the worker asked whether Accessibility is granted. It does not write the answer
    /// itself (`App::ax_trusted`): the main thread asks again in its own turn, so a worker that
    /// finishes late cannot put an old answer over a grant made since.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    accessibility: Option<bool>,
}

/// What is left to say once a destination has been placed.
struct Placed {
    label: Option<String>,
    note: Option<PinError>,
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    ask_permission: bool,
}

/// Is a pin worker's result still for the dictation in the box? Only if no dictation has
/// started and none has been put away since (`generation`), and its words have not yet gone:
/// listening, paused, or being recognised.
fn pin_still_wanted(for_generation: u64, generation: u64, session: &SessionState) -> bool {
    for_generation == generation
        && matches!(session, SessionState::Recording | SessionState::Paused | SessionState::Transcribing)
}

/// Put a worker's destination in place - if it is still wanted (`pin_still_wanted`). `None`:
/// it was for a dictation that is over, and nothing was touched. Called in a turn on the main
/// thread, where dictations start and are put away, so the answer cannot change under it.
fn place_pin(
    slot: &Mutex<Option<Box<dyn Destination>>>,
    found: Pinned,
    for_generation: u64,
    generation: u64,
    session: &SessionState,
) -> Option<Placed> {
    if !pin_still_wanted(for_generation, generation, session) {
        return None;
    }
    let label = found.destination.as_ref().map(|d| d.label());
    if let Some(d) = found.destination {
        *slot.lock() = Some(d);
    }
    Some(Placed { label, note: found.note, ask_permission: found.ask_permission })
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
    // Windows too, since 2026-10-03: a built-but-stopped stream does not light its "using your
    // microphone" sign - Windows' own privacy record shows no use until the stream starts
    // (`examples/mic_sign.rs`, on Windows 10) - and first sound comes after about 27 ms instead
    // of 47-62.
    if std::env::var_os("HVTT_COLD_MIC").is_some() {
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
    borrow: bool,
    found: &std::cell::RefCell<Pinned>,
) {
    use crate::destination::windows_paste::PasteDestination;
    use crate::destination::{is_chromium_executable, is_unsupported_executable, windows_uia};

    let _ = app;
    // Found, not applied: the main thread places it, if this dictation is still the one.
    let set = |d: Box<dyn Destination>| found.borrow_mut().destination = Some(d);
    let note = |e: PinError| found.borrow_mut().note = Some(e);

    // Nothing to type into was in front: the desktop, the taskbar, or the H's own menu.
    let Some(stamp) = stamp else {
        // Let go of the browser's pin - this dictation's own, by name (`release`).
        if let Some(Ok(d)) = &browser {
            d.release();
        }
        note(PinError::NotATextField);
        return;
    };

    let exe = windows_uia::executable_of(stamp.pid());
    let app_label = is_unsupported_executable(&exe).unwrap_or_else(|| windows_uia::app_name(&exe));
    // `borrow`: the clipboard this dictation started on, not what the menu says now.
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
    } else if let Some(Ok(d)) = &browser {
        // Not our destination; release it so the extension is not left holding a field.
        d.release();
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
    pending: hvtt_core::pinning::PendingPin<crate::destination::macos_ax::AxElement>,
    stamp: Option<crate::destination::macos_paste::FocusStamp>,
    browser: Option<Result<crate::destination::chromium::ChromiumDestination, String>>,
    borrow: bool,
    found: &std::cell::RefCell<Pinned>,
) {
    use crate::destination::macos_paste::{PasteDestination, SameWindow};
    use crate::destination::{is_chromium_executable, is_unsupported_executable, macos_ax};

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
    // `borrow`: the clipboard this dictation started on, not what the menu says now.
    // The box itself, when the app names it: lets him click away and come back (2026-09-29).
    let field = pending.candidate().cloned();
    eprintln!(
        "[hvtt] {app_label}: focused {}",
        field.as_ref().map(|f| f.role_name()).unwrap_or_else(|| "nothing".into())
    );
    let paste = || {
        stamp.map(|s| PasteDestination::new(s, app_label.clone(), borrow).with_field(field.clone()))
    };

    // Found, not applied: the main thread places it, if this dictation is still the one.
    let set = |d: Box<dyn Destination>| found.borrow_mut().destination = Some(d);
    let note = |e: PinError| found.borrow_mut().note = Some(e);

    // The extension is the only silent way into Chrome, and it needs no macOS permission.
    // Any text box is a destination, password boxes included (decided with him 2026-09-28).
    if is_chromium_executable(&exe) {
        if let Some(Ok(d)) = browser {
            set(Box::new(d));
            return;
        }
    } else if let Some(Ok(d)) = &browser {
        // Not our destination; release it - this dictation's own pin, by name - so the
        // extension is not left holding a field.
        d.release();
    }

    // Everything below writes or pastes into another app, and macOS allows neither without
    // Accessibility. Its own prompt adds the app to the list - the plain check never did, which
    // left nothing to switch on. Once per launch.
    // Whether it is granted is for the menu too (`ax_trusted`), and is written where this
    // worker's other findings are applied: on the main thread, by `Pinned::accessibility`.
    let trusted = macos_ax::accessibility_trusted();
    found.borrow_mut().accessibility = Some(trusted);
    if !trusted {
        // The ask itself is made on the main thread, with the rest of what was found.
        found.borrow_mut().ask_permission = true;
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

/// May a live pass write what it found? Asked **with the words locked**, and answered by the
/// epoch: everything that resets them - putting the box away, a new dictation - moves the epoch
/// first and resets second, so with the words locked either the epoch has moved (refused) or
/// the reset has not happened yet (and will wipe whatever is written now). Looked at before the
/// lock, as it was, the answer could be a dictation old: a pass on a silent window - cursor
/// moved on, no words - was let through after the box had been put away and another dictation
/// started, the "same place?" check passing because both sat at nought, and the new dictation's
/// first seconds were skipped as already heard (Codex's ninth review of the 0.1.8 candidate).
fn still_this_pass(live: &Live, epoch_now: &std::sync::atomic::AtomicU64, epoch: u64, from: usize) -> bool {
    epoch_now.load(std::sync::atomic::Ordering::SeqCst) == epoch && live.heard_upto == from
}

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

/// Where a live pass starts reading the recording (16 kHz samples): at the first sound not yet
/// recognised for good. A held window stops that point moving for the rest of the dictation
/// (`Live::hold_at`), and reading on from it would mean conditioning and recognising minutes of
/// sound, several times a second, for words that are only shown - a slower computer stalls on
/// it. So from then on only the newest window's worth is read. The stop press and a pause still
/// recognise all of it (`recognise_rest`). (Codex's tenth review.)
fn preview_from(heard_upto: usize, recorded: usize, window: usize, held: bool) -> usize {
    if held {
        heard_upto.max(recorded.saturating_sub(window))
    } else {
        heard_upto
    }
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
        // Held: nothing is shown - but the words are still recognised as he talks, at the
        // lighter pace, whatever Live Words says. They are what a Quit, a sign-out or a shutdown
        // keeps (`keep_words_so_far`); without them a held dictation cut short by a shutdown
        // had nothing recognised at all, and four seconds to do the whole of it (Codex's review
        // of hold to talk, 2026-10-05).
        let live_words = if held(generation) { LiveWords::Lighter } else { state.settings.lock().live_words };
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
        let window = (hvtt_core::engine::WINDOW_SECS * hvtt_core::audio::WHISPER_SAMPLE_RATE as f32) as usize;
        let held = state.live.lock().hold_at == Some(from);
        // Only what is not yet recognised for good - and, once a window is held, only the newest
        // of that (`preview_from`).
        let Some((rest, skipped)) = state.recording.lock().as_ref().map(|r| {
            let start = preview_from(from, r.recorded_len(), window, held);
            (r.peek_from(start), start > from)
        }) else {
            break;
        };

        // A full window waiting (`WINDOW_SECS`): recognised for good, with full care, up to its
        // last whole sentence. A recognition that fails (rather than hearing nothing) moves
        // nothing on, so the window is tried again.
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
        // Only shown, and replaced moments later: one quick attempt, and of one window at most -
        // a slower computer can be further behind than that, and a preview of it all would put
        // it further behind still. Words passed over are marked, so the gap does not read as
        // words lost.
        let shown = &rest[cut..];
        let tail = recognise(&engine, &shown[..shown.len().min(window)], &hints(&state, &so_far), give_up(), true)
            .ok()
            .flatten()
            .map(|words| if skipped { format!("… {words}") } else { words });
        breather = live_words
            .rest_after(pass_started.elapsed())
            .unwrap_or(std::time::Duration::from_millis(300));

        // Written only if nothing paused, resumed or finished the dictation in the meantime.
        {
            let _pass = state.live_pass.lock();
            let same = *state.generation.lock() == generation;
            let current = same && state.session.lock().is_capturing();
            // The last look is taken with the words locked (`still_this_pass`).
            let mut live = state.live.lock();
            if !current || !still_this_pass(&live, &state.live_epoch, epoch, from) {
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
///
/// **Of the dictation it was asked for, and no other** (Codex's third review of hold to talk,
/// 2026-10-05; both platforms, older than that work). This is a worker: by the time it runs, or
/// by the time its recognition is done, that dictation may have been closed and another begun -
/// which it then marked Paused, and whose first seconds it then marked as already heard. The
/// dictation is named where Pause is asked for, and both of this worker's changes are made
/// only while it is still the one (`DICTATION_CHANGES`).
fn pause(app: AppHandle) {
    let wanted = *app.state::<App>().generation.lock();
    std::thread::spawn(move || {
        let state: State<App> = app.state();
        let _pass = state.live_pass.lock();
        let (from, before, rest) = {
            let _same = DICTATION_CHANGES.lock();
            if *state.generation.lock() != wanted {
                return;
            }
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
            (from, before, rest)
        };
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
            // Still that dictation: another's words and place in its sound are not this one's
            // to write.
            let _same = DICTATION_CHANGES.lock();
            if *state.generation.lock() != wanted {
                return;
            }
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
///
/// Of the dictation it was asked for, as `pause` is - and only if its recording is still there:
/// without one, "Recording" would be a dictation nothing could finish.
fn resume(app: AppHandle) {
    let wanted = *app.state::<App>().generation.lock();
    std::thread::spawn(move || {
        let state: State<App> = app.state();
        let _pass = state.live_pass.lock();
        {
            let _same = DICTATION_CHANGES.lock();
            if *state.generation.lock() != wanted {
                return;
            }
            if !matches!(*state.session.lock(), SessionState::Paused) {
                return;
            }
            {
                let rec = state.recording.lock();
                let Some(r) = rec.as_ref() else { return };
                r.resume();
            }
            state.live_epoch.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            state.set_state(SessionState::Recording);
        }
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
///
/// **The first ending asked for is the one a dictation gets, and it is that dictation's alone**
/// (Codex's review of hold to talk, 2026-10-05). The session only shows as ending once this
/// worker has its turn, so until then a second request could still be made - he closes the box
/// while holding, then lets go - and whichever worker ran first decided whether his words were
/// sent or only kept. And a worker that waited could end the dictation *after* the one it was
/// asked about. So the dictation is named here, where the ending is asked for, and asked once.
///
/// `stopped_by_key`: what the paste gate is to expect of this ending - a stop press, or none -
/// when the caller knows. Noted only if this request is the one taken: a request that is
/// refused must change nothing about the one that was (Codex's second review).
///
/// Whether the request was taken.
fn finish(app: AppHandle, deliver: bool, stopped_by_key: Option<bool>) -> bool {
    use std::sync::atomic::Ordering::SeqCst;
    static ASKED_FOR: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(u64::MAX);
    let wanted = *app.state::<App>().generation.lock();
    if ASKED_FOR.swap(wanted, SeqCst) == wanted {
        return false;
    }
    if let Some(by_key) = stopped_by_key {
        crate::destination::box_input::set_stopped_by_key(by_key);
    }
    std::thread::spawn(move || {
        let state: State<App> = app.state();
        // A worker that ends nothing leaves the dictation free to be ended by a later request.
        let not_taken = || {
            let _ = ASKED_FOR.compare_exchange(wanted, u64::MAX, SeqCst, SeqCst);
        };
        let (from, before, rec, listening) = {
            // A pause still finishing its words completes first.
            let _pass = state.live_pass.lock();
            // Its dictation, still, from here until the recording is this worker's and the
            // session says so: nothing may put the box away or start another in between
            // (`DICTATION_CHANGES`), or this would take the next dictation's recording.
            let _same = DICTATION_CHANGES.lock();
            if *state.generation.lock() != wanted {
                return not_taken();
            }
            let listening = {
                let session = state.session.lock();
                if !session.is_dictating() {
                    return not_taken();
                }
                matches!(*session, SessionState::Recording)
            };
            let Some(rec) = state.recording.lock().take() else { return not_taken() };
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
    true
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
    // Which dictation this is. It cannot change while its words are being recognised - nothing
    // starts and nothing is put away meanwhile - so this is the one a Quit marked.
    let generation = *state.generation.lock();
    *state.transcript.lock() = Transcript::settled(text);

    let (choice, paste_key) = {
        let s = state.settings.lock();
        (state.clipboard_in_use.choice(), hvtt_core::settings::describe_shortcut(&s.paste_shortcut))
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
    #[cfg(any(target_os = "macos", windows))]
    exit::CopyOrder::new(&state.final_copy_started).start_final();
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

    // Windows: Huck's Clipboard leaves with the program. Words put there for a dictation a Quit
    // asked for are noted here, where they are written: the Quit may have stopped waiting long
    // ago, and the next one must not find them "already there" (Codex's fifth review).
    #[cfg(windows)]
    {
        let waited_for = *QUIT_WAITED_FOR.lock();
        let on_huck = choice == ClipboardChoice::Huck && report.clipboard_ok;
        note_words_a_quit_asked_for(&HUCK_UNTOLD, waited_for, generation, on_huck, &report.text);
    }

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
        // The box shows the words under this, with Copy to try again (`ui/app.js`).
        _ if !report.clipboard_ok => {
            eprintln!(
                "[hvtt] the words could not be copied: {}",
                report.clipboard_error.as_deref().unwrap_or("no reason given")
            );
            not_copied_message(report.draft_path.is_none() && !delivered, false)
        }
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
    *state.not_copied.lock() = !report.clipboard_ok;
    *state.words_unsaved.lock() = !report.text_is_safe();
    state.set_state(SessionState::Ready);
    log_latency(&state);
    push(app);
    // Closed rather than sent: the words are kept, and the box goes as he asked. Only this
    // dictation's box: one started in the instant since is left alone (`dismiss_if`).
    if ending == Ending::Close && report.clipboard_ok {
        dismiss_if(app, move |state| *state.generation.lock() == generation && !warning_up(state));
        return;
    }
    // No text box afterwards: the words are in the field, or on the clipboard. Say which,
    // briefly, and get out of the way. Two things hold the box up: a failed clipboard (the
    // words are shown in it, with Copy to try again - they may be nowhere else) and the
    // one-time permission ask, which needs a click.
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
    // whisper.cpp must not be touched before Vulkan's loader is in (`vulkan_ready`).
    #[cfg(windows)]
    if !engine_whisper::vulkan_ready() {
        eprintln!("[hvtt] no vulkan-1.dll; the voice detector is left out");
        return;
    }
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

    let loaded = load_engine(app, &path, file);
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

/// The engine for `file`. On Windows, wherever this computer runs it quickest - one of its
/// graphics cards or the processor (`engine_whisper::load_fastest`) - found once for this version,
/// model and set of cards, and remembered. A remembered card is checked in the hidden child on
/// each load because its driver can change; a failed card becomes a remembered processor.
#[cfg(windows)]
fn load_engine(
    app: &AppHandle,
    path: &std::path::Path,
    file: &str,
) -> Result<engine_whisper::WhisperEngine, hvtt_core::engine::EngineError> {
    let state: State<App> = app.state();
    let cards = engine_whisper::graphics_cards();
    let key = format!("{} {file} {}", app.package_info().version, cards.join(" | "));
    let known = {
        let settings = state.settings.lock();
        (settings.fastest_for == key).then(|| settings.fastest.clone())
    };
    let (engine, on) = engine_whisper::WhisperEngine::load_fastest(path, known.as_deref())?;
    eprintln!("[hvtt] {file} runs on: {on}");
    if known.as_deref() != Some(on.as_str()) {
        update_settings(app, |s| {
            s.fastest_for = key;
            s.fastest = on;
        });
    }
    Ok(engine)
}

#[cfg(not(windows))]
fn load_engine(
    _app: &AppHandle,
    path: &std::path::Path,
    _file: &str,
) -> Result<engine_whisper::WhisperEngine, hvtt_core::engine::EngineError> {
    engine_whisper::WhisperEngine::load(path)
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

/// One update check at a time.
static UPDATE_WORK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The speed check: one at a time, and whether its result has been asked for. Under one lock, so
/// a request can never fall between a check's last look and its end and go unanswered (Codex's
/// twentieth review; until 2026-10-03 these were two separate flags).
struct SpeedWork {
    running: bool,
    /// Asked for - from the menu, or after a switch made from its offer - so the result is said
    /// even if all is well.
    asked: bool,
}
static SPEED: Mutex<SpeedWork> = Mutex::new(SpeedWork { running: false, asked: false });

impl SpeedWork {
    /// A check is wanted (`announce`: and its result said). Whether one must be started for it -
    /// not if one is running, which will answer.
    fn request(&mut self, announce: bool) -> bool {
        self.asked |= announce;
        !std::mem::replace(&mut self.running, true)
    }

    /// The check answers for what was asked: taken, so each request is answered exactly once.
    fn answer(&mut self) -> bool {
        std::mem::take(&mut self.asked)
    }

    /// A check has ended. Whether another must run - one was asked for after this one answered.
    fn another(&mut self) -> bool {
        if !self.asked {
            self.running = false;
        }
        self.asked
    }
}

/// A speed check nobody asked for stays out of sight this long. Past it, the box says what is
/// happening (`show_measuring`): on a quick computer the whole check is over by then.
const QUIET_CHECK: std::time::Duration = std::time::Duration::from_secs(3);

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
///
/// **It is never silent for long** (his report from the PC, 2026-10-03: with "Best" there the test
/// takes most of a minute, and the box only appeared with the verdict - "you need that prompt up
/// so they know that you're working"). Asked for, the box comes up at once and says a check is
/// under way (`show_measuring`). Not asked for, it does the same once the check has taken more
/// than `QUIET_CHECK` - and a check that has shown itself always ends by saying what it found.
fn speed_check(app: &AppHandle, announce: bool) {
    // The box first, the request second. The other way round, a check that was just ending
    // could answer the request and be gone before the box said "checking" - which then stayed,
    // with nothing left running to replace it (Codex's review of the 0.1.8 candidate). This way
    // a check that is ending either sees the box and answers it, or a new one is started below.
    if announce {
        show_measuring(app);
    }
    // Asked for while one runs: that one says its result (Codex's nineteenth review).
    if !SPEED.lock().request(announce) {
        return;
    }
    let app = app.clone();
    std::thread::spawn(move || loop {
        speed_check_once(&app);
        // Asked for again while that one was finishing: answered by another.
        if !SPEED.lock().another() {
            break;
        }
    });
}

/// The box while the speed check runs: what it is doing, and with which model.
fn show_measuring(app: &AppHandle) {
    let state: State<App> = app.state();
    let model = state.settings.lock().model.clone();
    let name = hvtt_core::models::find(&model).map_or("this model", |m| m.name);
    speed_notice(
        app,
        UpdateView {
            stage: "measuring",
            title: "Checking this computer's speed…".into(),
            detail: format!(
                "A short test with {name}; the microphone stays off. On a slower computer this \
                 can take a minute or more."
            ),
            action: None,
        },
        None,
    );
}

/// Does this check owe him its result in words? He asked for it - taken here, so every request
/// is answered exactly once - or the box is already saying that a check is under way. Every way
/// out of `speed_check_once` asks this.
fn speed_result_wanted(state: &App) -> bool {
    let asked = SPEED.lock().answer();
    asked || state.update.lock().as_ref().is_some_and(|v| v.stage == "measuring")
}

/// One speed check, start to finish (`speed_check`).
fn speed_check_once(app: &AppHandle) {
    let state: State<App> = app.state();
    let busy = |state: &App| {
        let session = state.session.lock();
        session.is_dictating() || matches!(*session, SessionState::Transcribing)
    };
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
                if speed_result_wanted(&state) {
                    speed_notice(app, notice("Couldn't check the speed just now",
                        "It waits for dictation and model changes to finish. Try again in a moment.".into()), Some(8000));
                }
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
            epoch = current_epoch();
        }
        let measured_under = *state.model_changes.lock();
        let Some(engine) = state.engine.lock().clone() else {
            // No model to test: said, when he asked, with the reason the menu shows.
            if speed_result_wanted(&state) {
                let why = state.engine_status.lock().clone();
                speed_notice(app, notice("Couldn't check the speed", why), Some(8000));
            }
            return;
        };
        let model_file = state.settings.lock().model.clone();
        let give_up = Some(hvtt_core::engine::GiveUp { counter: state.live_epoch.clone(), value: epoch });
        let request = TranscriptionRequest { samples: sound.clone(), vocabulary_prompt: None, give_up, provisional: false };
        // The two passes run on their own thread, so this one can notice how long they are
        // taking and bring the box up.
        let (done, passes) = std::sync::mpsc::channel();
        {
            let (engine, live_epoch) = (engine.clone(), state.live_epoch.clone());
            std::thread::spawn(move || {
                let warm = engine.transcribe(&request);
                let unbroken = live_epoch.load(std::sync::atomic::Ordering::SeqCst) == epoch;
                let _ = done.send(if unbroken && warm.is_ok() { engine.transcribe(&request) } else { warm });
            });
        }
        let timed = passes.recv_timeout(QUIET_CHECK).or_else(|_| {
            show_measuring(app);
            passes.recv()
        });
        if current_epoch() != epoch {
            continue;
        }
        let Ok(Ok(result)) = timed else {
            if speed_result_wanted(&state) {
                speed_notice(app, notice("Couldn't check the speed",
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
    let announce = speed_result_wanted(&state);
    let ms = result.elapsed_ms;
    let secs = |ms: u128| format!("{:.1} s", ms as f32 / 1000.0);
    let key = format!("{} {model_file}", app.package_info().version);
    update_settings(app, |s| s.speed_checked = key);
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
    // The offer below is of a more accurate model, not a quicker one.
    let mut better = false;
    let (view, hide_after) = match hvtt_core::models::speed_advice(ms) {
        // Quick enough for a more accurate model: said once unasked, and whenever he asks.
        // Windows only: there the model inside the program is Quick, and a graphics card can
        // carry far more. On the Mac the one inside is already Best, and a model he chose
        // himself is left alone (Codex's review of the graphics-card work, 2026-10-05).
        SpeedAdvice::Fine => match current
            .filter(|_| cfg!(windows))
            .and_then(|c| hvtt_core::models::better_choice(c, ms))
            .filter(|_| announce || !state.settings.lock().better_offered)
        {
            Some((offer, estimate)) => {
                let download = if model_on_this_computer(offer) {
                    String::new()
                } else {
                    format!(" ({} download)", hvtt_core::models::megabytes(offer.bytes))
                };
                // Forgotten again below if the message cannot be shown - and counted as made
                // (`better_offered`) only once it has been.
                better = true;
                *state.offered_model.lock() = Some((offer, measured_under));
                (Some(UpdateView {
                    stage: "offer",
                    title: format!("This computer can run {}", offer.name),
                    detail: format!("A short test with {name} took {}. {} should take roughly {} here \
                        (an estimate, checked once you switch) - {}{download}.",
                        secs(ms), offer.name, secs(estimate), offer.note),
                    action: Some(format!("Switch to {}", offer.name)),
                }), None)
            }
            None if announce => (Some(notice("This computer is quick enough",
                format!("A short test with {name} took {}. Nothing needs changing.", secs(ms)))), Some(5000)),
            None => (None, None),
        },
        SpeedAdvice::Lighter => {
            lighter(app);
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
                lighter(app);
                (announce.then(|| notice("This computer is slow", format!(
                    "A short test with {name} took {}. The words shown while you talk now refresh less often.", secs(ms)))), Some(8000))
            }
        },
    };
    let Some(view) = view else {
        drop(changes);
        push(app);
        return;
    };
    let offering = view.stage == "offer";
    let title = view.title.clone();
    let shown = speed_notice(app, view, hide_after);
    if !shown && offering {
        *state.offered_model.lock() = None;
    }
    // Made once unasked - so only an offer he could see counts as made: accepted by the box,
    // and on it (`on_screen`), not behind the shortcut prompt or a dictation (Codex's reviews,
    // 2026-10-05). One that went unseen is made again at the next check.
    if shown && better && on_screen(&state, &title) {
        update_settings(app, |s| s.better_offered = true);
    }
    drop(changes);
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
    let clipboard_at_start = settings.clipboard;

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
        clipboard_in_use: DictationClipboard::new(clipboard_at_start),
        not_copied: Mutex::new(false),
        #[cfg(any(target_os = "macos", windows))]
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
            copy_words,
            discard_words,
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
            // The paste gate's watch on the keyboard says when one of the shortcut's keys comes
            // up while he holds (the shortcut's own report waits for its main key alone).
            {
                let handle = app.handle().clone();
                platform_paste::on_hold_key_up(move |keys| {
                    post_to_main(&handle, move |app| {
                        if keys != 0 && HOLD_KEYS.load(std::sync::atomic::Ordering::SeqCst) == keys {
                            hold_released(app);
                        }
                    })
                });
            }
            // Windows has no such policy; the box carries never-activate styles instead.
            #[cfg(windows)]
            if let Some(w) = app.get_webview_window("composer") {
                win_surface::prepare(&w);
                // Alt+F4 while the box has the keyboard (he is fixing words) would destroy the
                // program's only window, and the program would leave with it. It is the box's
                // close button instead: words are kept, the box is put away, the program stays.
                let handle = app.handle().clone();
                w.on_window_event(move |event| {
                    if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                        api.prevent_close();
                        dismiss(handle.clone());
                    }
                });
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
            #[cfg(any(target_os = "macos", windows))]
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
                // request (tao's `applicationWillTerminate`), and Windows does too (tao answers
                // `WM_ENDSESSION` by ending the loop, tao 0.35.3 `event_loop.rs`) - keep what can
                // be kept in the few seconds the system allows (Windows: about five before it
                // shows "this app is preventing shutdown"), then leave.
                tauri::RunEvent::Exit => {
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(4);
                    let session_ending = !EXIT_ALLOWED.load(std::sync::atomic::Ordering::SeqCst);
                    if session_ending {
                        LEAVING.lock().leaving = true;
                        keep_words_so_far(app);
                        // Words that are only in the box get one more try at the clipboard: there
                        // is nobody left to show them to.
                        keep_unsaved_words(app);
                        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                        if !keep_everything_before_exit(app, remaining) {
                            eprintln!("[hvtt] leaving before everything was kept (the system is ending the session)");
                        }
                    }
                    #[cfg(target_os = "macos")]
                    leave_now(app);
                    // Windows: after a Quit the loop is over and the process ends by itself, as
                    // it always has. At the end of a session it does not (`leave_now`).
                    #[cfg(windows)]
                    {
                        if session_ending {
                            leave_now(app);
                        }
                    }
                }
                _ => {}
            }
            #[cfg(not(any(target_os = "macos", windows)))]
            let _ = (app, event);
        });
}

/// Leave (Windows), at the end of a session, now that what can be kept is kept. tao answers
/// `WM_ENDSESSION` by reporting its loop ended and returning to Windows, which is expected to end
/// the process. Measured 2026-10-03 on the published 0.1.5: the H goes and the process stays - so
/// if the shutdown is then called off, a copy with no H and no box is left holding the one-copy
/// marker, and the program cannot be started again. Tauri's own cleanup first (the H leaves the
/// notification area), as on macOS.
#[cfg(windows)]
fn leave_now(app: &AppHandle) -> ! {
    app.cleanup_before_exit();
    std::process::exit(0)
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

#[cfg(test)]
mod huck_untold_tests {
    use super::*;

    fn words(text: &str) -> Option<String> {
        Some(text.to_string())
    }

    fn never_shown() -> bool {
        panic!("nothing should have been put on the box")
    }

    /// A Quit as the program runs it: the ask (`Asked::now`), then the look once everything is
    /// kept (`huck_untold_at_quit`). `warning_up`: the warning is on the box as he chooses Quit.
    fn quit(
        untold: &Mutex<HuckUntold>,
        before: &Option<String>,
        now: &Option<String>,
        leaving: bool,
        warning_up: bool,
        show: impl FnOnce() -> bool,
    ) -> Option<bool> {
        let agreed = untold.lock().agreed(warning_up);
        huck_untold_at_quit(untold, before, now, leaving, agreed.as_deref(), show)
    }

    /// Codex's fourth review of the 0.1.8 candidate, as it happens: a Quit finishes a dictation
    /// onto Huck's Clipboard and then stays for another reason - his own clipboard is stuck out
    /// on loan. The next Quit finds those words already there. They were noted only by a Quit
    /// free to go, so nothing was ever said, and that second Quit left with them.
    #[test]
    fn words_a_blocked_quit_put_there_are_still_warned_of_by_the_quit_that_leaves() {
        let untold = Mutex::new(HuckUntold::default());
        let (before, during) = (words("an older dictation"), words("the dictation Quit finished"));

        // The first Quit: the words arrive, but something else keeps it from leaving.
        assert_eq!(quit(&untold, &before, &during, false, false, never_shown), None);

        // The second: free to go, and the words were there before it began. Still said - and
        // it stays for them.
        let mut shown = 0;
        let said = quit(&untold, &during, &during, true, false, || {
            shown += 1;
            true
        });
        assert_eq!((said, shown), (Some(true), 1), "he is told before the program leaves with them");

        // The third, chosen with that warning in front of him: it goes.
        assert_eq!(quit(&untold, &during, &during, true, true, never_shown), None);
        assert_eq!(*untold.lock(), HuckUntold::default(), "nothing is left noted");
    }

    /// Codex's fifth review: recognition outlasts the Quit's twenty-second wait. The Quit stays
    /// with nothing new on Huck's Clipboard to note; the words are written there afterwards and
    /// the box goes; the next Quit finds them "already there" - and left with the only copy.
    #[test]
    fn words_that_land_after_a_quit_gave_up_waiting_are_warned_of_by_the_next() {
        let untold = Mutex::new(HuckUntold::default());
        let older = words("an older dictation");

        // The Quit found dictation 7 unfinished, asked for it, waited, and gave up.
        let waited_for = Some(7);
        assert_eq!(quit(&untold, &older, &older, false, false, never_shown), None);

        // Recognition finishes later; its words are written to Huck's Clipboard.
        let late = "the dictation Quit asked for";
        note_words_a_quit_asked_for(&untold, waited_for, 7, true, late);

        // The next Quit finds them already there - and still says so, and stays.
        let now = words(late);
        assert_eq!(quit(&untold, &now, &now, true, false, || true), Some(true));
        assert_eq!(quit(&untold, &now, &now, true, true, never_shown), None, "chosen again: it goes");
    }

    #[test]
    fn only_the_dictation_a_quit_asked_for_is_noted_and_only_on_hucks_clipboard() {
        let untold = Mutex::new(HuckUntold::default());
        note_words_a_quit_asked_for(&untold, Some(7), 8, true, "a later dictation of his own");
        note_words_a_quit_asked_for(&untold, None, 8, true, "no Quit has asked for anything");
        note_words_a_quit_asked_for(&untold, Some(7), 7, false, "these went to the normal clipboard");
        assert_eq!(*untold.lock(), HuckUntold::default());
    }

    #[test]
    fn an_ordinary_quit_with_nothing_new_on_it_leaves_at_once() {
        let untold = Mutex::new(HuckUntold::default());
        let held = words("delivered an hour ago");
        assert_eq!(quit(&untold, &held, &held, true, false, never_shown), None);
        assert_eq!(quit(&untold, &None, &None, true, false, never_shown), None);
    }

    /// Codex's third review: a warning the box could not show is not a telling.
    #[test]
    fn a_warning_the_box_could_not_show_is_said_again_by_the_next_quit() {
        let untold = Mutex::new(HuckUntold::default());
        let (before, during) = (None, words("kept by Quit"));
        assert_eq!(quit(&untold, &before, &during, true, false, || false), Some(false), "not seen: this Quit stays");
        // Whatever the box shows now, he was never told: it is not his answer.
        assert_eq!(quit(&untold, &during, &during, true, true, || true), Some(true), "said again, and seen");
        assert_eq!(quit(&untold, &during, &during, true, true, never_shown), None);
    }

    /// Codex's fifth review, the other half: a warning counted the moment it was put up, so one
    /// that was hidden or replaced straight afterwards still let the next Quit go. Now choosing
    /// Quit is his answer only while that warning is on the box.
    #[test]
    fn a_warning_that_has_gone_from_the_box_is_said_again_not_acted_on() {
        let untold = Mutex::new(HuckUntold::default());
        let during = words("kept by Quit");
        assert_eq!(quit(&untold, &None, &during, true, false, || true), Some(true));
        // Something else took the box, or he closed it, before he chose Quit again.
        assert_eq!(quit(&untold, &during, &during, true, false, || true), Some(true), "said again");
        assert_eq!(quit(&untold, &during, &during, true, true, never_shown), None);
    }

    /// The noting is not held while the box is waited on - a second or two, with the main thread
    /// taking its turn - so words that land meanwhile are neither lost nor wait on it.
    #[test]
    fn words_that_land_while_the_warning_is_being_put_up_stay_noted() {
        let untold = Mutex::new(HuckUntold::default());
        let first = words("kept by Quit");
        let said = quit(&untold, &None, &first, true, false, || {
            untold.lock().note("landed while the box was being waited on");
            true
        });
        assert_eq!(said, Some(true));
        let after = untold.lock();
        assert_eq!(after.words.as_deref(), Some("landed while the box was being waited on"));
        assert!(!after.told, "and he has not been told about those");
    }

    /// The same fault in the update, found reading for it after Codex's fourth review: words
    /// arrive on Huck's Clipboard while an update waits, the update backs out for another reason,
    /// and the next Open Update finds them "already there". He was told about the earlier words
    /// only.
    #[test]
    fn an_update_says_so_again_for_words_that_arrived_after_he_was_told() {
        let mut told = ToldWords::default();
        let earlier = words("an older dictation");
        assert!(told.untold(&earlier), "never told: said first");
        told.told(earlier.clone());
        assert!(!told.untold(&earlier), "told about these: Open Update again goes ahead");

        let arrived = words("dictated while the update waited");
        assert!(told.untold(&arrived), "these are new to him, whenever they are noticed");
        assert!(told.untold(&arrived), "and stay so until the box has shown the warning");
        told.told(arrived.clone());
        assert!(!told.untold(&arrived));

        assert!(!ToldWords::default().untold(&None), "nothing on Huck's Clipboard: nothing to say");
        told.forget();
        assert!(told.untold(&arrived), "a new update on offer: told afresh");
    }

    #[test]
    fn words_he_has_since_replaced_with_a_dictation_of_his_own_are_not_warned_of() {
        let untold = Mutex::new(HuckUntold::default());
        let (before, during) = (None, words("kept by a Quit that stayed"));
        assert_eq!(quit(&untold, &before, &during, false, false, never_shown), None);
        // He carried on, and dictated again: that replaced them, as any dictation does.
        let later = words("a later dictation of his own");
        assert_eq!(quit(&untold, &later, &later, true, false, never_shown), None);
        assert_eq!(*untold.lock(), HuckUntold::default(), "nothing is left noted");
    }
}

#[cfg(test)]
mod pin_tests {
    use super::*;
    use hvtt_core::pipeline::{DeliveryError, Liveness};

    struct Field(&'static str);
    impl Destination for Field {
        fn label(&self) -> String {
            self.0.into()
        }
        fn is_alive(&self) -> Liveness {
            Liveness::Alive
        }
        fn deliver(&self, _: &str, _: bool) -> Result<(), DeliveryError> {
            Ok(())
        }
    }

    fn found(label: &'static str) -> Pinned {
        Pinned { destination: Some(Box::new(Field(label))), ..Pinned::default() }
    }

    /// Codex's eighth review, with the delay made on purpose: dictation A's worker has its
    /// field and stalls; A is closed (the generation moves on), B starts elsewhere and gets its
    /// own destination; A's worker then comes back. It used to write A's field over B's, and
    /// B's words went there.
    #[test]
    fn a_worker_that_outlives_its_dictation_cannot_replace_the_next_ones_destination() {
        let slot: std::sync::Arc<Mutex<Option<Box<dyn Destination>>>> = std::sync::Arc::new(Mutex::new(None));
        let (release, stalled) = std::sync::mpsc::channel::<()>();
        // A: generation 1. Its worker has found the field and is held up.
        let a = {
            let slot = slot.clone();
            std::thread::spawn(move || {
                let a_found = found("A's text box");
                stalled.recv().unwrap();
                // By now the box is on generation 3 (A put away, B started), and listening.
                place_pin(&slot, a_found, 1, 3, &SessionState::Recording).is_some()
            })
        };
        // B: generation 3, and its own worker is prompt.
        assert!(place_pin(&slot, found("B's text box"), 3, 3, &SessionState::Recording).is_some());
        release.send(()).unwrap();
        assert!(!a.join().unwrap(), "A's result is refused");
        assert_eq!(slot.lock().as_ref().map(|d| d.label()).as_deref(), Some("B's text box"));
    }

    #[test]
    fn a_destination_is_placed_only_while_its_dictation_still_has_words_to_send() {
        for session in [SessionState::Recording, SessionState::Paused, SessionState::Transcribing] {
            assert!(pin_still_wanted(4, 4, &session), "{session:?}");
            assert!(!pin_still_wanted(4, 5, &session), "{session:?}: another dictation by now");
        }
        let problem = SessionState::Error { message: "The microphone could not be opened.".into() };
        for session in [SessionState::Idle, SessionState::Ready, problem] {
            assert!(!pin_still_wanted(4, 4, &session), "{session:?}: its words have gone, or it never ran");
        }
        // Nothing found is still an answer for its own dictation - the note is shown.
        let slot = Mutex::new(None);
        let nothing = Pinned { note: Some(PinError::NotATextField), ..Pinned::default() };
        let placed = place_pin(&slot, nothing, 4, 4, &SessionState::Recording).expect("its own dictation");
        assert!(placed.label.is_none() && placed.note.is_some() && slot.lock().is_none());
    }
}

#[cfg(test)]
mod dictation_clipboard_tests {
    use super::*;

    /// Codex's seventh review: started into VS Code on the normal clipboard, switched to Huck's
    /// Clipboard from the H while talking. The paste had been built not to borrow; the words were
    /// then copied to Huck's Clipboard, which counted as "copied"; the paste sent his old normal
    /// clipboard. The paste and the copy now read one choice, fixed at the start.
    #[test]
    fn a_clipboard_changed_while_he_dictates_applies_from_the_next_dictation() {
        let mut settings = Settings::default();
        settings.clipboard = ClipboardChoice::System;
        let in_use = DictationClipboard::new(settings.clipboard);
        in_use.start(&settings);

        // Changed in the menu mid-dictation.
        settings.clipboard = ClipboardChoice::Huck;
        assert_eq!(in_use.choice(), ClipboardChoice::System, "the copy still goes to the normal clipboard");
        assert!(!in_use.paste_borrows(), "which is the one the paste, built at the start, reads");
        // So a copy that worked is on the clipboard the paste keys send, and one that failed is
        // refused by the paste (`words_are_there_to_paste`): the two can no longer disagree.
        assert!(crate::destination::words_are_there_to_paste(in_use.paste_borrows(), true));
        assert!(!crate::destination::words_are_there_to_paste(in_use.paste_borrows(), false));

        // The next dictation takes the new choice, for both.
        in_use.start(&settings);
        assert_eq!(in_use.choice(), ClipboardChoice::Huck);
        assert!(in_use.paste_borrows());
    }
}

#[cfg(test)]
mod unsaved_tests {
    use super::*;

    /// Codex's fifth review: with the copy failed and no draft, the box said "copy it before
    /// closing" and showed nothing to copy; the first Quit brought that box forward and the
    /// second left with the only copy. He counts as warned only where the words are shown.
    #[test]
    fn unsaved_words_count_as_seen_only_in_the_box_that_shows_them() {
        assert!(unsaved_on_screen(&SessionState::Ready, false, true), "a finished dictation, not copied");
        assert!(!unsaved_on_screen(&SessionState::Ready, true, true), "the shortcut prompt has the box");
        assert!(!unsaved_on_screen(&SessionState::Ready, false, false), "nothing is unsaved");
        let problem = SessionState::Error { message: "The microphone could not be opened.".into() };
        for session in [
            SessionState::Idle,
            SessionState::Recording,
            SessionState::Paused,
            SessionState::Transcribing,
            problem,
        ] {
            assert!(!unsaved_on_screen(&session, false, true), "{session:?}");
        }
    }

    /// Codex's sixth review: the words were on the box, but Quit's sentence was not - a shortcut
    /// error had the line - and the second Quit still left with them. The sentence itself must
    /// be what the box says.
    #[test]
    fn a_quit_is_answered_only_while_its_own_sentence_is_on_the_box() {
        let ready = SessionState::Ready;
        assert!(quit_confirmation_on_screen(&ready, false, true, UNSAVED_AT_QUIT_TEXT));
        let replaced = not_copied_message(true, true);
        assert!(!quit_confirmation_on_screen(&ready, false, true, &replaced), "a Copy failed since: asked again");
        assert!(!quit_confirmation_on_screen(&ready, false, true, ""), "never said");
        assert!(!quit_confirmation_on_screen(&ready, true, true, UNSAVED_AT_QUIT_TEXT), "under the shortcut prompt");
        assert!(!quit_confirmation_on_screen(&SessionState::Idle, false, true, UNSAVED_AT_QUIT_TEXT));
        assert!(!quit_confirmation_on_screen(&ready, false, false, UNSAVED_AT_QUIT_TEXT), "nothing is unsaved");
    }

    /// The second Quit goes only if the first one's warning was seen, and the words were still
    /// in front of him as he chose it (`quit`: `warned` is dropped unless `Asked::unsaved_seen`).
    #[test]
    fn a_second_quit_leaves_unsaved_words_only_if_they_were_in_front_of_him() {
        use exit::{quit_decision, QuitDecision};
        let warned = Some(4);
        let at_the_ask = |seen: bool| warned.filter(|_| seen);
        assert_eq!(quit_decision(true, true, at_the_ask(true), 4), QuitDecision::Exit);
        assert_eq!(quit_decision(true, true, at_the_ask(false), 4), QuitDecision::Warn, "said again");
    }

    #[test]
    fn the_box_says_where_words_that_could_not_be_copied_are() {
        for again in [false, true] {
            let only_here = not_copied_message(true, again);
            assert!(only_here.contains("only here") && only_here.contains("Copy"), "{only_here}");
            // In a draft, or in the text box they were sent to: never called the only copy.
            let elsewhere = not_copied_message(false, again);
            assert!(elsewhere.contains("below") && !elsewhere.contains("only here"), "{elsewhere}");
            assert!(!elsewhere.contains("Discard"), "{elsewhere}");
        }
        assert!(not_copied_message(true, true).contains("Discard"), "the way out, once Copy has failed again");
    }
}

#[cfg(test)]
mod on_screen_tests {
    use super::*;

    const NOT_BACK: &str = "Your clipboard isn't back yet";

    fn view(stage: &'static str, title: &str) -> UpdateView {
        UpdateView { stage, title: title.into(), detail: String::new(), action: None }
    }

    fn problem() -> SessionState {
        SessionState::Error { message: "The microphone could not be opened.".into() }
    }

    /// Codex's third review of the 0.1.8 candidate: with *Copied* or a microphone problem on the
    /// box, the warning was set, counted as given, and never shown - so the second Quit let his
    /// clipboard go unseen.
    #[test]
    fn a_warning_is_on_screen_in_every_resting_state() {
        let warning = view("warning", NOT_BACK);
        for session in [SessionState::Idle, SessionState::Ready, problem()] {
            assert!(message_on_screen(&session, false, Some(&warning), NOT_BACK), "{session:?}");
        }
    }

    #[test]
    fn an_ordinary_message_is_on_screen_only_when_the_box_is_idle() {
        // The update's own message, which keeps its Open Update button.
        let ordinary = view("ready", NOT_BACK);
        assert!(message_on_screen(&SessionState::Idle, false, Some(&ordinary), NOT_BACK));
        for session in [SessionState::Ready, problem()] {
            assert!(
                !message_on_screen(&session, false, Some(&ordinary), NOT_BACK),
                "{session:?}: set, but the box is showing something else - he has not been told"
            );
        }
    }

    #[test]
    fn nothing_is_on_screen_while_he_dictates_under_the_shortcut_prompt_or_once_it_is_gone() {
        let warning = view("warning", NOT_BACK);
        for session in [SessionState::Recording, SessionState::Paused, SessionState::Transcribing] {
            assert!(!message_on_screen(&session, false, Some(&warning), NOT_BACK), "{session:?}");
        }
        assert!(!message_on_screen(&SessionState::Idle, true, Some(&warning), NOT_BACK), "the shortcut prompt has the box");
        assert!(!message_on_screen(&SessionState::Idle, false, None, NOT_BACK), "closed, or put away");
        let other = view("current", "You're up to date");
        assert!(!message_on_screen(&SessionState::Idle, false, Some(&other), NOT_BACK), "replaced by something else");
    }
}

#[cfg(test)]
mod leaving_tests {
    use super::*;

    fn fresh() -> Leaving {
        Leaving { leaving: false, starting: 0 }
    }

    #[test]
    fn nothing_starts_once_the_program_is_leaving_and_only_one_takes_the_way_out() {
        let mut life = fresh();
        assert!(life.begin(), "a Quit takes the way out");
        assert!(!life.begin(), "an update finds it taken");
        assert!(!life.admit(), "no dictation is let in");
        assert_eq!(life.starting, 0, "and none is counted");
        life.stay();
        assert!(life.admit(), "not leaving after all: dictation works again");
    }

    /// Codex's review of the 0.1.8 candidate: a dictation let in just before a Quit, not yet
    /// showing as one, was invisible to it - the Quit saw nothing in progress and left.
    #[test]
    fn a_dictation_let_in_just_before_leaving_is_still_counted() {
        let mut life = fresh();
        assert!(life.admit(), "the shortcut is let in");
        assert!(life.begin(), "a Quit takes the way out a moment later");
        assert_eq!(life.starting, 1, "and can see a dictation is on its way");
        life.started();
        assert_eq!(life.starting, 0, "until it shows as one, and is found by looking");
        life.started();
        assert_eq!(life.starting, 0, "never below nothing");
    }

    /// Codex's second review: a start refused while leaving built its guard and dropped it with
    /// the lock still held - and dropping takes the lock. The shortcut froze the program. Asked
    /// from another thread here, so a hang fails the test instead of hanging it.
    #[test]
    fn a_dictation_refused_while_leaving_neither_hangs_nor_is_counted() {
        static LIFE: Mutex<Leaving> = Mutex::new(Leaving { leaving: false, starting: 0 });
        assert!(LIFE.lock().begin(), "the program is leaving");
        let (done, answer) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = done.send(Admitted::of(&LIFE).is_none());
        });
        assert_eq!(
            answer.recv_timeout(std::time::Duration::from_secs(5)),
            Ok(true),
            "refused - and it came back to say so"
        );
        assert_eq!(LIFE.lock().starting, 0, "a start that was refused is not counted, up or down");

        LIFE.lock().stay();
        let admitted = Admitted::of(&LIFE).expect("let in once the program is staying");
        assert_eq!(LIFE.lock().starting, 1, "counted while it does not yet show as a dictation");
        drop(admitted);
        assert_eq!(LIFE.lock().starting, 0, "and no longer once it does");
    }
}

#[cfg(test)]
mod speed_tests {
    use super::*;

    fn idle() -> SpeedWork {
        SpeedWork { running: false, asked: false }
    }

    #[test]
    fn one_check_at_a_time_and_the_one_running_answers_a_request() {
        let mut work = idle();
        assert!(work.request(false), "nothing running: the opening check starts");
        assert!(!work.request(true), "asked for meanwhile: no second check");
        assert!(work.answer(), "the one running says its result");
        assert!(!work.another(), "and nothing is owed after it");
        assert!(work.request(true), "free again");
    }

    /// Codex's twentieth review: a request made in the instant a check was finishing - after it
    /// had answered, before it had gone - started nothing and was never answered.
    #[test]
    fn a_request_made_while_a_check_is_finishing_gets_a_check_of_its_own() {
        let mut work = idle();
        assert!(work.request(true));
        assert!(work.answer());
        assert!(!work.request(true), "it still counts as running, so nothing new starts");
        assert!(work.another(), "so the same worker runs another for it");
        assert!(work.answer(), "which answers");
        assert!(!work.another());
        assert!(!work.running && !work.asked);
    }

    #[test]
    fn a_check_nobody_asked_for_owes_no_answer() {
        let mut work = idle();
        assert!(work.request(false));
        assert!(!work.answer());
        assert!(!work.another());
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;

    /// Codex's ninth review, with the stall made on purpose. Dictation A's live pass has heard
    /// a silent window - nothing to add, the cursor to move on 24 seconds - and stalls just
    /// before writing. A is closed and B starts: the epoch moves, then the words are reset. A's
    /// pass comes back. It must not write its cursor into B, whose own cursor is also nought.
    #[test]
    fn a_pass_that_outlives_its_dictation_cannot_move_the_next_ones_cursor() {
        use std::sync::atomic::{AtomicU64, Ordering};
        let live = std::sync::Arc::new(Mutex::new(Live::default()));
        let epoch_now = std::sync::Arc::new(AtomicU64::new(7));
        let (release, stalled) = std::sync::mpsc::channel::<()>();
        let a = {
            let (live, epoch_now) = (live.clone(), epoch_now.clone());
            std::thread::spawn(move || {
                // What A read when its pass began.
                let (epoch, from, upto) = (7, 0, 24 * 16_000);
                stalled.recv().unwrap();
                let mut live = live.lock();
                let written = still_this_pass(&live, &epoch_now, epoch, from);
                if written {
                    live.heard_upto = upto;
                }
                written
            })
        };
        // A is put away and B starts, as `dismiss_now` and `start_recording` do it: the epoch
        // first, the words second.
        epoch_now.fetch_add(1, Ordering::SeqCst);
        *live.lock() = Live::default();
        epoch_now.fetch_add(1, Ordering::SeqCst);
        *live.lock() = Live::default();
        release.send(()).unwrap();
        assert!(!a.join().unwrap(), "A's pass is refused");
        assert_eq!(live.lock().heard_upto, 0, "B is heard from its first word");
    }

    /// Codex's tenth review: with a window held, every live pass read - and previewed - the whole
    /// rest of the dictation, growing until the stop.
    #[test]
    fn a_held_window_is_previewed_from_its_newest_part_only() {
        let window = 29 * 16_000;
        let minute = 60 * 16_000;
        // Nothing held: everything not yet recognised for good, as before.
        assert_eq!(preview_from(0, minute, window, false), 0);
        assert_eq!(preview_from(5_000, minute, window, false), 5_000);
        // Held a minute ago: only the last window's worth.
        assert_eq!(preview_from(5_000, 5_000 + minute, window, true), 5_000 + minute - window);
        // Held, but less than a window has been said since: all of it, never before the hold.
        assert_eq!(preview_from(5_000, 5_000 + window / 2, window, true), 5_000);
        assert_eq!(preview_from(5_000, 0, window, true), 5_000);
    }
}
