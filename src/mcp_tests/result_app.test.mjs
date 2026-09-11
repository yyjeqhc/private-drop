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
const cleanGit = {
  version: 1, kind: "git_changes",
  branch: "main", upstream_status: "absent", clean: true,
  files_total: 0, files_truncated: false, output_truncated: false, items_truncated: false,
  counts: { modified: 0, added: 0, deleted: 0, renamed: 0, copied: 0, untracked: 0, conflicted: 0, staged: 0, unstaged: 0 },
  files: [],
};
const partialReview = {
  version: 1, kind: "git_review", deterministic: true, truncated: true, items_truncated: true,
  scope: { base: "aaaaaaaa", head: "bbbbbbbb", base_is_ancestor: true, commit_count: 1 },
  stats: { files_changed: 2, insertions: 3, deletions: 1, binary_files: 0 },
  coverage: { production_changed: true, tests_changed: false, docs_changed: false, partial: true },
  files: [{ path: "src/lib.rs", path_omitted: false, status: "modified", additions: 3, deletions: 1, binary: false, gitlink: false, classes: ["production"] }],
};

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

for (const outcome of ["success", "error", "timeout"]) {
  for (const early of [true, false]) {
    test(`git changes survives initialize ${outcome}, result ${early ? "before" : "after"}`, async () => {
      const view = app();
      if (early) view.result(cleanGit);
      view.initialize(outcome);
      await Promise.resolve();
      if (!early) view.result(cleanGit);
      assert.equal(view.nodes.state.textContent, "Clean");
      assert.equal(view.nodes.summary.textContent, "main · 0 changed files · upstream: absent");
      assert.equal(view.nodes.cards.children.length, 1);
    });
  }
}

test("git review renders bounded committed-range metadata", () => {
  const view = app();
  view.result(partialReview);
  assert.equal(view.nodes.state.textContent, "Partial");
  assert.equal(view.nodes.summary.textContent, "1 commits · 2 files · +3 · −1 · bounded view");
  assert.equal(view.nodes.cards.children.length, 1);
});

test("git review distinguishes requested commits from the actual merge-base diff", () => {
  const view = app();
  view.result({
    ...partialReview,
    scope: { base: "aaaaaaaa", head: "bbbbbbbb", merge_base: "cccccccc", base_is_ancestor: false, commit_count: 1 },
  });
  const text = node => [node.textContent, ...node.children.map(text)].join(" ");
  const details = text(view.nodes.cards);
  assert.match(details, /Requested range aaaaaaaa → bbbbbbbb/);
  assert.match(details, /Diff range cccccccc → bbbbbbbb/);
  assert.match(details, /Base is ancestor no/);
});

test("git review does not infer a diff range when merge-base observation failed", () => {
  const view = app();
  view.result({
    version: 1, kind: "git_review", reason_code: "no_merge_base", deterministic: true,
    scope: { base: "aaaaaaaa", head: "bbbbbbbb" },
  });
  const text = node => [node.textContent, ...node.children.map(text)].join(" ");
  const details = text(view.nodes.cards);
  assert.equal(view.nodes.state.textContent, "Unavailable");
  assert.match(details, /Requested range aaaaaaaa → bbbbbbbb/);
  assert.doesNotMatch(details, /Diff range/);
});

test("untrusted messages are ignored and later results replace earlier cards", () => {
  const view = app();
  view.result(passed, {});
  assert.equal(view.nodes.cards.children.length, 0);
  view.result(passed);
  view.result({ ...passed, passed: false, failure_kind: "validation_failed" });
  assert.equal(view.nodes.state.textContent, "Validation failed");
  assert.equal(view.nodes.cards.children.length, 1);
});
