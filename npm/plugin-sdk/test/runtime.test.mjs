import assert from "node:assert/strict";
import { PassThrough, Readable } from "node:stream";
import test from "node:test";

import {
  definePlugin,
  defineTool,
  errorResult,
  schema,
  textResult,
} from "../dist/index.js";
import { servePlugin } from "../dist/runtime.js";

function deferred() {
  let resolve;
  let reject;
  const promise = new Promise((resolvePromise, rejectPromise) => {
    resolve = resolvePromise;
    reject = rejectPromise;
  });
  return { promise, resolve, reject };
}

function captureStream() {
  const stream = new PassThrough();
  let text = "";
  stream.setEncoding("utf8");
  stream.on("data", (chunk) => {
    text += chunk;
  });
  return { stream, read: () => text };
}

function callLine(id, name, args) {
  return `${JSON.stringify({ jsonrpc: "2.0", id, method: "tools/call", params: { name, arguments: args } })}\n`;
}

async function serveLines(plugin, lines) {
  const output = captureStream();
  const error = captureStream();
  await servePlugin(plugin, {
    input: Readable.from(lines),
    output: output.stream,
    error: error.stream,
  });
  return {
    responses: output.read().trimEnd().split("\n").filter(Boolean).map((line) => JSON.parse(line)),
    stdout: output.read(),
    stderr: error.read(),
  };
}

test("tools/call supports synchronous and asynchronous handlers", async () => {
  const sync = defineTool({
    name: "sync",
    inputSchema: schema.object({ text: schema.string() }),
    outputSchema: schema.object({ text: schema.string() }),
    execute({ text }) {
      return textResult(text, { text });
    },
  });
  const asyncTool = defineTool({
    name: "async",
    inputSchema: schema.object({ value: schema.integer() }),
    outputSchema: schema.object({ doubled: schema.integer() }),
    async execute({ value }) {
      await Promise.resolve();
      return textResult(String(value * 2), { doubled: value * 2 });
    },
  });
  const result = await serveLines(definePlugin({ tools: [sync, asyncTool] }), [
    callLine(1, "sync", { text: "hello" }),
    callLine(2, "async", { value: 4 }),
  ]);
  assert.deepEqual(result.responses, [
    {
      jsonrpc: "2.0",
      id: 1,
      result: {
        content: [{ type: "text", text: "hello" }],
        structuredContent: { text: "hello" },
        isError: false,
      },
    },
    {
      jsonrpc: "2.0",
      id: 2,
      result: {
        content: [{ type: "text", text: "8" }],
        structuredContent: { doubled: 8 },
        isError: false,
      },
    },
  ]);
  assert.equal(result.stderr, "");
});

test("async calls are strictly serialized without timing assumptions", async () => {
  const firstStarted = deferred();
  const releaseFirst = deferred();
  const order = [];
  const tool = defineTool({
    name: "serial",
    inputSchema: schema.object({ sequence: schema.integer() }),
    async execute({ sequence }) {
      order.push(`start-${sequence}`);
      if (sequence === 1) {
        firstStarted.resolve();
        await releaseFirst.promise;
      }
      order.push(`end-${sequence}`);
      return textResult(String(sequence), { sequence });
    },
  });

  const output = captureStream();
  const error = captureStream();
  const serving = servePlugin(definePlugin({ tools: [tool] }), {
    input: Readable.from([
      callLine(1, "serial", { sequence: 1 }),
      callLine(2, "serial", { sequence: 2 }),
    ]),
    output: output.stream,
    error: error.stream,
  });

  await firstStarted.promise;
  assert.deepEqual(order, ["start-1"]);
  releaseFirst.resolve();
  await serving;
  assert.deepEqual(order, ["start-1", "end-1", "start-2", "end-2"]);
  assert.equal(output.read().trimEnd().split("\n").length, 2);
});

test("explicit errorResult is a normal completed application result", async () => {
  const tool = defineTool({
    name: "known_failure",
    inputSchema: schema.object({}),
    outputSchema: schema.object({ code: schema.string() }),
    execute() {
      return errorResult("known application failure", { code: "rejected" });
    },
  });
  const result = await serveLines(definePlugin({ tools: [tool] }), [
    callLine(9, "known_failure", {}),
  ]);
  assert.equal(result.responses.length, 1);
  assert.equal(result.responses[0].result.isError, true);
  assert.deepEqual(result.responses[0].result.structuredContent, { code: "rejected" });
  assert.deepEqual(result.responses[0].result.content, [
    { type: "text", text: "known application failure" },
  ]);
});

test("handler rejection returns no ToolResult and emits only a bounded generic diagnostic", async () => {
  let secondRan = false;
  const tool = defineTool({
    name: "uncertain",
    inputSchema: schema.object({ secretArgument: schema.string() }),
    async execute({ secretArgument }) {
      if (secretArgument === "second") secondRan = true;
      throw new Error(`secret-exception-body:${secretArgument}`);
    },
  });
  const output = captureStream();
  const error = captureStream();
  await assert.rejects(
    servePlugin(definePlugin({ tools: [tool] }), {
      input: Readable.from([
        callLine(1, "uncertain", { secretArgument: "secret-argument" }),
        callLine(2, "uncertain", { secretArgument: "second" }),
      ]),
      output: output.stream,
      error: error.stream,
    }),
    /plugin handler failed/,
  );
  assert.equal(output.read(), "");
  assert.equal(secondRan, false);
  assert.equal(error.read(), "webcodex plugin handler failed; provider will stop\n");
  assert.equal(error.read().includes("secret-argument"), false);
  assert.equal(error.read().includes("secret-exception-body"), false);
  assert.equal(error.read().includes(" at "), false);
});

test("result helpers use only text content and camelCase v1 result fields", () => {
  const ok = textResult("ok", { value: 1 });
  const error = errorResult("bad", { value: 2 });
  assert.deepEqual(Object.keys(ok), ["content", "structuredContent", "isError"]);
  assert.deepEqual(Object.keys(error), ["content", "structuredContent", "isError"]);
  assert.deepEqual(ok.content, [{ type: "text", text: "ok" }]);
  assert.deepEqual(error.content, [{ type: "text", text: "bad" }]);
  assert.equal(ok.isError, false);
  assert.equal(error.isError, true);
});
