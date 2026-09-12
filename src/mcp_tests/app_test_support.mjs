import { readFileSync } from "node:fs";
import { runInNewContext } from "node:vm";

export const flush = () => new Promise(resolve => setImmediate(resolve));

// Execute the shipped App script with deterministic Host messages and timers.
export function app(filename) {
  const html = readFileSync(new URL(`../${filename}`, import.meta.url), "utf8");
  const script = html.match(/<script>([\s\S]*?)<\/script>/)[1];
  const nodes = {};
  const listeners = new Map();
  const timers = new Map();
  const sent = [];
  let nextTimer = 1;
  const parent = { postMessage(message) { sent.push(message); } };
  const document = {
    hidden: false,
    getElementById: id => nodes[id] ||= { textContent: "", hidden: false },
  };
  function addEventListener(name, listener) {
    if (!listeners.has(name)) listeners.set(name, []);
    listeners.get(name).push(listener);
  }
  function emit(name, event) {
    for (const listener of listeners.get(name) || []) listener(event);
  }
  function setTimer(callback, delay, interval = false) {
    const id = nextTimer++;
    timers.set(id, { callback, delay, interval });
    return id;
  }
  runInNewContext(script, {
    document, parent, addEventListener, TextEncoder,
    setTimeout: setTimer,
    clearTimeout: id => timers.delete(id),
    setInterval: (callback, delay) => setTimer(callback, delay, true),
    clearInterval: id => timers.delete(id),
  });
  function deliver(message, source = parent) {
    emit("message", { source, data: { jsonrpc: "2.0", ...message } });
  }
  return {
    nodes, timers, sent,
    calls(name) { return sent.filter(message => message.method === "tools/call" && message.params.name === name); },
    notification(method, params, source) {
      deliver({ method, params }, source);
    },
    toolInput(args, source) {
      deliver({ method: "ui/notifications/tool-input", params: { arguments: args } }, source);
    },
    toolResult(output, source) {
      deliver({ method: "ui/notifications/tool-result", params: toolResult(output) }, source);
    },
    async reply(request, result) {
      deliver({ id: request.id, result });
      await flush();
    },
    async initialize(outcome = "success") {
      if (outcome === "timeout") await this.fireTimers(10000);
      else {
        deliver({ id: sent[0].id, ...(outcome === "success"
          ? { result: { protocolVersion: "2026-01-26" } }
          : { error: { message: "Host initialization rejected" } }) });
        await flush();
      }
    },
    async fireTimers(delay) {
      for (const [id, timer] of [...timers]) {
        if (timer.delay !== delay) continue;
        if (!timer.interval) timers.delete(id);
        timer.callback();
      }
      await flush();
    },
    async visibility(hidden) {
      document.hidden = hidden;
      emit("visibilitychange", {});
      await flush();
    },
    async teardown(method = "ui/resource-teardown") {
      if (method === "ui/resource-teardown") deliver({ method, id: "host-teardown" });
      else emit(method, {});
      await flush();
    },
  };
}

export function toolResult(output, privateMeta) {
  return {
    structuredContent: { success: true, output },
    ...(privateMeta ? { _meta: { "webcodex/agentContinuation": privateMeta } } : {}),
  };
}
