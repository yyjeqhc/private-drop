export const PLUGIN_PROTOCOL_VERSION = "webcodex-plugin-v1" as const;

export type JsonRpcId = string | number;

export interface JsonRpcRequest {
  readonly jsonrpc: "2.0";
  readonly id: JsonRpcId;
  readonly method: string;
  readonly params?: unknown;
}

export interface JsonRpcErrorObject {
  readonly code: number;
  readonly message: string;
}

export type JsonRpcResponse =
  | {
      readonly jsonrpc: "2.0";
      readonly id: JsonRpcId | null;
      readonly result: unknown;
    }
  | {
      readonly jsonrpc: "2.0";
      readonly id: JsonRpcId | null;
      readonly error: JsonRpcErrorObject;
    };

export type ParsedRequest =
  | { readonly ok: true; readonly request: JsonRpcRequest }
  | { readonly ok: false; readonly response: JsonRpcResponse };

export function rpcResult(id: JsonRpcId, result: unknown): JsonRpcResponse {
  return { jsonrpc: "2.0", id, result };
}

export function rpcError(
  id: JsonRpcId | null,
  code: number,
  message: string,
): JsonRpcResponse {
  return { jsonrpc: "2.0", id, error: { code, message } };
}

export function parseRequestLine(line: string): ParsedRequest {
  let value: unknown;
  try {
    value = JSON.parse(line) as unknown;
  } catch {
    return { ok: false, response: rpcError(null, -32700, "parse error") };
  }

  if (!isRecord(value) || value.jsonrpc !== "2.0" || !isJsonRpcId(value.id)) {
    return { ok: false, response: rpcError(null, -32600, "invalid request") };
  }
  if (typeof value.method !== "string" || value.method.length === 0) {
    return { ok: false, response: rpcError(value.id, -32600, "invalid request") };
  }
  return {
    ok: true,
    request: {
      jsonrpc: "2.0",
      id: value.id,
      method: value.method,
      ...(Object.prototype.hasOwnProperty.call(value, "params") ? { params: value.params } : {}),
    },
  };
}

export function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function isJsonRpcId(value: unknown): value is JsonRpcId {
  return typeof value === "string" || (typeof value === "number" && Number.isFinite(value));
}
