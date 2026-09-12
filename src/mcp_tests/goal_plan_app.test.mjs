import test from "node:test";
import assert from "node:assert/strict";
import { app, flush, toolResult } from "./app_test_support.mjs";

const plan = {
  version: 1, goal_id: `wc_goal_${"1".repeat(32)}`, title: "Ship Goal",
  objective: "Review and validate the Goal flow", lifecycle: "active", revision: 1,
  updated_at_unix_ms: 1000, terminal_at_unix_ms: null,
  agent_task_count: 0, workflow_session_count: 0,
};

for (const outcome of ["success", "error", "timeout"]) {
  for (const early of [true, false]) {
    test(`Goal result ${early ? "before" : "after"} initialize ${outcome}`, async () => {
      const view = app("mcp_goal_plan_app.html");
      if (early) {
        view.result({ goal_plan: plan });
        await view.fireTimers(3000);
        assert.equal(view.calls("goal_plan_state").length, 0);
      }
      await view.initialize(outcome);
      if (!early) view.result({ goal_plan: plan });
      await view.fireTimers(3000);
      assert.equal(view.nodes.title.textContent, plan.title);
      assert.equal(view.calls("goal_plan_state").length, outcome === "success" ? 1 : 0);
    });
  }
}

test("terminal Goal stays terminal and stops polling after late active results and visibility changes", async () => {
  const view = app("mcp_goal_plan_app.html");
  await view.initialize();
  view.result({ goal_plan: plan });
  await view.fireTimers(3000);
  const request = view.calls("goal_plan_state").at(-1);
  await view.reply(request, toolResult({ goal_plan: {
    ...plan, lifecycle: "completed", revision: 2, terminal_at_unix_ms: 2000,
  } }));
  view.result({ goal_plan: plan });
  await view.fireTimers(3000);
  await view.visibility(false);
  assert.equal(view.nodes.lifecycle.textContent, "Completed");
  assert.equal(view.calls("goal_plan_state").length, 1);
});

test("Goal ignores a different identity and results after teardown", async () => {
  const view = app("mcp_goal_plan_app.html");
  await view.initialize();
  view.result({ goal_plan: plan });
  view.result({ goal_plan: { ...plan, goal_id: `wc_goal_${"2".repeat(32)}`, title: "Foreign" } });
  await view.teardown();
  view.result({ goal_plan: { ...plan, title: "Late", revision: 2 } });
  await flush();
  assert.equal(view.nodes.title.textContent, plan.title);
  assert.equal(view.timers.size, 0);
});
