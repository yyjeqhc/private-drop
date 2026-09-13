import test from "node:test";
import assert from "node:assert/strict";
import { app, flush, toolResult } from "./app_test_support.mjs";

const project = "agent:special:demo";
const session_id = `wc_sess_${"1".repeat(32)}`;
const input = { project, session_id };
const baseState = {
  version: 1,
  project,
  session_id,
  state_version: `wr1_${"a".repeat(64)}`,
  workspace: {
    git_available: true,
    clean: false,
    branch: "feature/work",
    counts: {
      modified: 1, added: 0, deleted: 0, renamed: 0, copied: 0,
      untracked: 0, conflicted: 0, staged: 0, unstaged: 1,
    },
    files_total: 1,
    files: [{ path: "src/a.rs", status: "modified", kind: "tracked", staged: false, unstaged: true, additions: 4, deletions: 1 }],
    additions: 4,
    deletions: 1,
    line_stats_partial: false,
    truncated: false,
  },
  validation: {
    status: "passed", latest_status: "passed", current_status: "passed",
    successes: 3, failures: 0, unresolved_failures: 0, evidence_gaps: 0,
  },
  review: {
    available: true, total: 1, read_only_inspection_count: 0, search_count: 0,
    diff_review_count: 1, workspace_review_count: 1, hygiene_review_count: 0,
    tools: ["show_changes"],
  },
};

const nextState = {
  ...baseState,
  state_version: `wr1_${"b".repeat(64)}`,
  workspace: {
    ...baseState.workspace,
    clean: true,
    files_total: 0,
    files: [],
    additions: 0,
    deletions: 0,
  },
  validation: {
    ...baseState.validation,
    status: "mixed",
    latest_status: "failed",
    current_status: "failed",
    failures: 1,
    unresolved_failures: 1,
  },
  review: {
    ...baseState.review,
    total: 2,
    read_only_inspection_count: 1,
    tools: ["show_changes", "git_review_summary"],
  },
};

for (const first of ["input", "result"]) {
  test(`Work Result ${first}-first bootstrap locks exact identity and starts polling after initialize`, async () => {
    const view = app("mcp_work_result_app.html");
    if (first === "input") view.toolInput(input);
    else view.toolResult({ work_result: baseState });
    assert.equal(view.calls("work_result_state").length, 0);
    await view.initialize();
    if (first === "result") assert.equal(view.nodes.changesTitle.textContent, "Changed 1 file");
    assert.equal(view.calls("work_result_state").length, 1);
    assert.deepEqual({ ...view.calls("work_result_state")[0].params.arguments }, input);
    await view.reply(view.calls("work_result_state")[0], toolResult({ work_result: baseState }));
    assert.equal(view.nodes.changesTitle.textContent, "Changed 1 file");
    assert.equal(view.nodes.validationStatus.textContent, "Passed");
    assert.equal(view.nodes.reviewStatus.textContent, "Workspace reviewed · Diff inspected");
  });
}

test("matching Work input/result identity is idempotent and unchanged state avoids DOM rebuild", async () => {
  const view = app("mcp_work_result_app.html");
  view.toolResult({ work_result: baseState });
  await view.initialize();
  view.toolInput(input);
  view.toolResult({ work_result: baseState });
  assert.equal(view.calls("work_result_state").length, 1);
  view.nodes.files.textContent = "sentinel";
  await view.reply(view.calls("work_result_state")[0], toolResult({ work_result: baseState }));
  assert.equal(view.nodes.files.textContent, "sentinel");
  await view.fireTimers(3000);
  assert.equal(view.calls("work_result_state").length, 2);
  await view.reply(view.calls("work_result_state")[1], toolResult({ work_result: nextState }));
  assert.equal(view.nodes.changesTitle.textContent, "Clean");
  assert.equal(view.nodes.validationStatus.textContent, "Failed");
  assert.match(view.nodes.reviewStatus.textContent, /Committed range mapped/);
});

for (const first of ["input", "result"]) {
  test(`conflicting Work ${first}-first identity fails closed and stops polling`, async () => {
    const view = app("mcp_work_result_app.html");
    if (first === "input") view.toolInput(input);
    else view.toolResult({ work_result: baseState });
    await view.initialize();
    const request = view.calls("work_result_state")[0];
    const foreign = { project: "agent:special:other", session_id: `wc_sess_${"2".repeat(32)}` };
    if (first === "input") view.toolResult({ work_result: { ...baseState, ...foreign } });
    else view.toolInput(foreign);
    const count = view.sent.length;
    await view.reply(request, toolResult({ work_result: baseState }));
    await view.fireTimers(3000);
    await view.visibility(false);
    assert.equal(view.sent.length, count);
    assert.equal(view.nodes.status.textContent, "Invalid or conflicting Work identity");
    assert.equal(view.timers.size, 0);
  });
}

test("malformed authoritative Work state fails closed", async () => {
  const view = app("mcp_work_result_app.html");
  await view.initialize();
  view.toolInput(input);
  const request = view.calls("work_result_state")[0];
  await view.reply(request, toolResult({ work_result: { ...baseState, state_version: "bad" } }));
  assert.equal(view.nodes.status.textContent, "Invalid authoritative Work state");
  assert.equal(view.timers.size, 0);
});

test("foreground visibility immediately reconciles exact Work state", async () => {
  const view = app("mcp_work_result_app.html");
  await view.initialize();
  view.toolInput(input);
  await view.reply(view.calls("work_result_state")[0], toolResult({ work_result: baseState }));
  const before = view.calls("work_result_state").length;
  await view.visibility(true);
  assert.equal(view.calls("work_result_state").length, before);
  await view.visibility(false);
  assert.equal(view.calls("work_result_state").length, before + 1);
  assert.deepEqual({ ...view.calls("work_result_state").at(-1).params.arguments }, input);
});

for (const method of ["ui/resource-teardown", "pagehide", "beforeunload"]) {
  test(`${method} stops Work polling and ignores a late in-flight response`, async () => {
    const view = app("mcp_work_result_app.html");
    view.toolInput(input);
    await view.initialize();
    const request = view.calls("work_result_state")[0];
    await view.teardown(method);
    await view.reply(request, toolResult({ work_result: nextState }));
    const count = view.sent.length;
    await view.fireTimers(3000);
    await view.visibility(false);
    view.toolResult({ work_result: nextState });
    await flush();
    assert.equal(view.sent.length, count);
    assert.equal(view.timers.size, 0);
    assert.notEqual(view.nodes.changesTitle?.textContent, "Clean");
  });
}

test("invalid Work input never polls", async () => {
  for (const bad of [
    { project: "", session_id },
    { project, session_id: "wc_sess_bad!" },
    { project: [project], session_id },
  ]) {
    const view = app("mcp_work_result_app.html");
    view.toolInput(bad);
    await view.initialize();
    await view.fireTimers(3000);
    assert.equal(view.calls("work_result_state").length, 0);
    assert.equal(view.nodes.status.textContent, "Invalid or conflicting Work identity");
    assert.equal(view.timers.size, 0);
  }
});
