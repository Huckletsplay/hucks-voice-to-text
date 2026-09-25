// Huck's Voice to Text — transport only.
//
// This worker owns the connection to the desktop app and nothing else. It deliberately does not
// hold the pin: MV3 terminates an idle service worker, so a pin kept here would evaporate
// mid-dictation. The pin lives in the content script, whose lifetime is its page's — which is
// exactly the lifetime a pinned text box should have.
//
// Routing uses long-lived ports opened by each content script, so the extension needs no
// "tabs" permission: a request is offered to every page, and only the one actually holding the
// pin answers.

const HOST = "com.huck.voicetotext";

let nativePort = null;
const pages = new Set();          // ports to content scripts
const waiting = new Map();        // request id -> { timer }

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

  // Offer it to every page. Exactly one — the page holding the pin, or for "pin" the page that
  // currently has an editable focused — will answer.
  for (const port of pages) {
    try { port.postMessage(req); } catch {}
  }

  // If nobody answers promptly the destination is gone. Silence must become an explicit
  // refusal, never an assumption that it worked.
  const timer = setTimeout(() => {
    if (waiting.has(req.id)) {
      waiting.delete(req.id);
      replyToDesktop({ id: req.id, ok: false, alive: false, delivered: false,
                       refused: "no-page-holds-the-pin" });
    }
  }, 400);
  waiting.set(req.id, { timer });
}

chrome.runtime.onConnect.addListener((port) => {
  if (port.name !== "hvtt-page") return;
  pages.add(port);
  port.onDisconnect.addListener(() => pages.delete(port));
  port.onMessage.addListener((msg) => {
    if (!msg || msg.id === undefined) return;
    const w = waiting.get(msg.id);
    if (!w) return;                 // already answered or timed out
    clearTimeout(w.timer);
    waiting.delete(msg.id);
    replyToDesktop(msg);
  });
});

// Open the link eagerly so the first dictation does not pay a connection cost.
connectNative();
chrome.runtime.onStartup.addListener(connectNative);
chrome.runtime.onInstalled.addListener(connectNative);
