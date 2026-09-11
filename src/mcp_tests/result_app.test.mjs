import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { runInNewContext } from "node:vm";

const html = readFileSync(new URL("../mcp_result_app.html", import.meta.url), "utf8");
const script = html.match(/<script>([\s\S]*?)<\/script>/)[1];

function element() {
  return {
    style: {}, textContent: "", children: [],
    appendChild(child) { this.children.push(child); },
    removeChild(child) { this.children.splice(this.children.indexOf(child), 1); },
    get firstChild() { return this.children[0]; },
  };
}

function app() {
  const nodes = Object.fromEntries(["state", "title", "summary", "cards"].map(id => [id, element()]));
  const timers = new Map();
  const sent = [];
  const parent = { postMessage(message) { sent.push(message); } };
  let receive;
  runInNewContext(script, {
    document: { getElementById: id => nodes[id], createElement: element },
    window: { parent, addEventListener: (_, listener) => { receive = listener; } },
    setTimeout: callback => { timers.set(1, callback); return 1; },
    clearTimeout: id => timers.delete(id),
  });
  const deliver = (message, source = parent) => receive({ source, data: { jsonrpc: "2.0", ...message } });
  return {
    nodes, sent,
    result(presentation, source) {
      deliver({ method: "ui/notifications/tool-result", params: { _meta: { "webcodex/presentation": presentation } } }, source);
    },
    initialize(outcome) {
      if (outcome === "timeout") timers.get(1)();
      else deliver({ id: sent[0].id, ...(outcome === "success"
        ? { result: { protocolVersion: "2026-01-26" } }
        : { error: { message: "host rejected initialization" } }) });
    },
  };
}

const passed = { version: 1, kind: "validation_run", tool: "cargo_test", execution_state: "completed", passed: true, tests_run_count: 2, tests_passed: 2 };

for (const outcome of ["success", "error", "timeout"]) {
  for (const early of [true, false]) {
    test(`validation result survives initialize ${outcome}, result ${early ? "before" : "after"}`, async () => {
      const view = app();
      if (early) view.result(passed);
      view.initialize(outcome);
      await Promise.resolve();
      if (!early) view.result(passed);
      assert.equal(view.nodes.state.textContent, "Passed");
      assert.equal(view.nodes.summary.textContent, "2 tests · 2 passed");
      assert.equal(view.nodes.cards.children.length, 1);
      assert.deepEqual(view.sent.map(message => message.method), outcome === "success"
        ? ["ui/initialize", "ui/notifications/initialized"] : ["ui/initialize"]);
    });
  }
}

test("untrusted messages are ignored and later results replace earlier cards", () => {
  const view = app();
  view.result(passed, {});
  assert.equal(view.nodes.cards.children.length, 0);
  view.result(passed);
  view.result({ ...passed, passed: false, failure_kind: "validation_failed" });
  assert.equal(view.nodes.state.textContent, "Validation failed");
  assert.equal(view.nodes.cards.children.length, 1);
});
