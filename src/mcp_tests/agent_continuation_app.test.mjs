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
const input = {
  agent_id: projection.agent_id, endpoint_id: projection.endpoint_id,
  expected_controller_generation: projection.controller_generation,
};
const prepared = () => toolResult({ dispatch_observation: "dispatch_prepared" }, { automatic_message: "Exact test continuation" });
const hostMessages = view => view.sent.filter(message => message.method === "ui/message");

async function boundView() {
  const view = app("mcp_agent_continuation_app.html");
  await view.initialize();
  view.toolInput(input);
  await flush();
  await view.reply(view.calls("agent_continuation_bind").at(-1), toolResult({ agent_continuation: projection }, { binding_id }));
  return view;
}

for (const outcome of ["success", "error", "timeout"]) {
  for (const early of [true, false]) {
    test(`continuation input ${early ? "before" : "after"} initialize ${outcome}, without initial result`, async () => {
      const view = app("mcp_agent_continuation_app.html");
      assert.equal(view.nodes.status.textContent, "App active · initializing Host");
      if (early) view.toolInput(input);
      assert.equal(view.calls("agent_continuation_bind").length, 0);
      await view.initialize(outcome);
      if (outcome === "success" && !early) {
        assert.equal(view.nodes.status.textContent, "Host initialized · waiting for exact tool identity");
      }
      if (!early) view.toolInput(input);
      await flush();
      assert.equal(view.calls("agent_continuation_bind").length, outcome === "success" ? 1 : 0);
      assert.equal(view.nodes.status.textContent, outcome === "success"
        ? "Exact identity received · binding Host carrier" : "Host initialization unavailable");
      if (outcome === "success") {
        assert.deepEqual({ ...view.calls("agent_continuation_bind")[0].params.arguments }, input);
        const quiet = { ...projection, wake: null, queued_delivery_count: 0 };
        await view.reply(view.calls("agent_continuation_bind")[0], toolResult({ agent_continuation: quiet }, { binding_id }));
        assert.equal(view.nodes.binding.textContent, "Host bound");
        assert.equal(view.nodes.status.textContent, "Host carrier live; no pending durable Wake");
        await view.reply(view.calls("agent_continuation_state")[0], toolResult({ agent_continuation: quiet }));
        await view.fireTimers(3000);
        assert.equal(view.calls("agent_continuation_state").length, 2);
        assert.deepEqual({ ...view.calls("agent_continuation_state")[1].params.arguments }, { ...input, binding_id });
      }
    });
  }
}

for (const order of [
  ["initialize", "input", "result"], ["input", "initialize", "result"],
  ["result", "initialize", "input"], ["initialize", "result", "input"],
  ["input", "result", "initialize"], ["result", "input", "initialize"],
]) {
  test(`continuation matching bootstrap is idempotent: ${order.join(" -> ")}`, async () => {
    const view = app("mcp_agent_continuation_app.html");
    for (const step of order) {
      if (step === "initialize") await view.initialize();
      else if (step === "input") view.toolInput(input);
      else view.toolResult({ agent_continuation: projection });
    }
    view.toolInput(input);
    view.toolResult({ agent_continuation: projection });
    assert.equal(view.calls("agent_continuation_bind").length, 1);
    await view.reply(view.calls("agent_continuation_bind")[0], toolResult({ agent_continuation: projection }, { binding_id }));
    const quiet = { ...projection, wake: null, queued_delivery_count: 0, display_name: "Current" };
    await view.reply(view.calls("agent_continuation_state")[0], toolResult({ agent_continuation: quiet }));
    view.toolInput(input);
    view.toolResult({ agent_continuation: projection });
    assert.equal(view.nodes.agent.textContent, "Current");
    assert.equal(view.nodes.wake.textContent, "None");
    assert.equal(view.nodes.binding.textContent, "Host bound");
    assert.equal(view.calls("agent_continuation_bind").length, 1);
  });
}

const conflicts = {
  agent_id: `wc_dagent_${"a".repeat(32)}`,
  endpoint_id: `wc_endpoint_${"b".repeat(32)}`,
  controller_generation: 2,
};
for (const [field, value] of Object.entries(conflicts)) {
  for (const stage of ["initialize", "bind", "state", "acquire", "prepare", "dispatch"]) {
    test(`conflicting ${field} while ${stage} is pending closes the carrier`, async () => {
      const view = app("mcp_agent_continuation_app.html");
      view.toolInput(input);
      let pending = view.sent[0];
      let response = { protocolVersion: "2026-01-26" };
      if (stage !== "initialize") {
        await view.initialize();
        pending = view.calls("agent_continuation_bind")[0];
        response = toolResult({ agent_continuation: projection }, { binding_id });
      }
      if (["state", "acquire", "prepare", "dispatch"].includes(stage)) {
        await view.reply(pending, response);
        pending = view.calls("agent_continuation_state")[0];
        response = toolResult({ agent_continuation: projection });
      }
      if (["acquire", "prepare", "dispatch"].includes(stage)) {
        await view.reply(pending, response);
        pending = view.calls("agent_continuation_wake_acquire")[0];
        response = toolResult({ wake });
      }
      if (["prepare", "dispatch"].includes(stage)) {
        await view.reply(pending, response);
        pending = view.calls("agent_continuation_wake_prepare")[0];
        response = prepared();
      }
      if (stage === "dispatch") {
        await view.reply(pending, response);
        pending = hostMessages(view)[0];
        response = {};
      }
      view.toolResult({ agent_continuation: { ...projection, [field]: value, display_name: "Foreign" } });
      const count = view.sent.length;
      await view.reply(pending, response);
      view.toolInput(input);
      view.toolResult({ agent_continuation: projection });
      await view.visibility(false);
      await view.fireTimers(3000);
      await view.fireTimers(10000);
      assert.equal(view.sent.length, count);
      assert.equal(view.timers.size, 0);
      assert.equal(view.nodes.status.textContent, "Invalid or conflicting continuation identity");
      assert.equal(view.nodes.binding.textContent, "Unavailable");
      assert.notEqual(view.nodes.agent?.textContent, "Foreign");
      assert.equal(hostMessages(view).length, stage === "dispatch" ? 1 : 0);
      for (const call of view.sent.filter(message => message.method === "tools/call")) {
        const args = call.params.arguments;
        assert.equal(args.agent_id, input.agent_id);
        assert.equal(args.endpoint_id, input.endpoint_id);
        assert.equal(args.expected_controller_generation, input.expected_controller_generation);
      }
    });
  }
  test(`result-first ${field} conflict in tool input fails before bind`, async () => {
    const view = app("mcp_agent_continuation_app.html");
    view.toolResult({ agent_continuation: projection });
    const key = field === "controller_generation" ? "expected_controller_generation" : field;
    view.toolInput({ ...input, [key]: value });
    await view.initialize();
    assert.equal(view.calls("agent_continuation_bind").length, 0);
    assert.equal(view.nodes.status.textContent, "Invalid or conflicting continuation identity");
  });
}

for (const invalid of [
  null, [], {},
  { ...input, agent_id: input.agent_id.replace("dagent", "agent") },
  { ...input, agent_id: `wc_dagent_${"A".repeat(32)}` },
  { ...input, agent_id: [input.agent_id] },
  { ...input, endpoint_id: `${input.endpoint_id}extra` },
  { ...input, endpoint_id: [input.endpoint_id] },
  ...[0, -1, 1.5, "1", Number.MAX_SAFE_INTEGER + 1, null].map(expected_controller_generation => ({ ...input, expected_controller_generation })),
]) {
  test(`invalid continuation input is terminal: ${JSON.stringify(invalid)}`, async () => {
    const view = app("mcp_agent_continuation_app.html");
    await view.initialize();
    view.toolInput(invalid);
    view.toolResult({ agent_continuation: projection });
    await flush();
    assert.equal(view.calls("agent_continuation_bind").length, 0);
    assert.equal(view.nodes.status.textContent, "Invalid or conflicting continuation identity");
    assert.equal(view.timers.size, 0);
  });
}

test("continuation accepts only parent complete input with canonical arguments", async () => {
  const view = app("mcp_agent_continuation_app.html");
  await view.initialize();
  view.toolInput(input, {});
  view.notification("ui/notifications/tool-input-partial", { arguments: input });
  assert.equal(view.calls("agent_continuation_bind").length, 0);
  assert.equal(view.nodes.status.textContent, "Host initialized · waiting for exact tool identity");
  view.notification("ui/notifications/tool-input", { input });
  assert.equal(view.calls("agent_continuation_bind").length, 0);
  assert.equal(view.nodes.status.textContent, "Invalid or conflicting continuation identity");
});

test("bind failure has a stable bounded diagnostic even after a late matching result", async () => {
  const view = app("mcp_agent_continuation_app.html");
  await view.initialize();
  view.toolInput(input);
  await view.fireTimers(10000);
  view.toolResult({ agent_continuation: projection });
  assert.equal(view.nodes.status.textContent, "Host binding unavailable; durable Agent state is unchanged");
  assert.equal(view.calls("agent_continuation_bind").length, 1);
  assert.equal(view.calls("agent_continuation_state").length, 0);
});

for (const method of ["ui/resource-teardown", "pagehide", "beforeunload"]) {
  test(`input-only carrier ${method} stops coordination and unbinds only its exact carrier`, async () => {
    const view = await boundView();
    await view.teardown(method);
    assert.deepEqual({ ...view.calls("agent_continuation_unbind")[0].params.arguments }, { ...input, binding_id });
    await view.reply(view.calls("agent_continuation_state")[0], toolResult({ agent_continuation: projection }));
    const count = view.sent.length;
    view.toolInput(input);
    view.toolResult({ agent_continuation: projection });
    await view.visibility(false);
    await view.fireTimers(3000);
    assert.equal(view.sent.length, count);
    assert.equal(view.timers.size, 0);
    assert.equal(hostMessages(view).length, 0);
  });
}

test("input-only background carrier heartbeats and reconciles immediately on foreground", async () => {
  const view = await boundView();
  await view.visibility(true);
  await view.reply(view.calls("agent_continuation_state")[0], toolResult({ agent_continuation: projection }));
  await view.fireTimers(15000);
  assert.equal(view.calls("agent_continuation_state").length, 2);
  await view.reply(view.calls("agent_continuation_state")[1], toolResult({ agent_continuation: projection }));
  assert.equal(view.calls("agent_continuation_wake_acquire").length, 0);
  await view.visibility(false);
  assert.equal(view.calls("agent_continuation_state").length, 3);
  await view.reply(view.calls("agent_continuation_state")[2], toolResult({ agent_continuation: projection }));
  assert.equal(view.calls("agent_continuation_wake_acquire").length, 1);
  assert.equal(view.nodes.status.textContent, "Reconciling authoritative durable Wake state");
});

test("input-only dispatch never displays its private binding or consume envelope", async () => {
  const view = await boundView();
  await view.reply(view.calls("agent_continuation_state")[0], toolResult({ agent_continuation: projection }));
  await view.reply(view.calls("agent_continuation_wake_acquire")[0], toolResult({ wake }));
  const consume_token = `wc_wake_consume_${"c".repeat(32)}`;
  const automatic_message = `Exact test continuation consume_token=${consume_token}`;
  await view.reply(view.calls("agent_continuation_wake_prepare")[0], toolResult(
    { dispatch_observation: "dispatch_prepared" }, { automatic_message },
  ));
  assert.equal(hostMessages(view)[0].params.content[0].text, automatic_message);
  const displayed = Object.values(view.nodes).map(node => node.textContent).join("\n");
  for (const secret of [binding_id, consume_token, automatic_message]) assert.ok(!displayed.includes(secret));
});

for (const stage of ["bind", "state"]) {
  test(`a conflicting authoritative ${stage} response cannot retarget the carrier`, async () => {
    const view = app("mcp_agent_continuation_app.html");
    await view.initialize();
    view.toolInput(input);
    if (stage === "state") {
      await view.reply(view.calls("agent_continuation_bind")[0], toolResult({ agent_continuation: projection }, { binding_id }));
    }
    await view.reply(view.calls(`agent_continuation_${stage}`)[0], toolResult(
      { agent_continuation: { ...projection, endpoint_id: conflicts.endpoint_id } }, { binding_id },
    ));
    assert.equal(view.nodes.status.textContent, "Invalid or conflicting continuation identity");
    assert.equal(view.calls("agent_continuation_wake_acquire").length, 0);
    assert.equal(view.timers.size, 0);
  });
}

for (const outcome of ["success", "error", "timeout"]) {
  for (const early of [true, false]) {
    test(`continuation result ${early ? "before" : "after"} initialize ${outcome}`, async () => {
      const view = app("mcp_agent_continuation_app.html");
      if (early) {
        view.toolResult({ agent_continuation: projection });
        await flush();
        assert.equal(view.calls("agent_continuation_bind").length, 0);
      }
      await view.initialize(outcome);
      if (!early) view.toolResult({ agent_continuation: projection });
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
  view.toolResult({ agent_continuation: projection });
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
