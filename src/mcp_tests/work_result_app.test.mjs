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
    status: "passed", latest_status: "passed", current_status: "passed", history_partial: false,
    successes: 3, failures: 0, unresolved_failures: 0, evidence_gaps: 0,
  },
  review: {
    available: true, total: 1, history_partial: false, read_only_inspection_count: 0, search_count: 0,
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
  test(`Work Result ${first}-first bootstrap renders the initial snapshot without automatic refresh`, async () => {
    const view = app("mcp_work_result_app.html");
    if (first === "input") view.toolInput(input);
    else view.toolResult({ work_result: baseState });
    assert.equal(view.calls("work_result_state").length, 0);
    await view.initialize();
    if (first === "input") view.toolResult({ work_result: baseState });
    else view.toolInput(input);
    await flush();
    assert.equal(view.calls("work_result_state").length, 0);
    assert.equal(view.nodes.changesTitle.textContent, "Changed 1 file");
    assert.equal(view.nodes.validationStatus.textContent, "Passed");
    assert.equal(view.nodes.reviewStatus.textContent, "Workspace reviewed · Diff inspected");
    assert.equal(view.nodes.refresh.disabled, false);
  });
}

test("input-only bootstrap waits for a user Refresh before reading state", async () => {
  const view = app("mcp_work_result_app.html");
  view.toolInput(input);
  await view.initialize();
  assert.equal(view.calls("work_result_state").length, 0);
  assert.equal(view.nodes.refresh.disabled, false);
  assert.match(view.nodes.status.textContent, /Waiting for Work snapshot/);
  view.nodes.refresh.onclick();
  await flush();
  assert.equal(view.calls("work_result_state").length, 1);
  assert.deepEqual({ ...view.calls("work_result_state")[0].params.arguments }, input);
  await view.reply(view.calls("work_result_state")[0], toolResult({ work_result: baseState }));
  assert.equal(view.nodes.changesTitle.textContent, "Changed 1 file");
  assert.equal(view.nodes.status.textContent, "Updated");
});

test("matching Work input/result identity is idempotent and unchanged initial state avoids DOM rebuild", async () => {
  const view = app("mcp_work_result_app.html");
  view.toolResult({ work_result: baseState });
  await view.initialize();
  view.toolInput(input);
  view.nodes.files.textContent = "sentinel";
  view.toolResult({ work_result: baseState });
  await flush();
  assert.equal(view.calls("work_result_state").length, 0);
  assert.equal(view.nodes.files.textContent, "sentinel");
});

test("time and visibility changes produce zero automatic Work state requests", async () => {
  const view = app("mcp_work_result_app.html");
  view.toolInput(input);
  view.toolResult({ work_result: baseState });
  await view.initialize();
  await view.fireTimers(3000);
  await view.fireTimers(10000);
  await view.visibility(true);
  await view.visibility(false);
  assert.equal(view.calls("work_result_state").length, 0);
  assert.equal(view.timers.size, 0);
});

test("user Refresh performs one exact state read and updates the snapshot", async () => {
  const view = app("mcp_work_result_app.html");
  view.toolInput(input);
  view.toolResult({ work_result: baseState });
  await view.initialize();
  view.nodes.refresh.onclick();
  await flush();
  assert.equal(view.calls("work_result_state").length, 1);
  assert.deepEqual({ ...view.calls("work_result_state")[0].params.arguments }, input);
  assert.equal(view.nodes.refresh.disabled, true);
  assert.equal(view.nodes.refresh.textContent, "Refreshing…");
  await view.reply(view.calls("work_result_state")[0], toolResult({ work_result: nextState }));
  assert.equal(view.nodes.changesTitle.textContent, "Clean");
  assert.equal(view.nodes.validationStatus.textContent, "Failed");
  assert.match(view.nodes.reviewStatus.textContent, /Committed range mapped/);
  assert.equal(view.nodes.status.textContent, "Updated");
  assert.equal(view.nodes.refresh.disabled, false);
});

test("in-flight Refresh clicks coalesce and a later user click performs one new request", async () => {
  const view = app("mcp_work_result_app.html");
  view.toolResult({ work_result: baseState });
  await view.initialize();
  view.toolInput(input);
  view.nodes.refresh.onclick();
  view.nodes.refresh.onclick();
  view.nodes.refresh.onclick();
  await flush();
  assert.equal(view.calls("work_result_state").length, 1);
  await view.reply(view.calls("work_result_state")[0], toolResult({ work_result: baseState }));
  assert.equal(view.nodes.status.textContent, "Up to date");
  view.nodes.refresh.onclick();
  await flush();
  assert.equal(view.calls("work_result_state").length, 2);
  assert.deepEqual({ ...view.calls("work_result_state")[1].params.arguments }, input);
});

test("failed Refresh preserves the last valid snapshot and remains retryable", async () => {
  const view = app("mcp_work_result_app.html");
  view.toolResult({ work_result: baseState });
  await view.initialize();
  view.toolInput(input);

  view.nodes.refresh.onclick();
  await flush();
  await view.reply(view.calls("work_result_state")[0], { structuredContent: { success: false, output: { error_kind: "workspace_unavailable" } } });
  assert.equal(view.nodes.changesTitle.textContent, "Changed 1 file");
  assert.match(view.nodes.status.textContent, /Refresh unavailable/);
  assert.equal(view.nodes.refresh.disabled, false);

  view.nodes.refresh.onclick();
  await flush();
  assert.equal(view.calls("work_result_state").length, 2);
  await view.reject(view.calls("work_result_state")[1]);
  assert.equal(view.nodes.changesTitle.textContent, "Changed 1 file");
  assert.match(view.nodes.status.textContent, /Refresh unavailable/);
  assert.equal(view.nodes.refresh.disabled, false);

  view.nodes.refresh.onclick();
  await flush();
  assert.equal(view.calls("work_result_state").length, 3);
  await view.reply(view.calls("work_result_state")[2], toolResult({ work_result: baseState }));
  assert.equal(view.nodes.status.textContent, "Up to date");
});

for (const first of ["input", "result"]) {
  test(`conflicting Work ${first}-first identity fails closed without a state request`, async () => {
    const view = app("mcp_work_result_app.html");
    if (first === "input") view.toolInput(input);
    else view.toolResult({ work_result: baseState });
    await view.initialize();
    const foreign = { project: "agent:special:other", session_id: `wc_sess_${"2".repeat(32)}` };
    if (first === "input") view.toolResult({ work_result: { ...baseState, ...foreign } });
    else view.toolInput(foreign);
    await flush();
    assert.equal(view.calls("work_result_state").length, 0);
    assert.equal(view.nodes.status.textContent, "Invalid or conflicting Work identity");
    assert.equal(view.nodes.refresh.disabled, true);
    assert.equal(view.timers.size, 0);
  });
}

test("malformed authoritative Refresh state fails closed", async () => {
  const view = app("mcp_work_result_app.html");
  view.toolResult({ work_result: baseState });
  await view.initialize();
  view.toolInput(input);
  view.nodes.refresh.onclick();
  await flush();
  await view.reply(view.calls("work_result_state")[0], toolResult({ work_result: { ...baseState, state_version: "bad" } }));
  assert.equal(view.nodes.status.textContent, "Invalid authoritative Work state");
  assert.equal(view.nodes.refresh.disabled, true);
});

test("partial evidence stays honest and exact files_total is not decorated as a lower bound", async () => {
  const files = Array.from({ length: 8 }, (_, index) => ({
    path: index === 0 ? "src/future.rs" : `src/file_${index}.rs`,
    status: index === 0 ? "future_status" : "modified",
    kind: "tracked",
    staged: false,
    unstaged: true,
  }));
  const partialState = {
    ...baseState,
    state_version: `wr1_${"c".repeat(64)}`,
    workspace: { ...baseState.workspace, files_total: 14, files, truncated: true, additions: undefined, deletions: undefined, line_stats_partial: true },
    validation: { ...baseState.validation, status: "unknown", latest_status: "unknown", current_status: "unknown", history_partial: true, successes: 0 },
    review: { ...baseState.review, available: false, total: 0, history_partial: true, tools: [] },
  };
  const view = app("mcp_work_result_app.html");
  view.toolResult({ work_result: partialState });
  await view.initialize();
  assert.equal(view.nodes.changesTitle.textContent, "Changed 14 files");
  assert.match(view.nodes.files.textContent, /^\? src\/future\.rs/m);
  assert.equal(view.nodes.validationStatus.textContent, "Unknown");
  assert.match(view.nodes.validationMeta.textContent, /history partial/);
  assert.equal(view.nodes.reviewStatus.textContent, "Review history partial");
  assert.match(view.nodes.reviewMeta.textContent, /Earlier review evidence/);
  assert.equal(view.calls("work_result_state").length, 0);
});

for (const method of ["ui/resource-teardown", "pagehide", "beforeunload"]) {
  test(`${method} ignores a late in-flight Refresh response and leaves no timer`, async () => {
    const view = app("mcp_work_result_app.html");
    view.toolResult({ work_result: baseState });
    await view.initialize();
    view.toolInput(input);
    view.nodes.refresh.onclick();
    await flush();
    const request = view.calls("work_result_state")[0];
    await view.teardown(method);
    await view.reply(request, toolResult({ work_result: nextState }));
    await view.visibility(false);
    assert.equal(view.calls("work_result_state").length, 1);
    assert.equal(view.timers.size, 0);
    assert.equal(view.nodes.changesTitle.textContent, "Changed 1 file");
  });
}

test("invalid Work input never refreshes", async () => {
  for (const bad of [
    { project: "", session_id },
    { project, session_id: "wc_sess_bad!" },
    { project: [project], session_id },
  ]) {
    const view = app("mcp_work_result_app.html");
    view.toolInput(bad);
    await flush();
    assert.equal(view.calls("work_result_state").length, 0);
    assert.equal(view.nodes.status.textContent, "Invalid or conflicting Work identity");
    assert.equal(view.nodes.refresh.disabled, true);
    assert.equal(view.timers.size, 0);
  }
});
