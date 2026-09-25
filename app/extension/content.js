// Huck's Voice to Text — the pin lives here.
//
// THE PIN IS A DIRECT REFERENCE to the DOM element the user clicked into. Never a selector,
// never an id, never a description. That is the same model the macOS Accessibility path uses,
// and for the same measured reason: after a page reload the replacement field matched the
// original on every attribute we could read, so a look-alike is indistinguishable. The only
// trustworthy identity is the reference itself, which dies when the element does.
//
// Consequences, all deliberate:
//   - no re-finding, ever;
//   - liveness is checked before every write;
//   - every write is verified by reading it back, because Chromium returns success from writes
//     that do nothing;
//   - if this page goes away nothing answers, and the desktop app treats silence as refusal.

(() => {
  let pinned = null;            // { el, docURL }
  let lastFocusedEditable = null;
  let port = null;

  const isEditable = (el) =>
    !!el &&
    (el.tagName === "TEXTAREA" ||
      (el.tagName === "INPUT" &&
        /^(text|search|url|email|tel)$/i.test(el.type || "text")) ||
      el.isContentEditable);

  // A password box is never a destination. Checked at pin time and again before writing.
  const isSecure = (el) =>
    !!el && el.tagName === "INPUT" && /^password$/i.test(el.type || "");

  document.addEventListener(
    "focusin",
    (e) => { if (isEditable(e.target)) lastFocusedEditable = e.target; },
    true
  );

  const readValue = (el) => (el.isContentEditable ? el.innerText ?? "" : el.value ?? "");

  const describe = (el) =>
    el.getAttribute?.("aria-label") ||
    el.getAttribute?.("placeholder") ||
    el.getAttribute?.("data-placeholder") ||
    (el.isContentEditable ? "text box" : el.tagName.toLowerCase());

  function liveness() {
    if (!pinned) return { alive: false, why: "nothing-pinned" };
    const el = pinned.el;
    if (!el.isConnected) return { alive: false, why: "element-detached" };
    if (!document.contains(el)) return { alive: false, why: "not-in-document" };
    if (location.href !== pinned.docURL) return { alive: false, why: "page-navigated" };
    if (isSecure(el)) return { alive: false, why: "secure-field" };
    return { alive: true, why: "ok" };
  }

  // React, ProseMirror and friends own their inputs: assigning `.value` is reverted on the next
  // render. The native setter plus a real InputEvent is what those frameworks actually listen
  // for, and it is what makes ChatGPT's composer work.
  function write(el, text) {
    const before = readValue(el);

    if (el.isContentEditable) {
      el.focus({ preventScroll: true });   // focus INSIDE the page; does not raise the window
      try {
        const sel = window.getSelection();
        const r = document.createRange();
        r.selectNodeContents(el);
        r.collapse(false);
        sel.removeAllRanges();
        sel.addRange(r);
      } catch {}
      let ok = false;
      try { ok = document.execCommand("insertText", false, text); } catch { ok = false; }
      if (!ok) {
        const r = document.createRange();
        r.selectNodeContents(el);
        r.collapse(false);
        r.insertNode(document.createTextNode(text));
        el.dispatchEvent(new InputEvent("input", { bubbles: true, data: text,
                                                   inputType: "insertText" }));
      }
    } else {
      const proto =
        el.tagName === "TEXTAREA"
          ? window.HTMLTextAreaElement.prototype
          : window.HTMLInputElement.prototype;
      Object.getOwnPropertyDescriptor(proto, "value").set.call(el, before + text);
      el.dispatchEvent(new InputEvent("input", { bubbles: true, data: text,
                                                 inputType: "insertText" }));
      el.dispatchEvent(new Event("change", { bubbles: true }));
    }

    // VERIFY. A write that cannot be read back did not happen, whatever the API returned.
    // Normalised because contenteditable turns a leading space into a non-breaking one.
    const norm = (t) => t.replace(/ /g, " ").replace(/\s+/g, " ").trim();
    const after = readValue(el);
    return norm(after) !== norm(before) && norm(after).includes(norm(text));
  }

  function handle(req) {
    if (req.cmd === "pin") {
      const el =
        (isEditable(document.activeElement) && document.activeElement) ||
        (document.hasFocus() && lastFocusedEditable) ||
        null;
      if (!el || !el.isConnected) return null;          // stay silent; another page may answer
      if (isSecure(el)) return { id: req.id, ok: false, error: "secure-field" };
      pinned = { el, docURL: location.href };
      return {
        id: req.id,
        ok: true,
        target: { host: location.host, describe: describe(el) },
      };
    }

    if (!pinned) return null;                            // not ours to answer

    if (req.cmd === "status") return { id: req.id, ...liveness() };

    if (req.cmd === "unpin") {
      pinned = null;
      return { id: req.id, ok: true };
    }

    if (req.cmd === "deliver") {
      const live = liveness();
      if (!live.alive) {
        return { id: req.id, delivered: false, refused: live.why };
      }
      const t0 = performance.now();
      const landed = write(pinned.el, req.text || "");
      return {
        id: req.id,
        delivered: landed,
        refused: landed ? null : "write-did-not-land",
        ms: Math.round(performance.now() - t0),
      };
    }
    return null;
  }

  // The service worker is terminated when idle, which breaks the port. Reconnecting keeps this
  // page reachable without the pin ever leaving the page.
  function connect() {
    try {
      port = chrome.runtime.connect({ name: "hvtt-page" });
    } catch {
      setTimeout(connect, 2000);
      return;
    }
    port.onDisconnect.addListener(() => { port = null; setTimeout(connect, 1000); });
    port.onMessage.addListener((req) => {
      if (!req || req.id === undefined) return;
      let res = null;
      try { res = handle(req); } catch (e) {
        res = { id: req.id, delivered: false, alive: false,
                refused: "extension-error:" + String(e).slice(0, 60) };
      }
      if (res && port) { try { port.postMessage(res); } catch {} }
    });
  }
  connect();
})();
