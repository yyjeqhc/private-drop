import test from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import vm from "node:vm";

test("operation navigation shows one destination and moves focus without replacing its form", async () => {
  const source = await readFile(new URL("../dist/runtime.js", import.meta.url), "utf8");
  const start = source.indexOf("function revealOperationsSection(");
  const end = source.indexOf("\n}", start) + 2;
  const names = ["overview", "runners", "agents"].map((name) => "runtime-operations-" + name);
  let focused;
  const panels = new Map(names.map((id) => [id, {
    hidden: id !== names[0],
    draft: "unsent message",
    focus() { focused = id; },
  }]));
  const buttons = names.map((id) => ({
    dataset: { operationsTarget: id },
    attrs: {},
    classList: { toggle() {} },
    setAttribute(key, value) { this.attrs[key] = value; },
    removeAttribute(key) { delete this.attrs[key]; },
  }));
  const scroll = { scrollTop: 800 };
  const context = vm.createContext({
    document: { querySelectorAll: () => buttons, querySelector: () => scroll },
    applyWorkspaceView() {},
    show(id, visible) { panels.get(id).hidden = !visible; },
    el: (id) => panels.get(id),
  });
  vm.runInContext(source.slice(start, end), context);
  for (const id of [names[2], names[1], names[0], names[2]]) {
    context.revealOperationsSection(id);
    assert.deepEqual([...panels].filter(([, panel]) => !panel.hidden).map(([key]) => key), [id]);
    assert.deepEqual(buttons.filter((button) => button.attrs["aria-current"] === "page").map((button) => button.dataset.operationsTarget), [id]);
    assert.equal(focused, id);
    assert.equal(scroll.scrollTop, 0);
    assert.equal(panels.get(id).draft, "unsent message");
  }
  context.revealOperationsSection("invalid");
  assert.equal(focused, names[2]);
  assert.equal(panels.get(names[2]).hidden, false);
});

test("context overview exposes validation while activity and identity have direct entries", async () => {
  const html = await readFile(new URL("../src/runtime.html", import.meta.url), "utf8");
  const overview = html.slice(html.indexOf('<section id="runtime-context-overview"'), html.indexOf('<section id="runtime-context-activity"'));
  assert.match(overview, /id="runtime-overview-validation"/);
  assert.match(overview, /id="runtime-overview-attention"/);
  assert.doesNotMatch(overview, /<details/);
  for (const name of ["overview", "activity", "details"]) {
    assert.match(html, new RegExp('data-context-target="runtime-context-' + name + '" aria-controls="runtime-context-' + name + '"'));
  }
});

test("closing mobile navigation fences its delayed focus callback", async () => {
  const source = await readFile(new URL("../dist/runtime.js", import.meta.url), "utf8");
  const start = source.indexOf("function setMobileNavigationOpen(");
  const end = source.indexOf("\n}", start) + 2;
  const classes = new Set();
  const callbacks = [];
  let focused = false;
  const shell = { classList: {
    toggle(name, enabled) { if (enabled) classes.add(name); else classes.delete(name); },
    contains(name) { return classes.has(name); },
  } };
  const context = vm.createContext({
    el(id) {
      if (id === "runtime-console") return shell;
      if (id === "runtime-mobile-nav-close") return { focus() { focused = true; } };
      return { setAttribute() {}, removeAttribute() {} };
    },
    mobileNavigationViewport: () => true,
    closeAppearanceMenus() {},
    closeTopbarMore() {},
    closeRuntimeInspector() {},
    window: { setTimeout(callback) { callbacks.push(callback); } },
  });
  vm.runInContext(source.slice(start, end), context);
  context.setMobileNavigationOpen(true);
  context.setMobileNavigationOpen(false);
  callbacks.shift()();
  assert.equal(focused, false);
  context.setMobileNavigationOpen(true);
  callbacks.shift()();
  assert.equal(focused, true);
});

test("project search opens the correct view and focuses the search on desktop and mobile", async () => {
  const source = await readFile(new URL("../dist/runtime.js", import.meta.url), "utf8");
  const start = source.indexOf("function focusProjectNavigation(");
  const end = source.indexOf("\n}", start) + 2;
  let mobile = false;
  const calls = [];
  const context = vm.createContext({
    applyWorkspaceView(view) { calls.push(view); },
    mobileNavigationViewport: () => mobile,
    setMobileNavigationOpen(...args) { calls.push(args); },
    el: (id) => ({ focus() { calls.push(id); } }),
  });
  vm.runInContext(source.slice(start, end), context);
  context.focusProjectNavigation();
  assert.deepEqual(calls.splice(0), ["sessions", "runtime-project-search"]);
  mobile = true;
  context.focusProjectNavigation();
  assert.deepEqual(calls, ["sessions", [true, false, "runtime-project-search"]]);
});

test("project shortcut respects locked state, composition, and unmodified typing", async () => {
  const source = await readFile(new URL("../dist/runtime.js", import.meta.url), "utf8");
  const start = source.indexOf('document.addEventListener("keydown", (event) => {');
  const end = source.indexOf('\n});', start) + 4;
  let handler;
  let opened = 0;
  let prevented = 0;
  const shell = { hidden: false, classList: { contains: () => false } };
  const context = vm.createContext({
    document: { addEventListener(_name, callback) { handler = callback; }, querySelector: () => null },
    el: () => shell,
    focusProjectNavigation() { opened++; },
  });
  vm.runInContext(source.slice(start, end), context);
  const key = (overrides = {}) => handler({ key: "k", preventDefault() { prevented++; }, ...overrides });
  key();
  key({ ctrlKey: true, isComposing: true });
  key({ ctrlKey: true, altKey: true });
  shell.hidden = true;
  key({ metaKey: true });
  assert.equal(opened, 0);
  assert.equal(prevented, 0);
  shell.hidden = false;
  key({ metaKey: true });
  key({ ctrlKey: true, key: "K" });
  assert.equal(opened, 2);
  assert.equal(prevented, 2);
});

test("mobile project search only receives focus while navigation remains open", async () => {
  const source = await readFile(new URL("../dist/runtime.js", import.meta.url), "utf8");
  const start = source.indexOf("function setMobileNavigationOpen(");
  const end = source.indexOf("\n}", start) + 2;
  const classes = new Set();
  const callbacks = [];
  const focused = [];
  const context = vm.createContext({
    el(id) {
      return {
        classList: {
          toggle(name, enabled) { if (enabled) classes.add(name); else classes.delete(name); },
          contains(name) { return classes.has(name); },
        },
        setAttribute() {}, removeAttribute() {}, focus() { focused.push(id); },
      };
    },
    mobileNavigationViewport: () => true,
    closeAppearanceMenus() {}, closeTopbarMore() {}, closeRuntimeInspector() {},
    window: { setTimeout(callback) { callbacks.push(callback); } },
  });
  vm.runInContext(source.slice(start, end), context);
  context.setMobileNavigationOpen(true, false, "runtime-project-search");
  context.setMobileNavigationOpen(false);
  callbacks.shift()();
  assert.deepEqual(focused, []);
  context.setMobileNavigationOpen(true, false, "runtime-project-search");
  callbacks.shift()();
  assert.deepEqual(focused, ["runtime-project-search"]);
});

test("retained message search matches body and resolution without mutating messages", async () => {
  const source = await readFile(new URL("../dist/runtime.js", import.meta.url), "utf8");
  const start = source.indexOf("function runtimeSearchMatches(");
  const end = source.indexOf("function renderCollaboration(", start);
  const messages = [
    { message_id: "a", message: "Build failed", resolution: "Fixed Unicode 路径" },
    { message_id: "b", message: "Pending", author_session_id: "worker-2" },
  ];
  const original = JSON.stringify(messages);
  const cards = messages.map((message) => ({ dataset: { messageId: message.message_id }, hidden: false }));
  const separator = { hidden: false };
  const input = { value: "FIXED 路径" };
  let status;
  const context = vm.createContext({
    state: { collaboration: { messages } },
    el: () => input,
    setText: (_id, text) => { status = text; },
    document: { querySelectorAll: (selector) => selector.includes(".message-card") ? cards : [separator] },
  });
  vm.runInContext(source.slice(start, end), context);
  context.filterCollaborationMessages();
  assert.deepEqual(cards.map((card) => card.hidden), [false, true]);
  assert.equal(status, "1 / 2");
  assert.equal(separator.hidden, true);
  input.value = "missing";
  context.filterCollaborationMessages();
  assert.equal(status, "0 / 2");
  input.value = "";
  context.filterCollaborationMessages();
  assert.deepEqual(cards.map((card) => card.hidden), [false, false]);
  assert.equal(separator.hidden, false);
  assert.equal(JSON.stringify(messages), original);
});

test("workspace disclosure survives rerender and ignores detached toggle events", async () => {
  const source = await readFile(new URL("../dist/runtime.js", import.meta.url), "utf8");
  const start = source.indexOf('const workspace = document.createElement("details")');
  const end = source.indexOf('const row = document.createElement("summary")', start);
  const stored = new Map();
  let disclosure;
  const context = vm.createContext({
    clientId: "runner-a", project: { id: "project-a" }, state: { selectedProject: "project-a" },
    window: { localStorage: { getItem: (key) => stored.get(key), setItem: (key, value) => stored.set(key, value) } },
    document: { createElement: () => (disclosure = {
      isConnected: true,
      addEventListener(_name, handler) { this.toggle = handler; },
    }) },
  });
  const render = () => vm.runInContext("(() => {" + source.slice(start, end) + "})()", context);
  render();
  assert.equal(disclosure.open, true);
  disclosure.open = false;
  disclosure.toggle();
  render();
  assert.equal(disclosure.open, false);
  const detached = disclosure;
  detached.isConnected = false;
  detached.open = true;
  detached.toggle();
  render();
  assert.equal(disclosure.open, false);
  context.clientId = "runner-b";
  render();
  assert.equal(disclosure.open, true);
});
