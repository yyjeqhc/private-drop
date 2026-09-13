import test from "node:test";
import assert from "node:assert/strict";
import { app, flush, toolResult } from "./app_test_support.mjs";

const plan = {
  version: 1, goal_id: `wc_goal_${"1".repeat(32)}`, title: "Ship Goal",
  objective: "Review and validate the Goal flow", lifecycle: "active", revision: 1,
  updated_at_unix_ms: 1000, terminal_at_unix_ms: null,
  agent_task_count: 0, workflow_session_count: 0,
};
const input = { goal_id: plan.goal_id };

for (const outcome of ["success", "error", "timeout"]) {
  for (const early of [true, false]) {
    test(`Goal input ${early ? "before" : "after"} initialize ${outcome}, without initial result`, async () => {
      const view = app("mcp_goal_plan_app.html");
      assert.equal(view.nodes.status.textContent, "App active · initializing Host");
      if (early) view.toolInput(input);
      assert.equal(view.calls("goal_plan_state").length, 0);
      await view.initialize(outcome);
      if (outcome === "success" && !early) {
        assert.equal(view.nodes.status.textContent, "Host initialized · waiting for exact tool identity");
      }
      if (!early) view.toolInput(input);
      assert.equal(view.calls("goal_plan_state").length, outcome === "success" ? 1 : 0);
      assert.equal(view.nodes.status.textContent, outcome === "success"
        ? "Exact identity received · reading authoritative Goal state" : "Host initialization unavailable");
      if (outcome === "success") {
        assert.deepEqual({ ...view.calls("goal_plan_state")[0].params.arguments }, input);
        await view.reply(view.calls("goal_plan_state")[0], toolResult({ goal_plan: plan }));
        view.toolInput(input);
        assert.equal(view.calls("goal_plan_state").length, 1);
        await view.fireTimers(3000);
        assert.equal(view.calls("goal_plan_state").length, 2);
        await view.reply(view.calls("goal_plan_state")[1], toolResult({ goal_plan: {
          ...plan, lifecycle: "completed", revision: 2, terminal_at_unix_ms: 2000,
        } }));
        assert.equal(view.nodes.lifecycle.textContent, "Completed");
        assert.equal(view.timers.size, 0);
      }
    });
  }
}

for (const order of [
  ["initialize", "input", "result"], ["input", "initialize", "result"],
  ["result", "initialize", "input"], ["initialize", "result", "input"],
  ["input", "result", "initialize"], ["result", "input", "initialize"],
]) {
  test(`matching Goal bootstrap is idempotent: ${order.join(" -> ")}`, async () => {
    const view = app("mcp_goal_plan_app.html");
    for (const step of order) {
      if (step === "initialize") await view.initialize();
      else if (step === "input") view.toolInput(input);
      else view.toolResult({ goal_plan: plan });
    }
    await view.reply(view.calls("goal_plan_state")[0], toolResult({ goal_plan: { ...plan, title: "Current", revision: 2 } }));
    view.toolInput(input);
    view.toolInput(input);
    view.toolResult({ goal_plan: plan });
    assert.equal(view.calls("goal_plan_state").length, 1);
    assert.equal(view.nodes.title.textContent, "Current");
    await view.fireTimers(3000);
    assert.equal(view.calls("goal_plan_state").length, 2);
  });
}

for (const stage of ["initialize", "poll", "idle"]) {
  for (const first of ["input", "result"]) {
    test(`Goal ${first}-first conflict while ${stage} is pending stops exact polling`, async () => {
      const view = app("mcp_goal_plan_app.html");
      if (first === "input") view.toolInput(input);
      else view.toolResult({ goal_plan: plan });
      let pending = view.sent[0];
      let response = { protocolVersion: "2026-01-26" };
      if (stage !== "initialize") {
        await view.initialize();
        pending = view.calls("goal_plan_state")[0];
        response = toolResult({ goal_plan: plan });
      }
      if (stage === "idle") await view.reply(pending, response);
      const foreign = `wc_goal_${"2".repeat(32)}`;
      if (first === "input") view.toolResult({ goal_plan: { ...plan, goal_id: foreign, title: "Foreign" } });
      else view.toolInput({ goal_id: foreign });
      const count = view.sent.length;
      await view.reply(pending, response);
      view.toolInput(input);
      view.toolResult({ goal_plan: { ...plan, title: "Late", revision: 2 } });
      await view.fireTimers(3000);
      await view.visibility(false);
      assert.equal(view.sent.length, count);
      assert.equal(view.nodes.status.textContent, "Invalid or conflicting Goal identity");
      assert.notEqual(view.nodes.title?.textContent, "Foreign");
      assert.notEqual(view.nodes.title?.textContent, "Late");
      assert.equal(view.timers.size, 0);
      for (const call of view.calls("goal_plan_state")) assert.deepEqual({ ...call.params.arguments }, input);
    });
  }
}

for (const goal_id of [undefined, null, "", "wc_goal_1", `wc_goal_${"A".repeat(32)}`, plan.goal_id + "extra", [plan.goal_id], 1]) {
  test(`invalid Goal input cannot start polling: ${JSON.stringify(goal_id)}`, async () => {
    const view = app("mcp_goal_plan_app.html");
    view.toolInput({ goal_id });
    await view.initialize();
    view.toolResult({ goal_plan: plan });
    await view.fireTimers(3000);
    assert.equal(view.calls("goal_plan_state").length, 0);
    assert.equal(view.nodes.status.textContent, "Invalid or conflicting Goal identity");
    assert.equal(view.timers.size, 0);
  });
}

test("Goal accepts only parent complete input with canonical arguments", async () => {
  const view = app("mcp_goal_plan_app.html");
  await view.initialize();
  view.toolInput(input, {});
  view.notification("ui/notifications/tool-input-partial", { arguments: input });
  assert.equal(view.calls("goal_plan_state").length, 0);
  assert.equal(view.nodes.status.textContent, "Host initialized · waiting for exact tool identity");
  view.notification("ui/notifications/tool-input", { input });
  assert.equal(view.calls("goal_plan_state").length, 0);
  assert.equal(view.nodes.status.textContent, "Invalid or conflicting Goal identity");
});

test("an input-only Goal retries an unavailable first read without another notification", async () => {
  const view = app("mcp_goal_plan_app.html");
  await view.initialize();
  view.toolInput(input);
  await view.fireTimers(10000);
  assert.equal(view.nodes.status.textContent, "State refresh pending");
  await view.fireTimers(3000);
  assert.equal(view.calls("goal_plan_state").length, 2);
  await view.reply(view.calls("goal_plan_state")[1], toolResult({ goal_plan: plan }));
  assert.equal(view.nodes.status.textContent, "Tracking authoritative Goal state");
});


test("Goal accepts a nested CallToolResult returned by the Host bridge", async () => {
  const view = app("mcp_goal_plan_app.html");
  await view.initialize();
  view.toolInput(input);
  await view.reply(view.calls("goal_plan_state")[0], { result: toolResult({ goal_plan: plan }) });
  assert.equal(view.nodes.title.textContent, plan.title);
  assert.equal(view.nodes.lifecycle.textContent, "Active");
  assert.equal(view.nodes.status.textContent, "Tracking authoritative Goal state");
});

test("a conflicting nested CallToolResult still stops Goal polling", async () => {
  const view = app("mcp_goal_plan_app.html");
  await view.initialize();
  view.toolInput(input);
  await view.reply(view.calls("goal_plan_state")[0], { result: toolResult({ goal_plan: {
    ...plan, goal_id: `wc_goal_${"2".repeat(32)}`,
  } }) });
  assert.equal(view.nodes.status.textContent, "Invalid or conflicting Goal identity");
  assert.equal(view.timers.size, 0);
});

test("a conflicting authoritative Goal response stops polling", async () => {
  const view = app("mcp_goal_plan_app.html");
  await view.initialize();
  view.toolInput(input);
  await view.reply(view.calls("goal_plan_state")[0], toolResult({ goal_plan: {
    ...plan, goal_id: `wc_goal_${"2".repeat(32)}`,
  } }));
  assert.equal(view.nodes.status.textContent, "Invalid or conflicting Goal identity");
  assert.equal(view.timers.size, 0);
});

for (const method of ["ui/resource-teardown", "pagehide", "beforeunload"]) {
  test(`input-only Goal ${method} discards a pending poll and stops timers`, async () => {
    const view = app("mcp_goal_plan_app.html");
    view.toolInput(input);
    await view.initialize();
    await view.teardown(method);
    await view.reply(view.calls("goal_plan_state")[0], toolResult({ goal_plan: plan }));
    const count = view.sent.length;
    view.toolInput(input);
    view.toolResult({ goal_plan: plan });
    await view.visibility(false);
    await view.fireTimers(3000);
    assert.equal(view.sent.length, count);
    assert.equal(view.timers.size, 0);
    assert.notEqual(view.nodes.title?.textContent, plan.title);
  });
}

for (const outcome of ["success", "error", "timeout"]) {
  for (const early of [true, false]) {
    test(`Goal result ${early ? "before" : "after"} initialize ${outcome}`, async () => {
      const view = app("mcp_goal_plan_app.html");
      if (early) {
        view.toolResult({ goal_plan: plan });
        await view.fireTimers(3000);
        assert.equal(view.calls("goal_plan_state").length, 0);
      }
      await view.initialize(outcome);
      if (!early) view.toolResult({ goal_plan: plan });
      await view.fireTimers(3000);
      assert.equal(view.nodes.title.textContent, plan.title);
      assert.equal(view.calls("goal_plan_state").length, outcome === "success" ? 1 : 0);
    });
  }
}

test("terminal Goal stays terminal and stops polling after late active results and visibility changes", async () => {
  const view = app("mcp_goal_plan_app.html");
  await view.initialize();
  view.toolResult({ goal_plan: plan });
  await view.fireTimers(3000);
  const request = view.calls("goal_plan_state").at(-1);
  await view.reply(request, toolResult({ goal_plan: {
    ...plan, lifecycle: "completed", revision: 2, terminal_at_unix_ms: 2000,
  } }));
  view.toolResult({ goal_plan: plan });
  await view.fireTimers(3000);
  await view.visibility(false);
  assert.equal(view.nodes.lifecycle.textContent, "Completed");
  assert.equal(view.calls("goal_plan_state").length, 1);
});

test("Goal rejects a different identity and ignores results after teardown", async () => {
  const view = app("mcp_goal_plan_app.html");
  await view.initialize();
  view.toolResult({ goal_plan: plan });
  view.toolResult({ goal_plan: { ...plan, goal_id: `wc_goal_${"2".repeat(32)}`, title: "Foreign" } });
  assert.equal(view.nodes.status.textContent, "Invalid or conflicting Goal identity");
  await view.teardown();
  view.toolResult({ goal_plan: { ...plan, title: "Late", revision: 2 } });
  await flush();
  assert.equal(view.nodes.title.textContent, plan.title);
  assert.equal(view.timers.size, 0);
});
