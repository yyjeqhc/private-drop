import assert from "node:assert/strict";
import { PassThrough, Readable } from "node:stream";
import test from "node:test";

import { definePlugin, defineTool, schema, textResult } from "../dist/index.js";
import { servePlugin } from "../dist/runtime.js";

function captureStream() {
  const stream = new PassThrough();
  let text = "";
  stream.setEncoding("utf8");
  stream.on("data", (chunk) => {
    text += chunk;
  });
  return { stream, read: () => text };
}

async function exchange(lines, plugin = definePlugin({ tools: [] })) {
  const output = captureStream();
  const error = captureStream();
  await servePlugin(plugin, {
    input: Readable.from(lines.map((line) => `${line}\n`)),
    output: output.stream,
    error: error.stream,
  });
  return {
    lines: output.read().trimEnd().split("\n").filter(Boolean).map((line) => JSON.parse(line)),
    raw: output.read(),
    stderr: error.read(),
  };
}

test("initialize requires the exact webcodex-plugin-v1 version", async () => {
  const ok = await exchange([
    JSON.stringify({ jsonrpc: "2.0", id: 1, method: "initialize", params: { protocolVersion: "webcodex-plugin-v1" } }),
  ]);
  assert.deepEqual(ok.lines, [
    { jsonrpc: "2.0", id: 1, result: { protocolVersion: "webcodex-plugin-v1" } },
  ]);

  const wrong = await exchange([
    JSON.stringify({ jsonrpc: "2.0", id: 2, method: "initialize", params: { protocolVersion: "webcodex-plugin-v2" } }),
  ]);
  assert.deepEqual(wrong.lines, [
    { jsonrpc: "2.0", id: 2, error: { code: -32602, message: "unsupported protocol version" } },
  ]);
});

test("tools/list exposes only protocol definitions and preserves authoring order", async () => {
  const second = defineTool({
    name: "second",
    inputSchema: schema.object({ value: schema.string() }),
    execute({ value }) {
      return textResult(value);
    },
  });
  const first = defineTool({
    name: "first",
    title: "First",
    description: "First tool",
    inputSchema: schema.object({}),
    annotations: { readOnlyHint: true },
    execute() {
      return textResult("first");
    },
  });
  const result = await exchange(
    [JSON.stringify({ jsonrpc: "2.0", id: 3, method: "tools/list", params: {} })],
    definePlugin({ tools: [second, first] }),
  );
  const tools = result.lines[0].result.tools;
  assert.deepEqual(tools.map((tool) => tool.name), ["second", "first"]);
  assert.equal("execute" in tools[0], false);
  assert.equal(JSON.stringify(tools).includes("function"), false);
  assert.deepEqual(Object.keys(tools[1]).sort(), [
    "annotations",
    "description",
    "inputSchema",
    "name",
    "title",
  ]);
});

test("definePlugin snapshots its catalog independently of the caller tools array", async () => {
  const first = defineTool({
    name: "first",
    inputSchema: schema.object({}),
    execute() {
      return textResult("first");
    },
  });
  const second = defineTool({
    name: "second",
    inputSchema: schema.object({}),
    execute() {
      return textResult("second");
    },
  });
  const tools = [first];
  const plugin = definePlugin({ tools });
  tools.push(second);
  const result = await exchange(
    [JSON.stringify({ jsonrpc: "2.0", id: 4, method: "tools/list", params: {} })],
    plugin,
  );
  assert.deepEqual(result.lines[0].result.tools.map((tool) => tool.name), ["first"]);
  assert.equal(JSON.stringify(first), "{}");
});

test("duplicate tool names fail before serving", () => {
  const make = () => defineTool({
    name: "same",
    inputSchema: schema.object({}),
    execute() {
      return textResult("ok");
    },
  });
  assert.throws(() => definePlugin({ tools: [make(), make()] }), /duplicate plugin tool name/);
});

test("unknown methods and malformed JSON fail closed without ToolResult", async () => {
  const result = await exchange([
    JSON.stringify({ jsonrpc: "2.0", id: "x", method: "other/method", params: {} }),
    "{broken-json",
    JSON.stringify({ jsonrpc: "1.0", id: 5, method: "tools/list", params: {} }),
  ]);
  assert.equal(result.lines.length, 3);
  assert.deepEqual(result.lines[0], {
    jsonrpc: "2.0",
    id: "x",
    error: { code: -32601, message: "method not found" },
  });
  assert.equal(result.lines[1].error.code, -32700);
  assert.equal(result.lines[2].error.code, -32600);
  for (const response of result.lines) assert.equal("result" in response, false);
});

test("stdout framing is exactly one JSON response per line", async () => {
  const result = await exchange([
    JSON.stringify({ jsonrpc: "2.0", id: 1, method: "tools/list", params: {} }),
    JSON.stringify({ jsonrpc: "2.0", id: 2, method: "tools/list", params: {} }),
  ]);
  assert.equal(result.raw.endsWith("\n"), true);
  assert.equal(result.raw.split("\n").filter(Boolean).length, 2);
  for (const line of result.raw.trimEnd().split("\n")) assert.doesNotThrow(() => JSON.parse(line));
  assert.equal(result.stderr, "");
});
