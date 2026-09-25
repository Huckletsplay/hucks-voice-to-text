// Huck's Voice to Text — the normal-use interface.
//
// No bundler and no npm: this project lives on an exFAT volume that cannot store the symlinks a
// package manager needs, so the UI uses Tauri's injected global directly.
//
// This is a voice layer, not an application. The surface shows one state, says briefly where the
// words went, and leaves by itself. The words are never shown back afterwards: they are in the
// text box, or on the clipboard.

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
};

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
  const h = Math.ceil(document.querySelector(".surface").getBoundingClientRect().height);
  try { await getCurrentWindow().setSize(new LogicalSize(380, Math.max(64, h))); } catch {}
}

// ------------------------------------------------------------------ render

const LABEL = {
  idle: "Ready",
  recording: "Listening",
  transcribing: "Transcribing",
  error: "Problem",
};

function stateLabel(s) {
  if (s.shortcut_error) return "Shortcut not working";
  if (s.state !== "ready") return LABEL[s.state] ?? s.state;
  if (s.delivered) return "Sent";
  return (s.text || "").trim() ? "Copied" : "Nothing heard";
}

let last = null;
let panelClosed = false;
let rebinding = null;

const REBIND_FOR = { dictation: "Dictation", paste: "Huck's Clipboard" };

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
    ui.close.hidden = false;
    last = s.state;
    fit();
    return;
  }
  rebinding = null;

  // Check for Updates, chosen from the menu, shown only while nothing else is happening.
  ui.updateActions.hidden = true;
  if (s.update && s.state === "idle") {
    ui.body.dataset.state = s.update.stage === "failed" ? "error" : "idle";
    ui.state.textContent = s.update.title;
    ui.target.textContent = "";
    ui.note.textContent = s.update.detail;
    ui.note.hidden = !s.update.detail;
    ui.permission.hidden = true;
    ui.updateActions.hidden = s.update.stage !== "ready";
    ui.close.hidden = false;
    last = s.state;
    fit();
    return;
  }

  ui.body.dataset.state = s.shortcut_error ? "error" : s.state;
  ui.state.textContent = stateLabel(s);

  // Asked once per launch, decided by the app; the panel just shows it until he answers.
  if (!s.ask_permission) panelClosed = false;
  ui.permission.hidden = !s.ask_permission || panelClosed;

  // Where it is pointed - glanceable, never announced. An arrow only where the words are going
  // or went; the permission panel already says its own sentence.
  const aimed = s.state === "recording" || (s.state === "ready" && s.delivered);
  if (aimed && s.pinned) {
    ui.target.textContent = `→ ${s.pinned}`;
  } else if (s.state === "recording" || s.state === "ready") {
    ui.target.textContent = ui.permission.hidden ? (s.pin_note || "") : "";
  } else {
    ui.target.textContent = s.engine_ready ? "" : s.engine;
  }

  // A shortcut that did not register outranks everything else: without it the product has no
  // front door, and the failure must never be silent. "Sent" needs no second line.
  const note = s.shortcut_error || (s.delivered || s.state === "recording" ? "" : s.message) || "";
  ui.note.textContent = note;
  ui.note.hidden = !note;

  // ~100 ms of recognition is the only moment closing is held back.
  ui.close.hidden = s.state === "transcribing";

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
  if (last === "recording") { fit(); return; }
  invoke("dismiss");
}

ui.close.addEventListener("click", () => invoke("dismiss"));
ui.updateOpen.addEventListener("click", () => invoke("open_update"));
ui.updateLater.addEventListener("click", () => invoke("dismiss"));
ui.permOpen.addEventListener("click", () => { invoke("open_accessibility_settings"); done(); });
ui.permLater.addEventListener("click", done);

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
