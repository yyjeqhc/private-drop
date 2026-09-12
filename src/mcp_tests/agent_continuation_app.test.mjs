import test from "node:test";
import assert from "node:assert/strict";
import { app, flush, toolResult } from "./app_test_support.mjs";

const binding_id = `wc_host_binding_${"3".repeat(32)}`;
const wake = {
  wake_id: `wc_wake_${"4".repeat(32)}`, attempt_id: `wc_wake_attempt_${"5".repeat(32)}`,
  state: "claimed", revision: 2, dispatch_observation: null,
};
const projection = {
  version: 1, agent_id: `wc_dagent_${"1".repeat(32)}`, endpoint_id: `wc_endpoint_${"2".repeat(32)}`,
  controller_generation: 1, display_name: "Reviewer", queued_delivery_count: 1,
  host_binding: { bound: true }, wake: { ...wake, state: "pending", revision: 1 },
  dispatch_observation: null,
};
const prepared = () => toolResult({ dispatch_observation: "dispatch_prepared" }, { automatic_message: "Exact test continuation" });
const hostMessages = view => view.sent.filter(message => message.method === "ui/message");

async function boundView() {
  const view = app("mcp_agent_continuation_app.html");
  await view.initialize();
  view.result({ agent_continuation: projection });
  await flush();
  await view.reply(view.calls("agent_continuation_bind").at(-1), toolResult({ agent_continuation: projection }, { binding_id }));
  return view;
}

for (const outcome of ["success", "error", "timeout"]) {
  for (const early of [true, false]) {
    test(`continuation result ${early ? "before" : "after"} initialize ${outcome}`, async () => {
      const view = app("mcp_agent_continuation_app.html");
      if (early) {
        view.result({ agent_continuation: projection });
        await flush();
        assert.equal(view.calls("agent_continuation_bind").length, 0);
      }
      await view.initialize(outcome);
      if (!early) view.result({ agent_continuation: projection });
      await flush();
      assert.equal(view.nodes.agent.textContent, projection.display_name);
      assert.equal(view.calls("agent_continuation_bind").length, outcome === "success" ? 1 : 0);
    });
  }
}

for (const stage of ["acquire", "prepare"]) {
  test(`backgrounding during ${stage} does not dispatch a Host message`, async () => {
    const view = await boundView();
    await view.reply(view.calls("agent_continuation_state").at(-1), toolResult({ agent_continuation: projection }));
    const acquisition = view.calls("agent_continuation_wake_acquire").at(-1);
    if (stage === "acquire") await view.visibility(true);
    await view.reply(acquisition, toolResult({ wake }));
    if (stage === "prepare") {
      await view.visibility(true);
      await view.reply(view.calls("agent_continuation_wake_prepare").at(-1), prepared());
      assert.equal(hostMessages(view).length, 0);
      assert.equal(view.calls("agent_continuation_wake_finish").at(-1).params.arguments.outcome, "delivery_unknown");
    } else {
      assert.equal(view.calls("agent_continuation_wake_prepare").length, 0);
    }
    assert.equal(hostMessages(view).length, 0);
  });
}

test("Host dispatch timeout is finished as unknown and the same Attempt is never resent", async () => {
  const view = await boundView();
  await view.reply(view.calls("agent_continuation_state").at(-1), toolResult({ agent_continuation: projection }));
  await view.reply(view.calls("agent_continuation_wake_acquire").at(-1), toolResult({ wake }));
  await view.reply(view.calls("agent_continuation_wake_prepare").at(-1), prepared());
  assert.equal(hostMessages(view).length, 1);
  await view.fireTimers(10000);
  const finish = view.calls("agent_continuation_wake_finish").at(-1);
  assert.equal(finish.params.arguments.outcome, "delivery_unknown");
  await view.reply(finish, toolResult({}));
  await view.fireTimers(3000);
  await view.reply(view.calls("agent_continuation_state").at(-1), toolResult({ agent_continuation: {
    ...projection, dispatch_observation: "dispatch_unknown",
  } }));
  await view.reply(view.calls("agent_continuation_wake_acquire").at(-1), toolResult({ wake: {
    ...wake, dispatch_observation: "dispatch_unknown",
  } }));
  assert.equal(hostMessages(view).length, 1);
});

test("finish failure keeps the old claim until its ACK is reconciled before acquiring a successor", async () => {
  const view = await boundView();
  await view.reply(view.calls("agent_continuation_state").at(-1), toolResult({ agent_continuation: projection }));
  await view.reply(view.calls("agent_continuation_wake_acquire").at(-1), toolResult({ wake }));
  await view.reply(view.calls("agent_continuation_wake_prepare").at(-1), prepared());
  await view.reply(hostMessages(view).at(-1), {});
  const rejected = { structuredContent: { success: false, output: {} }, isError: true };
  await view.reply(view.calls("agent_continuation_wake_finish").at(-1), rejected);
  await view.fireTimers(3000);
  await view.reply(view.calls("agent_continuation_wake_finish").at(-1), rejected);
  const successor = { ...projection, wake: { ...projection.wake, wake_id: `wc_wake_${"6".repeat(32)}` } };
  await view.reply(view.calls("agent_continuation_state").at(-1), toolResult({ agent_continuation: successor }));
  assert.equal(view.calls("agent_continuation_wake_acquire").length, 1);
  await view.fireTimers(3000);
  const retry = view.calls("agent_continuation_wake_finish").at(-1);
  assert.equal(retry.params.arguments.attempt_id, wake.attempt_id);
  assert.equal(retry.params.arguments.outcome, "dispatch_accepted");
  await view.reply(retry, toolResult({}));
  await view.reply(view.calls("agent_continuation_state").at(-1), toolResult({ agent_continuation: successor }));
  assert.equal(view.calls("agent_continuation_wake_acquire").length, 2);
  assert.equal(hostMessages(view).length, 1);
});

test("one View carries successive Attempts and exact teardown stops coordination", async () => {
  const view = await boundView();
  for (let round = 0; round < 3; round++) {
    const currentWake = { ...wake, wake_id: `wc_wake_${String(round + 6).repeat(32)}`, attempt_id: `wc_wake_attempt_${String(round + 6).repeat(32)}` };
    const currentProjection = { ...projection, wake: currentWake };
    await view.reply(view.calls("agent_continuation_state").at(-1), toolResult({ agent_continuation: currentProjection }));
    await view.reply(view.calls("agent_continuation_wake_acquire").at(-1), toolResult({ wake: currentWake }));
    await view.reply(view.calls("agent_continuation_wake_prepare").at(-1), prepared());
    assert.equal(hostMessages(view).length, round + 1);
    await view.reply(hostMessages(view).at(-1), {});
    await view.reply(view.calls("agent_continuation_wake_finish").at(-1), toolResult({}));
    await view.fireTimers(3000);
  }
  view.result({ agent_continuation: projection });
  await flush();
  assert.equal(view.calls("agent_continuation_bind").length, 1);
  await view.teardown();
  const unbind = view.calls("agent_continuation_unbind").at(-1);
  assert.equal(unbind.params.arguments.binding_id, binding_id);
  const count = view.sent.length;
  await view.visibility(false);
  await view.fireTimers(3000);
  assert.equal(view.sent.length, count);
  assert.equal(view.timers.size, 0);
});
