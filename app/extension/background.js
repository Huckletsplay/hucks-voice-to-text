// Huck's Voice to Text — transport and exclusive pin routing.
//
// The DOM reference and committed ownership live in the page, not this worker: MV3 terminates
// an idle worker. A reconnecting page re-announces its committed pin to rebuild routing.
//
// Routing uses long-lived ports opened by each content script, so the extension needs no
// "tabs" permission. Only pin offers are broadcast. One page is selected and acknowledges its
// claim before the desktop gets an answer; every later request is unicast to that owner.
// Unknown or conflicting ownership is refusal, never a broadcast delivery.

const HOST = "com.huck.voicetotext";

let nativePort = null;
const pages = new Set();          // ports to content scripts
const waiting = new Map();        // request id -> { timer, req, owner, phase }
const owners = new Map();         // pin -> port; null means conflicting re-announcements
let latestPin = null;

function send(port, msg) {
  try { port.postMessage(msg); } catch {}
}

function dropOthers(pin, owner) {
  for (const port of pages) {
    if (port !== owner) send(port, { id: 0, cmd: "drop-pin", pin });
  }
}

function advancePin(pin) {
  if (pin === undefined || pin === null) return;
  if (latestPin === null || pin > latestPin) {
    latestPin = pin;
    for (const [old, owner] of owners) {
      if (old === undefined || old < pin) {
        if (owner) send(owner, { id: 0, cmd: "drop-pin", pin: old });
        owners.delete(old);
      }
    }
  }
}

function finish(id, msg) {
  const w = waiting.get(id);
  if (!w) return;
  clearTimeout(w.timer);
  waiting.delete(id);
  replyToDesktop(msg);
}

function refuse(id) {
  finish(id, { id, ok: false, alive: false, delivered: false,
               refused: "no-page-holds-the-pin" });
}

function connectNative() {
  if (nativePort) return nativePort;
  try {
    nativePort = chrome.runtime.connectNative(HOST);
  } catch {
    nativePort = null;
    return null;
  }
  nativePort.onMessage.addListener(handleFromDesktop);
  nativePort.onDisconnect.addListener(() => { nativePort = null; });
  return nativePort;
}

function replyToDesktop(msg) {
  const p = connectNative();
  if (p) { try { p.postMessage(msg); } catch {} }
}

function handleFromDesktop(req) {
  if (!req || req.id === undefined) return;
  // Register before sending: a fast page's answer must not outrun its waiting entry.
  const w = { req, owner: null, phase: req.cmd === "pin" ? "offered" : "routed" };
  w.timer = setTimeout(() => {
    if (w.req.cmd === "pin") {
      dropOthers(req.pin, null);
      owners.delete(req.pin);
    }
    refuse(req.id);
  }, 400);
  waiting.set(req.id, w);

  if (req.cmd === "pin") {
    if (req.pin !== undefined && (!Number.isSafeInteger(req.pin) || req.pin < 0 ||
        (latestPin !== null && req.pin <= latestPin))) { refuse(req.id); return; }
    advancePin(req.pin);
    for (const port of pages) send(port, req);
    return;
  }

  if (!["status", "deliver", "unpin"].includes(req.cmd)) { refuse(req.id); return; }
  const owner = owners.get(req.pin);
  if (!owner || !pages.has(owner)) { refuse(req.id); return; }
  w.owner = owner;
  send(owner, req);              // Never broadcast delivery, even during reconnection.
}

chrome.runtime.onConnect.addListener((port) => {
  if (port.name !== "hvtt-page") return;
  pages.add(port);
  port.onDisconnect.addListener(() => {
    pages.delete(port);
    for (const [pin, owner] of owners) {
      if (owner === port) owners.delete(pin);
    }
    for (const [id, w] of waiting) {
      if (w.owner === port) refuse(id);
    }
  });
  port.onMessage.addListener((msg) => {
    if (!msg) return;
    if (msg.type === "pin-state" && msg.pinProtocol === 1) {
      if (msg.latestPin !== null && msg.latestPin !== undefined &&
          (!Number.isSafeInteger(msg.latestPin) || msg.latestPin < 0)) return;
      advancePin(msg.latestPin);
      if (!msg.held) return;
      if (msg.pin !== undefined && (!Number.isSafeInteger(msg.pin) || msg.pin < 0 ||
          (latestPin !== null && msg.pin < latestPin))) {
        send(port, { id: 0, cmd: "drop-pin", pin: msg.pin });
        return;
      }
      if (owners.has(msg.pin) && owners.get(msg.pin) !== port) {
        owners.set(msg.pin, null); // Cannot choose between conflicting committed owners.
        dropOthers(msg.pin, null);
      } else {
        owners.set(msg.pin, port);
      }
      return;
    }
    if (msg.id === undefined) return;
    const w = waiting.get(msg.id);
    if (msg.phase === "offered") {
      if (!w || w.req.cmd !== "pin" || w.req.pin !== msg.pin || !msg.ok ||
          msg.pinProtocol !== 1 || w.phase !== "offered") {
        // A late/losing offer is not ownership. Do not drop an already selected owner's pin.
        if (owners.get(msg.pin) !== port && (!w || w.owner !== port)) {
          send(port, { id: 0, cmd: "drop-pin", pin: msg.pin });
        }
        return;
      }
      w.owner = port;
      w.phase = "claimed";
      dropOthers(msg.pin, port);
      send(port, { id: msg.id, cmd: "claim-pin", pin: msg.pin });
      return;
    }
    if (!w || w.owner !== port) return;
    if (w.req.cmd === "pin") {
      if (msg.phase !== "claimed" || msg.protocol !== "owned-pin-v1" ||
          msg.pin !== w.req.pin || !msg.ok ||
          (msg.pin !== undefined && msg.pin !== latestPin)) return;
      owners.set(msg.pin, port);
      dropOthers(msg.pin, port);
    } else if (w.req.cmd === "unpin") {
      owners.delete(w.req.pin);
    }
    finish(msg.id, msg);
  });
});

// Open the link eagerly so the first dictation does not pay a connection cost.
connectNative();
chrome.runtime.onStartup.addListener(connectNative);
chrome.runtime.onInstalled.addListener(connectNative);
