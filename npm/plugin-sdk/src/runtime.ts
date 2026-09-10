import { once } from "node:events";
import process from "node:process";
import readline from "node:readline";
import type { Readable, Writable } from "node:stream";

import {
  PLUGIN_PROTOCOL_VERSION,
  isRecord,
  parseRequestLine,
  rpcError,
  rpcResult,
  type JsonRpcResponse,
} from "./protocol.js";
import {
  getPluginDefinitions,
  getPluginExecutor,
  type Plugin,
  type ToolResult,
} from "./types.js";

const HANDLER_FAILURE_DIAGNOSTIC = "webcodex plugin handler failed; provider will stop\n";
const RUNTIME_FAILURE_DIAGNOSTIC = "webcodex plugin runtime failed; provider will stop\n";

export interface PluginStreams {
  readonly input: Readable;
  readonly output: Writable;
  readonly error: Writable;
}

class PluginHandlerFailure extends Error {
  constructor() {
    super("plugin handler failed");
    this.name = "PluginHandlerFailure";
  }
}

async function writeResponse(output: Writable, response: JsonRpcResponse): Promise<void> {
  const line = `${JSON.stringify(response)}\n`;
  if (!output.write(line)) {
    await once(output, "drain");
  }
}

function writeDiagnostic(error: Writable, message: string): void {
  try {
    error.write(message);
  } catch {
    // Diagnostics are best-effort. Never redirect them onto protocol stdout.
  }
}

async function handleCall(
  plugin: Plugin,
  id: string | number,
  params: unknown,
  streams: Pick<PluginStreams, "output" | "error">,
): Promise<void> {
  if (!isRecord(params) || typeof params.name !== "string" || !isRecord(params.arguments)) {
    await writeResponse(streams.output, rpcError(id, -32602, "invalid params"));
    return;
  }

  const execute = getPluginExecutor(plugin, params.name);
  if (execute === undefined) {
    await writeResponse(streams.output, rpcError(id, -32602, "unknown tool"));
    return;
  }

  let result: ToolResult<object>;
  try {
    result = await execute(params.arguments);
  } catch {
    writeDiagnostic(streams.error, HANDLER_FAILURE_DIAGNOSTIC);
    throw new PluginHandlerFailure();
  }
  await writeResponse(streams.output, rpcResult(id, result));
}

async function handleOne(
  plugin: Plugin,
  line: string,
  streams: Pick<PluginStreams, "output" | "error">,
): Promise<void> {
  const parsed = parseRequestLine(line);
  if (!parsed.ok) {
    await writeResponse(streams.output, parsed.response);
    return;
  }

  const { id, method, params } = parsed.request;
  if (method === "initialize") {
    if (!isRecord(params) || params.protocolVersion !== PLUGIN_PROTOCOL_VERSION) {
      await writeResponse(streams.output, rpcError(id, -32602, "unsupported protocol version"));
      return;
    }
    await writeResponse(
      streams.output,
      rpcResult(id, { protocolVersion: PLUGIN_PROTOCOL_VERSION }),
    );
    return;
  }

  if (method === "tools/list") {
    if (params !== undefined && !isRecord(params)) {
      await writeResponse(streams.output, rpcError(id, -32602, "invalid params"));
      return;
    }
    await writeResponse(streams.output, rpcResult(id, { tools: getPluginDefinitions(plugin) }));
    return;
  }

  if (method === "tools/call") {
    await handleCall(plugin, id, params, streams);
    return;
  }

  await writeResponse(streams.output, rpcError(id, -32601, "method not found"));
}

export async function servePlugin(plugin: Plugin, streams: PluginStreams): Promise<void> {
  const lines = readline.createInterface({
    input: streams.input,
    crlfDelay: Infinity,
    terminal: false,
  });
  try {
    for await (const line of lines) {
      await handleOne(plugin, line, streams);
    }
  } finally {
    lines.close();
  }
}

export function runPlugin(plugin: Plugin): void {
  void servePlugin(plugin, {
    input: process.stdin,
    output: process.stdout,
    error: process.stderr,
  }).catch((error: unknown) => {
    if (!(error instanceof PluginHandlerFailure)) {
      writeDiagnostic(process.stderr, RUNTIME_FAILURE_DIAGNOSTIC);
    }
    process.exitCode = 1;
    process.stdin.destroy();
  });
}
