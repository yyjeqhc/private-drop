# @yyjeqhc/webcodex-plugin-sdk

TypeScript authoring helpers for WebCodex Native Tool Plugins. The SDK removes JSON-RPC, stdio framing, and dispatch boilerplate while keeping the existing `webcodex-plugin-v1` protocol unchanged.

The Rust Runner remains authoritative for Plugin admission, schema validation, authority, timeout, process lifecycle, frozen catalog identity, response bounds, and `OutcomeUnknown` semantics. TypeScript types and SDK helpers improve authoring; `plugin_tool check` remains the authoritative WebCodex Plugin admission check.

## Requirements

- Node.js 18 or newer to run a Plugin built with this SDK.
- TypeScript is an authoring/build dependency only. Production Plugin execution uses compiled ESM JavaScript and does not require a TypeScript runtime or compiler.
- The SDK has zero runtime package dependencies and uses Node built-ins only.

Using this SDK does not make a Plugin an MCP server, grant new WebCodex permissions, or sandbox the executable. Native Plugins are trusted local processes. WebCodex Server and Runner themselves do not require Node because of this SDK; only a Plugin that chooses this SDK needs a Node runtime on its Runner machine.

## Stability

The SDK is experimental while it is pre-1.0. Minor `0.x` releases may contain breaking authoring API changes; pin an exact version when a Plugin needs a stable build input. Native Plugin protocol versioning and SDK package versioning remain separate concerns.

## Example

```ts
import {
  definePlugin,
  defineTool,
  runPlugin,
  schema,
  textResult,
} from "@yyjeqhc/webcodex-plugin-sdk";

const echo = defineTool({
  name: "echo",
  description: "Echo one string",
  inputSchema: schema.object({
    text: schema.string({ minLength: 1, maxLength: 4096 }),
    uppercase: schema.optional(schema.boolean()),
  }),
  outputSchema: schema.object({
    text: schema.string({ maxLength: 4096 }),
  }),
  async execute({ text, uppercase }) {
    const value = uppercase ? text.toUpperCase() : text;
    return textResult(value, { text: value });
  },
});

runPlugin(definePlugin({ tools: [echo] }));
```

Compile authoring source with TypeScript, then configure the Runner to execute the generated JavaScript:

```text
plugin.ts
   -> tsc/build
 dist/plugin.js
   -> node dist/plugin.js
```

`stdout` is protocol-only. Plugin diagnostics belong on `stderr`; ordinary application logging must not use `console.log` or otherwise write to stdout.

## Schema builder

`schema` intentionally models only the Native Plugin Schema Profile v1, not general JSON Schema. It provides:

- `schema.object`, `schema.array`, `schema.string`, `schema.number`, `schema.integer`, `schema.boolean`, and `schema.null`;
- `schema.optional` for object properties;
- `title`, `description`, `enum`, and `const` where applicable;
- string `minLength` / `maxLength` and array `minItems` / `maxItems`;
- boolean `additionalProperties`, defaulting to `false` for `schema.object`.

Object properties not wrapped with `schema.optional(...)` are emitted in `required`. Input and output roots accepted by `defineTool` are object schemas. Enum and const literals are preserved in ordinary TypeScript inference where practical.

The builder deliberately does not implement `$ref`, `$defs`, recursive references, `pattern`, `format`, numeric ranges, schema-valued `additionalProperties`, union types, `anyOf`, `oneOf`, `allOf`, `not`, or draft-specific keywords. It also does not copy WebCodex's byte, depth, node-count, or catalog admission limits. Run `plugin_tool check` against the configured provider for the authoritative result.

## Results and failure semantics

`textResult(text, structuredContent?)` returns a normal result with `isError: false`. `errorResult(text, structuredContent?)` returns a **known application result** with `isError: true`. Both emit v1 text content only and use the protocol fields `structuredContent` and `isError`.

A handler throw or rejected Promise has deliberately different semantics. The SDK does **not** turn it into `errorResult`. It emits only a generic diagnostic to stderr, stops the provider runtime, and sends no fabricated ToolResult for that request. If the Runner may already have sent an effectful `tools/call`, the existing Runner lifecycle can therefore preserve `OutcomeUnknown` instead of misreporting a known application failure. Plugin authors should return `errorResult(...)` only when the failure is known and safe to represent as a completed application result.

The SDK does not truncate or rewrite results to fit Runner limits. When an `outputSchema` is declared, its TypeScript generic constrains normal authoring, but the Runner remains the authoritative runtime validator for `structuredContent`.

## Runtime model

`runPlugin(plugin)` serves newline-delimited JSON-RPC 2.0 over process stdin/stdout. Requests are handled strictly one at a time; async handlers are awaited before the next input line is dispatched. Only `initialize`, `tools/list`, and `tools/call` are supported. Notifications and arbitrary methods fail closed.

`definePlugin` rejects duplicate provider-local tool names before serving and freezes the authoring catalog. `tools/list` exposes only protocol definitions; executable handlers and local closures never enter the wire representation. Declaration order is preserved by the SDK; the WebCodex Runner independently admits and freezes its authoritative catalog.

The published package includes [`examples/echo-plugin.ts`](examples/echo-plugin.ts) as a minimal SDK authoring example. For the raw protocol without any SDK dependency, see [`examples/native-tool-plugin.mjs`](https://github.com/yyjeqhc/webcodex/blob/main/examples/native-tool-plugin.mjs) in the WebCodex repository.
