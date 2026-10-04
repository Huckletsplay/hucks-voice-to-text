// Isolated protocol checks: node --test app/extension/pin-tests.cjs
// No browser, desktop app, microphone, real DOM, clipboard or native host is used.
const assert = require("node:assert/strict");
const { test } = require("node:test");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");
const { execFileSync } = require("node:child_process");
const content = fs.readFileSync(path.join(__dirname, "content.js"), "utf8");
const background = fs.readFileSync(path.join(__dirname, "background.js"), "utf8");

function event() {
  const listeners = [];
  return { addListener(fn) { listeners.push(fn); }, emit(msg) { for (const fn of listeners) fn(msg); } };
}

function harness() {
  let now = 0, worker, native, nextTimer = 0;
  const queue = [], timers = new Map(), links = [], replies = [], pages = [];
  const setTimeout = (fn, delay) => {
    const id = ++nextTimer;
    timers.set(id, { fn, at: now + delay });
    return id;
  };
  const clearTimeout = (id) => timers.delete(id);
  function flush() {
    let budget = 1000;
    while (queue.length) {
      assert.ok(budget-- > 0, "message loop terminated");
      queue.shift()();
    }
  }
  function advance(ms) {
    now += ms;
    for (const [id, timer] of [...timers]) {
      if (timer.at <= now) { timers.delete(id); timer.fn(); }
    }
    flush();
  }
  function startWorker() {
    const onConnect = event();
    native = { onMessage: event(), onDisconnect: event(), postMessage: msg => replies.push(msg) };
    worker = { onConnect };
    vm.runInNewContext(background, {
      chrome: { runtime: { onConnect, onStartup: event(), onInstalled: event(),
                           connectNative: () => native } }, setTimeout, clearTimeout,
    }, { filename: "background.js" });
  }
  function connect(page) {
    const toPage = event(), toWorker = event(), pageDisconnect = event(), workerDisconnect = event();
    const link = { alive: true };
    link.pagePort = { onMessage: toPage, onDisconnect: pageDisconnect,
      postMessage(msg) {
        page.answers.push(msg);
        queue.push(() => { if (link.alive) toWorker.emit(msg); });
      } };
    link.workerPort = { name: "hvtt-page", onMessage: toWorker, onDisconnect: workerDisconnect,
      postMessage(msg) {
        page.requests.push(msg);
        queue.push(() => { if (link.alive) toPage.emit(msg); });
      } };
    link.disconnect = () => {
      if (!link.alive) return;
      link.alive = false;
      workerDisconnect.emit();
      pageDisconnect.emit();
    };
    links.push(link);
    page.link = link;
    worker.onConnect.emit(link.workerPort);
    return link.pagePort;
  }
  function addPage(focused, source = content, noDomApi = false) {
    const page = { focused, requests: [], answers: [], fields: [], listeners: {} };
    class TextArea {
      constructor() { this.tagName = "TEXTAREA"; this.isConnected = true; this._value = ""; }
      get value() { return this._value; }
      set value(v) { this._value = v; }
      getAttribute() { return null; }
      dispatchEvent() {} // synthetic mock only
    }
    const field = new TextArea();
    page.fields.push(field);
    const body = { tagName: "BODY" };
    page.body = body;
    page.document = { activeElement: field, hasFocus: () => page.focused, body,
      documentElement: { tagName: "HTML" },
      contains: el => page.fields.includes(el),
      addEventListener(type, fn) { (page.listeners[type] ||= []).push(fn); } };
    // He clicks into a field: the page remembers it (`lastFocusedEditable`).
    page.focusIn = (el) => { for (const fn of page.listeners.focusin || []) fn({ target: el }); };
    page.anotherField = () => { const el = new TextArea(); page.fields.push(el); return el; };
    vm.runInNewContext(source, {
      document: page.document, location: { href: "https://example.test/", host: "example.test" },
      window: { HTMLTextAreaElement: TextArea, HTMLInputElement: TextArea },
      InputEvent: class {}, Event: class {}, performance: { now: () => now }, setTimeout,
      // `closedRoot` stands for a shadow tree `.shadowRoot` does not show.
      chrome: { runtime: { connect: () => connect(page) },
                dom: noDomApi ? undefined
                  : { openOrClosedShadowRoot: el => el.shadowRoot ?? el.closedRoot ?? null } },
    }, { filename: "content.js" });
    pages.push(page);
    flush();
    return page;
  }
  function request(req) { native.onMessage.emit(req); flush(); return replies.at(-1); }
  function direct(page, req) {
    const before = page.answers.length;
    page.link.pagePort.onMessage.emit(req);
    flush();
    return page.answers.slice(before).filter(msg => msg.id === req.id);
  }
  function restart() {
    // Termination erases the worker's maps/timers and ports, but not the pages' DOM references.
    timers.clear();
    for (const link of links) link.disconnect();
    startWorker();
  }
  startWorker();
  return { addPage, request, direct, restart, advance, flush, replies, pages };
}

test("background tab cannot offer its retained active element; only focused page receives words", () => {
  const h = harness(), backgroundPage = h.addPage(false), focused = h.addPage(true);
  const answer = h.request({ id: 1, cmd: "pin", pin: 200 });
  assert.equal(answer.protocol, "owned-pin-v1");
  assert.equal(answer.pin, 200);
  assert.equal(backgroundPage.answers.filter(m => m.phase === "offered").length, 0);
  assert.equal(h.direct(backgroundPage, { id: 8, cmd: "status", pin: 200 }).length, 0);
  assert.equal(h.request({ id: 2, cmd: "deliver", pin: 200, text: "words" }).delivered, true);
  assert.equal(focused.fields[0].value, "words");
  assert.equal(backgroundPage.fields[0].value, "");
  assert.equal(backgroundPage.requests.filter(m => m.cmd === "deliver").length, 0);
});

test("two focused documents/frames offer the same pin but only one can commit it", () => {
  const h = harness(), first = h.addPage(true), second = h.addPage(true);
  h.request({ id: 1, cmd: "pin", pin: 200 });
  assert.equal(first.answers.filter(m => m.phase === "offered").length, 1);
  assert.equal(second.answers.filter(m => m.phase === "offered").length, 1);
  assert.ok(second.requests.some(m => m.cmd === "drop-pin" && m.pin === 200));
  assert.equal(h.direct(second, { id: 8, cmd: "status", pin: 200 }).length, 0);
  h.request({ id: 2, cmd: "deliver", pin: 200, text: "words" });
  assert.deepEqual([first.fields[0].value, second.fields[0].value], ["words", ""]);
  assert.equal(second.requests.filter(m => m.cmd === "deliver").length, 0);
});

test("with the caret in an iframe editor, the parent's remembered field is never chosen", () => {
  // The parent is added first, so its answer would reach the worker first.
  const h = harness(), parent = h.addPage(true), frame = h.addPage(true);
  parent.focusIn(parent.fields[0]);                       // he was in the parent's field earlier
  parent.document.activeElement = { tagName: "IFRAME" };  // and is now inside the frame
  const answer = h.request({ id: 1, cmd: "pin", pin: 200 });
  assert.equal(answer.ok, true);
  assert.equal(parent.answers.filter(m => m.phase === "offered").length, 0, "the parent makes no offer");
  assert.equal(h.request({ id: 2, cmd: "deliver", pin: 200, text: "words" }).delivered, true);
  assert.deepEqual([parent.fields[0].value, frame.fields[0].value], ["", "words"]);
});

test("with the caret in a shadow-tree editor, the remembered field is never chosen", () => {
  // The document sees only the shadow host: not editable, not a frame.
  const h = harness(), page = h.addPage(true);
  page.focusIn(page.fields[0]);
  page.document.activeElement = { tagName: "FANCY-EDITOR" };
  h.request({ id: 1, cmd: "pin", pin: 200 });
  h.advance(500);
  assert.equal(page.answers.filter(m => m.phase === "offered").length, 0, "no offer");
  assert.equal(h.replies.at(-1).ok, false, "refused: the program pastes instead");
  assert.equal(page.fields[0].value, "");
});

test("with the caret in an iframe inside a shadow tree, only the frame is chosen", () => {
  // The parent sees the shadow host, not the frame, and is added first.
  const h = harness(), parent = h.addPage(true), frame = h.addPage(true);
  parent.focusIn(parent.fields[0]);
  parent.document.activeElement = { tagName: "FANCY-EDITOR" };
  assert.equal(h.request({ id: 1, cmd: "pin", pin: 200 }).ok, true);
  assert.equal(parent.answers.filter(m => m.phase === "offered").length, 0);
  h.request({ id: 2, cmd: "deliver", pin: 200, text: "words" });
  assert.deepEqual([parent.fields[0].value, frame.fields[0].value], ["", "words"]);
});

for (const [kind, root] of [["open", { shadowRoot: {} }], ["closed", { shadowRoot: null, closedRoot: {} }]]) {
  test(`an editable host of a ${kind} shadow tree is never offered, focused or remembered`, () => {
    const h = harness(), page = h.addPage(true);
    const host = { tagName: "FANCY-EDITOR", isContentEditable: true, isConnected: true, ...root };
    page.document.activeElement = host;                   // the caret is inside its tree
    h.request({ id: 1, cmd: "pin", pin: 200 });
    h.advance(500);
    assert.equal(page.answers.filter(m => m.phase === "offered").length, 0, "no offer while focused");
    assert.equal(h.replies.at(-1).ok, false, "refused: the program pastes instead");
    page.focusIn(host);                                   // focusin is retargeted to the host
    page.document.activeElement = page.body;
    h.request({ id: 2, cmd: "pin", pin: 201 });
    h.advance(500);
    assert.equal(page.answers.filter(m => m.phase === "offered").length, 0, "no offer when remembered");
    assert.equal(h.replies.at(-1).ok, false);
  });
}

test("an iframe that inherits editability from the region around it is never offered", () => {
  const h = harness(), parent = h.addPage(true), frame = h.addPage(true);
  parent.document.activeElement = { tagName: "IFRAME", isContentEditable: true, isConnected: true };
  assert.equal(h.request({ id: 1, cmd: "pin", pin: 200 }).ok, true);
  assert.equal(parent.answers.filter(m => m.phase === "offered").length, 0, "the parent makes no offer");
  assert.equal(h.request({ id: 2, cmd: "deliver", pin: 200, text: "words" }).delivered, true);
  assert.deepEqual([parent.fields[0].value, frame.fields[0].value], ["", "words"]);
});

test("an ordinary rich-text box is still offered; one Chrome cannot vouch for is not", () => {
  const box = { tagName: "DIV", isContentEditable: true, isConnected: true, shadowRoot: null };
  const h = harness(), page = h.addPage(true);
  page.document.activeElement = box;
  assert.equal(h.request({ id: 1, cmd: "pin", pin: 200 }).ok, true);
  // No chrome.dom: a closed shadow tree cannot be ruled out.
  const blind = harness(), unsure = blind.addPage(true, content, true);
  unsure.document.activeElement = box;
  blind.request({ id: 1, cmd: "pin", pin: 200 });
  blind.advance(500);
  assert.equal(unsure.answers.filter(m => m.phase === "offered").length, 0);
  assert.equal(blind.replies.at(-1).ok, false);
});

test("a textarea inside an editable region is written as a textarea", () => {
  const h = harness(), page = h.addPage(true);
  page.fields[0].isContentEditable = true;                // inherited from the region around it
  assert.equal(h.request({ id: 1, cmd: "pin", pin: 200 }).ok, true);
  assert.equal(h.request({ id: 2, cmd: "deliver", pin: 200, text: "words" }).delivered, true);
  assert.equal(page.fields[0].value, "words");
});

test("focus on any other control of the page does not bring back the remembered field", () => {
  const h = harness(), page = h.addPage(true);
  page.focusIn(page.fields[0]);
  page.document.activeElement = { tagName: "BUTTON" };
  h.request({ id: 1, cmd: "pin", pin: 200 });
  h.advance(500);
  assert.equal(page.answers.filter(m => m.phase === "offered").length, 0);
});

test("a page whose own remembered field is all it has still offers it", () => {
  const h = harness(), page = h.addPage(true);
  page.focusIn(page.fields[0]);
  page.document.activeElement = page.body;                // focus fell back to the page itself
  assert.equal(h.request({ id: 1, cmd: "pin", pin: 200 }).ok, true);
  h.request({ id: 2, cmd: "deliver", pin: 200, text: "words" });
  assert.equal(page.fields[0].value, "words");
});

test("late named pin, late unpin and mismatched status meet silence in the page", () => {
  const h = harness(), page = h.addPage(true);
  h.request({ id: 1, cmd: "pin", pin: 200 });
  page.document.activeElement = page.anotherField();
  assert.equal(h.direct(page, { id: 2, cmd: "pin", pin: 100 }).length, 0);
  assert.equal(h.direct(page, { id: 3, cmd: "unpin", pin: 100 }).length, 0);
  assert.equal(h.direct(page, { id: 4, cmd: "status", pin: 100 }).length, 0);
  h.request({ id: 5, cmd: "deliver", pin: 200, text: "B words" });
  assert.deepEqual(page.fields.map(f => f.value), ["B words", ""]);
  h.request({ id: 6, cmd: "unpin", pin: 200 });
  assert.equal(h.direct(page, { id: 7, cmd: "pin", pin: 100 }).length, 0);
});

test("an older pin on another focused page cannot replace the newer owner", () => {
  const h = harness(), first = h.addPage(true), second = h.addPage(false);
  h.request({ id: 1, cmd: "pin", pin: 200 });
  first.focused = false; second.focused = true;
  assert.equal(h.request({ id: 2, cmd: "pin", pin: 100 }).ok, false);
  assert.equal(h.request({ id: 3, cmd: "unpin", pin: 100 }).ok, false);
  h.request({ id: 4, cmd: "deliver", pin: 200, text: "B words" });
  assert.deepEqual([first.fields[0].value, second.fields[0].value], ["B words", ""]);
});

test("worker restart refuses before re-announcement, then restores exclusive routing", () => {
  const h = harness(), first = h.addPage(true), second = h.addPage(true);
  h.request({ id: 1, cmd: "pin", pin: 200 });
  h.restart();
  assert.equal(h.request({ id: 2, cmd: "deliver", pin: 200, text: "lost" }).delivered, false);
  h.advance(1000); // pages reconnect and announce only committed pins
  assert.equal(h.request({ id: 3, cmd: "status", pin: 200 }).alive, true);
  assert.equal(h.request({ id: 4, cmd: "deliver", pin: 200, text: "kept" }).delivered, true);
  assert.deepEqual([first.fields[0].value, second.fields[0].value], ["kept", ""]);
  assert.equal(second.requests.filter(m => m.cmd === "deliver").length, 0);
});

test("conflicting ownership announcements refuse rather than broadcasting", () => {
  const h = harness(), first = h.addPage(true), second = h.addPage(false);
  h.request({ id: 1, cmd: "pin", pin: 200 });
  second.link.pagePort.postMessage({ type: "pin-state", pinProtocol: 1, held: true, pin: 200, latestPin: 200 });
  h.flush();
  assert.equal(h.request({ id: 2, cmd: "deliver", pin: 200, text: "unsafe" }).delivered, false);
  assert.deepEqual([first.fields[0].value, second.fields[0].value], ["", ""]);
  assert.equal(h.pages.flatMap(p => p.requests).filter(m => m.cmd === "deliver").length, 0);
});

test("unknown or disconnected owner refuses without delivery to other pages", () => {
  const h = harness(), first = h.addPage(true), second = h.addPage(false);
  h.request({ id: 1, cmd: "pin", pin: 200 });
  first.link.disconnect();
  assert.equal(h.request({ id: 2, cmd: "deliver", pin: 200, text: "unsafe" }).delivered, false);
  assert.equal(second.requests.filter(m => m.cmd === "deliver").length, 0);
});

test("new extension still supports an old desktop's unnamed pin, with exclusive routing", () => {
  const h = harness(), first = h.addPage(true), second = h.addPage(true);
  assert.equal(h.request({ id: 1, cmd: "pin" }).ok, true);
  assert.equal(h.request({ id: 2, cmd: "deliver", text: "words" }).delivered, true);
  assert.deepEqual([first.fields[0].value, second.fields[0].value], ["words", ""]);
});

test("the actual 0.1.0 script cannot negotiate ownership with the new worker", () => {
  // Kept as a source fixture so this check also runs in a source archive without private history.
  const legacy = fs.readFileSync(path.join(__dirname, "tests", "content-0.1.0.js"), "utf8");
  const h = harness(), page = h.addPage(true, legacy);
  h.request({ id: 1, cmd: "pin", pin: 200 });
  h.advance(400);
  assert.equal(h.replies.at(-1).ok, false);
  assert.equal(h.request({ id: 2, cmd: "deliver", pin: 200, text: "unsafe" }).delivered, false);
  assert.equal(page.fields[0].value, "");
  assert.equal(page.requests.filter(m => m.cmd === "deliver").length, 0);
});

test("actual 0.1.0 pin answer is refused by the Rust desktop validator",
     { skip: !process.env.HVTT_DESKTOP_TEST_EXE && "run dev.ps1 test, then set HVTT_DESKTOP_TEST_EXE to its desktop unit-test binary" }, () => {
  const legacy = fs.readFileSync(path.join(__dirname, "tests", "content-0.1.0.js"), "utf8");
  const h = harness(), page = h.addPage(true, legacy);
  const [answer] = h.direct(page, { id: 1, cmd: "pin", pin: 200 });
  assert.equal(answer.ok, true);
  // Feed this real legacy response to the same Rust function used by ChromiumDestination::pin.
  // HVTT_DESKTOP_TEST_EXE is the existing test binary printed by dev.ps1 test; no app is launched.
  assert.ok(process.env.HVTT_DESKTOP_TEST_EXE, "set HVTT_DESKTOP_TEST_EXE to the desktop unit-test executable");
  const result = execFileSync(process.env.HVTT_DESKTOP_TEST_EXE,
    ["destination::chromium::pin_answer_tests::refuses_an_old_extension_without_the_capability", "--exact", "--nocapture"],
    { encoding: "utf8", env: { ...process.env, HVTT_MOCK_PIN_ANSWER: JSON.stringify(answer) } });
  assert.match(result, /1 passed/);
  assert.equal(page.fields[0].value, ""); // desktop refusal: no destination, delivery or release
});
