// Huck's Voice to Text — the normal-use interface.
//
// No bundler and no npm: this project lives on an exFAT volume that cannot store the symlinks a
// package manager needs, so the UI uses Tauri's injected global directly.
//
// This is a voice layer, not an application. While he talks the words fill in here (decided with
// him 2026-09-28); he can pause, fix them, and send with the shortcut or the Send button. After
// that the surface says briefly where the words went and leaves by itself - they are in the text
// box, or on the clipboard.

import { H_BODY, ARCS, WORDS } from "./mark.js";

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const { getCurrentWindow, LogicalSize } = window.__TAURI__.window;

const el = (id) => document.getElementById(id);
const ui = {
  body: document.body,
  hbody: el("hbody"), arcs: el("arcs"), words: el("words"),
  state: el("state-line"), target: el("target-line"),
  close: el("close"), note: el("note"),
  permission: el("permission"), permOpen: el("permission-open"), permLater: el("permission-later"),
  updateActions: el("update-actions"), updateOpen: el("update-open"), updateLater: el("update-later"),
  unsavedActions: el("unsaved-actions"), wordsCopy: el("words-copy"), wordsDiscard: el("words-discard"),
  learning: el("learning"), controls: el("controls"), pause: el("pause"), send: el("send"),
  live: el("live"), liveText: el("live-text"), liveTail: el("live-tail"), edit: el("edit"),
};

// ------------------------------------------------------------------ what he does in the box

// The paste into apps like VS Code is only made if nothing moved since the shortcut, which is
// judged by counting clicks and key presses. Clicks and keys in this box are his work here, not a
// sign he went elsewhere, so the box counts its own and reports them; the program allows exactly
// those. Modifier keys are not counted anywhere. Windows counts a held key once, macOS every
// repeat, so both counts are kept.
const MODIFIERS = ["Shift", "Control", "Alt", "Meta", "AltGraph", "CapsLock", "Fn", "OS"];
let input = { generation: 0, clicks: 0, keys: 0, keys_repeated: 0 };
const report = () => invoke("box_input", { input: { ...input } });
document.addEventListener("mousedown", () => { input.clicks += 1; report(); }, true);
document.addEventListener("keydown", (e) => {
  if (MODIFIERS.includes(e.key)) return;
  input.keys_repeated += 1;
  if (!e.repeat) input.keys += 1;
  report();
}, true);

// ------------------------------------------------------------------ the mark

function buildMark() {
  ui.hbody.setAttribute("d", H_BODY);
  for (const d of ARCS) ui.arcs.appendChild(path(d));
  for (const d of WORDS) ui.words.appendChild(path(d));
}
function path(d) {
  const p = document.createElementNS("http://www.w3.org/2000/svg", "path");
  p.setAttribute("d", d);
  return p;
}

// Both halves are knocked out of the H, so opacity 1 means "this shape reads as a gap". The mark
// is the only meter: the sound side reacts to the microphone, the text side writes itself in.

// Quiet room shows the inner arc only; speech makes all three radiate in.
function arcsFromLevel(state, level) {
  const rings = ui.arcs.children;
  if (state !== "recording") {
    for (const r of rings) r.style.opacity = "1";
    return;
  }
  const lit = Math.min(rings.length, 1 + Math.floor(Math.min(level * 3.2, 1) * rings.length));
  for (let i = 0; i < rings.length; i++) rings[i].style.opacity = i < lit ? "1" : "0.18";
}

// Each stretch of speech writes the next word block, in reading order; a full page starts over.
// Silence writes nothing, so the words only move while he is actually talking.
const TICKS_PER_WORD = 6;   // updates arrive every ~60 ms: about 2.5 words a second of speech
let spoken = 0;
function wordsFromSpeech(state, level, wasRecording) {
  const blocks = ui.words.children;
  if (state !== "recording") {
    for (const b of blocks) b.style.opacity = "";   // hand back to the stylesheet
    return;
  }
  if (!wasRecording) spoken = 0;
  if (Math.min(level * 3.2, 1) > 0.15) spoken += 1;
  const written = Math.floor(spoken / TICKS_PER_WORD) % (blocks.length + 1);
  for (let i = 0; i < blocks.length; i++) blocks[i].style.opacity = i < written ? "1" : "0.12";
}

// ------------------------------------------------------------------ sizing

// The window is the interface, so it is only ever as big as what it is showing.
async function fit() {
  if (!ui.edit.hidden) {
    ui.edit.style.height = "auto";
    ui.edit.style.height = `${Math.min(ui.edit.scrollHeight + 2, 168)}px`;
  }
  const h = Math.ceil(document.querySelector(".surface").getBoundingClientRect().height);
  try { await getCurrentWindow().setSize(new LogicalSize(420, Math.max(64, h))); } catch {}
}

// ------------------------------------------------------------------ render

const LABEL = {
  idle: "Ready",
  recording: "Listening",
  paused: "Paused",
  transcribing: "Transcribing",
  error: "Problem",
};

function stateLabel(s) {
  if (s.shortcut_error) return "Shortcut not working";
  if (s.state === "paused" && s.settling) return "Pausing…";
  if (s.state !== "ready") return LABEL[s.state] ?? s.state;
  if (s.delivered) return "Sent";
  if (s.not_copied) return "Not copied";
  return (s.text || "").trim() ? "Copied" : "Nothing heard";
}

// A finished dictation the clipboard would not take. Its words stay in the box, to read and to
// copy, with Copy to try again - and Discard when no draft holds them either. The program counts
// him as having seen such words only when this shows them (`unsaved_on_screen` in lib.rs, which
// must keep step with this).
const notCopied = (s) => s.state === "ready" && !!s.not_copied;

let last = null;
let panelClosed = false;
let rebinding = null;
// He has typed in the box since it was last filled; his text is never overwritten.
let dirty = false;
// He clicked into the words while listening: put the caret there once they are ready.
let wantCaret = false;

const REBIND_FOR = { dictation: "Dictation", paste: "Huck's Clipboard" };

// The words while he dictates: live and read-only while listening, his to fix while paused.
function renderWords(s) {
  if (s.generation !== input.generation) {
    input = { generation: s.generation, clicks: 0, keys: 0, keys_repeated: 0 };
    dirty = false;
    wantCaret = false;
  }
  const dictating = s.state === "recording" || s.state === "paused";
  ui.controls.hidden = !dictating;
  ui.learning.hidden = !dictating;
  ui.learning.setAttribute("aria-pressed", String(!!s.learning));
  // Only when it changes: this runs with every live update, and WebKit (macOS) drops a click
  // whose word was replaced between press and release - Pause worked only off its label.
  const pauseLabel = s.state === "paused" ? "Resume" : "Pause";
  if (ui.pause.textContent !== pauseLabel) ui.pause.textContent = pauseLabel;

  const paused = s.state === "paused";
  const held = notCopied(s);
  const showLive = s.state === "recording" || s.state === "transcribing" || held;
  ui.live.hidden = !showLive || (s.state === "transcribing" && !s.live_text);
  ui.edit.hidden = !paused;
  ui.unsavedActions.hidden = !held;
  ui.wordsDiscard.hidden = !s.words_unsaved;
  if (showLive) {
    // Follow the newest words, as a chat does - unless he has scrolled up to read, and then
    // only until he scrolls back down to the end (asked for 2026-09-30 and 2026-10-03).
    const box = ui.live;
    const following = box.scrollTop + box.clientHeight >= box.scrollHeight - 24;
    // Not copied: the finished words, whole and settled.
    const text = held ? s.text || "" : s.live_text || "";
    const tail = held ? "" : s.live_tail || "";
    ui.liveText.textContent = text;
    ui.liveTail.textContent = tail;
    if (following) box.scrollTop = box.scrollHeight;
    ui.live.dataset.empty = String(!text && !tail);
    ui.live.dataset.off = String(s.live_words === "off");
    dirty = false;
  }
  if (paused) {
    ui.edit.disabled = !!s.settling;
    ui.edit.placeholder = s.settling ? "Finishing your last words…" : "Nothing yet — Resume to keep talking.";
    if (!dirty && document.activeElement !== ui.edit && ui.edit.value !== (s.live_text || "")) {
      ui.edit.value = s.live_text || "";
      // Paused, the newest words are the ones in view too.
      ui.edit.scrollTop = ui.edit.scrollHeight;
    }
    if (wantCaret && !s.settling) {
      wantCaret = false;
      ui.edit.focus();
      ui.edit.setSelectionRange(ui.edit.value.length, ui.edit.value.length);
    }
  }
}

function render(s) {
  // Chosen from the menu: the box becomes the key prompt, as in Snip 'n' Clip.
  if (s.rebinding) {
    rebinding = s.rebinding;
    ui.body.dataset.state = s.rebind_error ? "error" : "idle";
    ui.state.textContent = "Press the new shortcut";
    ui.target.textContent = `for ${REBIND_FOR[s.rebinding]} — Esc to cancel`;
    ui.note.textContent = s.rebind_error || "";
    ui.note.hidden = !s.rebind_error;
    ui.permission.hidden = true;
    // The prompt has the box to itself: nothing a finished dictation or an update left in it
    // shows under it (the program counts none of that as seen meanwhile).
    ui.live.hidden = true;
    ui.edit.hidden = true;
    ui.controls.hidden = true;
    ui.learning.hidden = true;
    ui.unsavedActions.hidden = true;
    ui.updateActions.hidden = true;
    ui.close.hidden = false;
    last = s.state;
    fit();
    return;
  }
  rebinding = null;

  // Check for Updates, chosen from the menu, shown only while nothing else is happening.
  ui.updateActions.hidden = true;
  ui.unsavedActions.hidden = true;
  if (s.update && s.state === "idle") {
    // "measuring": the speed check is running. The mark works as it does while transcribing, so
    // the box is visibly busy for however long the test takes. "warning": something of his that
    // leaving would lose - in the problem colour.
    const problem = s.update.stage === "failed" || s.update.stage === "warning";
    ui.body.dataset.state = problem ? "error" : s.update.stage === "measuring" ? "transcribing" : "idle";
    ui.state.textContent = s.update.title;
    ui.target.textContent = "";
    ui.note.textContent = s.update.detail;
    ui.note.hidden = !s.update.detail;
    ui.permission.hidden = true;
    // Idle: no dictation's words or buttons belong in the box.
    ui.live.hidden = true;
    ui.edit.hidden = true;
    ui.controls.hidden = true;
    ui.learning.hidden = true;
    // "ready": an update to open. "offer": a suggestion with its own button (the speed check).
    const offer = s.update.stage === "offer";
    ui.updateActions.hidden = !(s.update.stage === "ready" || offer);
    ui.updateOpen.textContent = offer ? s.update.action || "Switch" : "Open Update";
    ui.updateLater.textContent = offer ? "Keep it as it is" : "Not now";
    updateStage = s.update.stage;
    ui.close.hidden = false;
    last = s.state;
    fit();
    return;
  }

  // Not copied is a problem, and said in the problem colour.
  const held = notCopied(s);
  ui.body.dataset.state = (s.shortcut_error || held) ? "error" : s.state;
  ui.state.textContent = stateLabel(s);

  // Asked once per launch, decided by the app; the panel just shows it until he answers.
  if (!s.ask_permission) panelClosed = false;
  ui.permission.hidden = !s.ask_permission || panelClosed;

  // Where it is pointed - glanceable, never announced. An arrow only where the words are going
  // or went; the permission panel already says its own sentence.
  const aimed = s.state === "recording" || (s.state === "ready" && s.delivered);
  if ((s.state === "recording" || s.state === "paused") && s.live_trouble) {
    // Words that could not be recognised yet: kept, and tried again.
    ui.target.textContent = s.live_trouble;
  } else if (aimed && s.pinned) {
    ui.target.textContent = `→ ${s.pinned}`;
  } else if (s.state === "recording" || s.state === "ready") {
    ui.target.textContent = ui.permission.hidden ? (s.pin_note || "") : "";
  } else {
    ui.target.textContent = s.engine_ready ? "" : s.engine;
  }

  // A shortcut that did not register outranks everything else: without it the product has no
  // front door, and the failure must never be silent. "Sent" needs no second line - unless the
  // words could not also be copied, which is always said.
  const quiet = (s.delivered && !held) || s.state === "recording" || s.state === "paused";
  // The last few seconds missing from words already sent: said even after "Sent".
  const missing = s.state === "ready" ? s.live_trouble || "" : "";
  // A warning that leaving would lose something of his is said here too, beside whatever a
  // finished dictation or a problem left on the box - a message of its own would only show once
  // the box is idle, and the program counts him as told only when it shows (`message_on_screen`
  // in lib.rs, which must keep step with this).
  const resting = s.state === "ready" || s.state === "error";
  const warning = s.update && s.update.stage === "warning" && resting
    ? `${s.update.title}. ${s.update.detail}` : "";
  // A box whose words were not copied always says its message, whatever else it has to say: it
  // may be Quit's "choose Quit again to leave without them", which the program counts as seen
  // only because this shows it (`quit_confirmation_on_screen` in lib.rs).
  // A shortcut error comes first and never alone hides the rest: beside it may be the one
  // sentence he needs (Quit's, or why Quit is waiting).
  const said = [s.shortcut_error, missing, quiet ? "" : s.message].filter(Boolean).join(" ");
  const note = [said, warning].filter(Boolean).join(" ");
  ui.note.textContent = note;
  ui.note.hidden = !note;

  // ~100 ms of recognition is the only moment closing is held back.
  ui.close.hidden = s.state === "transcribing";

  renderWords(s);
  arcsFromLevel(s.state, s.level ?? 0);
  wordsFromSpeech(s.state, s.level ?? 0, last === "recording");

  last = s.state;
  fit();
}

// ------------------------------------------------------------------ actions

function done() {
  // Mid-recording the panel only closes itself; afterwards the whole box can go.
  panelClosed = true;
  ui.permission.hidden = true;
  if (last === "recording" || last === "paused") { fit(); return; }
  invoke("dismiss");
}

ui.close.addEventListener("click", () => invoke("dismiss"));
let updateStage = null;
ui.updateOpen.addEventListener("click", () => invoke(updateStage === "offer" ? "accept_offer" : "open_update"));
ui.updateLater.addEventListener("click", () => invoke("dismiss"));
ui.permOpen.addEventListener("click", () => { invoke("open_accessibility_settings"); done(); });
ui.permLater.addEventListener("click", done);
ui.wordsCopy.addEventListener("click", () => invoke("copy_words"));
ui.wordsDiscard.addEventListener("click", () => invoke("discard_words"));

ui.pause.addEventListener("click", () => invoke("pause_resume", { input: { ...input } }));
ui.send.addEventListener("click", () => invoke("send", { input: { ...input } }));
ui.learning.addEventListener("click", () => {
  const on = ui.learning.getAttribute("aria-pressed") !== "true";
  ui.learning.setAttribute("aria-pressed", String(on));
  invoke("set_learning", { on });
});

// Clicking into the words pauses, so they stop changing under him, and gives the box the keyboard.
ui.live.addEventListener("click", async () => {
  wantCaret = true;
  // The keyboard first, so the pause does not hand it back to his app (macOS).
  await invoke("take_keyboard", { input: { ...input } });
  if (last === "recording") await invoke("pause_resume", { input: { ...input } });
});
ui.edit.addEventListener("mousedown", () => invoke("take_keyboard", { input: { ...input } }));
ui.edit.addEventListener("input", () => {
  dirty = true;
  invoke("edit_text", { text: ui.edit.value, input: { ...input } });
  fit();
});

// Build a shortcut from the keys actually pressed, so nobody ever types shortcut syntax.
function accelerator(e) {
  const parts = [];
  if (e.altKey) parts.push("Alt");
  if (e.ctrlKey) parts.push("Ctrl");
  if (e.metaKey) parts.push("Cmd");
  if (e.shiftKey) parts.push("Shift");
  let key = e.code;
  if (key.startsWith("Key")) key = key.slice(3);
  else if (key.startsWith("Digit")) key = key.slice(5);
  else if (["AltLeft", "AltRight", "ShiftLeft", "ShiftRight", "ControlLeft", "ControlRight",
            "MetaLeft", "MetaRight"].includes(key)) return null;   // modifiers alone, so far
  parts.push(key);
  return parts.join("+");
}

document.addEventListener("keydown", (e) => {
  if (e.key === "Escape") { invoke("dismiss"); return; }
  if (!rebinding) return;
  e.preventDefault();
  const accel = accelerator(e);
  if (accel) invoke("finish_rebind", { accelerator: accel });
});

// ------------------------------------------------------------------ boot

listen("hvtt:update", (e) => render(e.payload));
buildMark();
invoke("get_snapshot").then(render);
