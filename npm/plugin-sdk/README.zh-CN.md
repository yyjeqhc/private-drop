# @yyjeqhc/webcodex-plugin-sdk

WebCodex Native Tool Plugin 的 TypeScript authoring SDK。它只负责消除 JSON-RPC、stdio framing 与 dispatch 样板代码，底层协议仍然是现有的 `webcodex-plugin-v1`。

Rust Runner 继续权威拥有 Plugin admission、schema validation、authority、timeout、process lifecycle、frozen catalog identity、response bounds 与 `OutcomeUnknown` 语义。TypeScript 类型和 SDK helper 只改善作者体验；`plugin_tool check` 仍然是 WebCodex Plugin 的 authoritative admission check。

## 运行要求

- 使用本 SDK 构建的 Plugin 在生产运行时需要 Node.js 18 或更高版本。
- TypeScript 只属于 authoring/build dependency。生产 Plugin 执行编译后的 ESM JavaScript，不要求 TypeScript runtime 或 compiler。
- SDK runtime package dependency 为 0，只使用 Node built-ins。

使用这个 SDK 不会把 Plugin 变成 MCP Server，不会增加 WebCodex 权限，也不会 sandbox executable。Native Plugin 仍然是受信任的本地进程。WebCodex Server / Runner 不会因为 SDK 而要求 Node；只有选择这个 SDK 的 Plugin 自己需要 Runner 机器提供 Node runtime。

## 稳定性

SDK 在 1.0 之前仍属于实验阶段。`0.x` 的 minor release 可能包含破坏性的 authoring API 调整；如果 Plugin 需要稳定的构建输入，应固定到精确版本。Native Plugin protocol version 与 SDK package version 仍然是两个独立概念。

## 示例

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

正常发布/部署 Plugin 时先编译 TypeScript，再让 Runner 执行生成的 JavaScript：

```text
plugin.ts
   -> tsc/build
 dist/plugin.js
   -> node dist/plugin.js
```

`stdout` 只能用于 protocol。Plugin diagnostics 应写入 `stderr`；普通应用日志不能通过 `console.log` 或其他方式污染 stdout。

## Schema builder

`schema` 只建模 Native Plugin Schema Profile v1，并不是通用 JSON Schema framework。它提供：

- `schema.object`、`schema.array`、`schema.string`、`schema.number`、`schema.integer`、`schema.boolean`、`schema.null`；
- object property 使用 `schema.optional` 表示非 required；
- profile 允许的 `title`、`description`、`enum`、`const`；
- string 的 `minLength` / `maxLength` 与 array 的 `minItems` / `maxItems`；
- boolean `additionalProperties`，`schema.object` 默认是 `false`。

没有包裹 `schema.optional(...)` 的 property 会进入 `required`。`defineTool` 的 input/output root 在 TypeScript API 上必须是 object schema。普通场景下 enum / const 会尽可能保留 literal inference。

Builder 明确不支持 `$ref`、`$defs`、recursive refs、`pattern`、`format`、numeric range、schema-valued `additionalProperties`、union type、`anyOf`、`oneOf`、`allOf`、`not` 或 draft-specific keyword。它也不会复制 WebCodex 的 byte/depth/node-count/catalog admission 上限。请对真实配置 provider 运行 `plugin_tool check`，以 Runner 的结果为准。

## Result 与失败语义

`textResult(text, structuredContent?)` 表示正常完成结果，`isError: false`；`errorResult(text, structuredContent?)` 表示**确定的应用层完成结果**，`isError: true`。两者都只产生 v1 text content，并使用 wire 字段 `structuredContent` / `isError`。

handler throw 或 Promise rejection 的语义刻意不同。SDK **不会**自动将异常转换成 `errorResult`。它只向 stderr 输出 generic diagnostic，停止 provider runtime，并且不会为当前 request 伪造 ToolResult。这样在 Runner 已经可能发送 effectful `tools/call` 的情况下，Runner 现有 lifecycle 可以继续得到 `OutcomeUnknown`，而不是把不确定副作用误报成已知 application failure。只有当 Plugin 作者明确知道失败已经确定且可安全表示时，才应显式返回 `errorResult(...)`。

SDK 不会为了通过 Runner bounds 而截断或改写结果。声明 `outputSchema` 时 TypeScript generic 会约束常规 authoring，但 `structuredContent` 的 runtime validation 仍由 Runner 权威执行。

## Runtime 模型

`runPlugin(plugin)` 在 process stdin/stdout 上提供 newline-delimited JSON-RPC 2.0。请求严格串行：async handler 完成以前不会 dispatch 下一行。只支持 `initialize`、`tools/list`、`tools/call`；notification 与 arbitrary method fail closed。

`definePlugin` 会在 serve 前拒绝 provider-local duplicate tool name，并冻结 authoring catalog。`tools/list` 只暴露 protocol definition；execute handler、function source、closure/local state 不会进入 wire。SDK 保留声明顺序，WebCodex Runner 会独立 admission 并冻结自己的 authoritative catalog。

发布后的 package 会包含 [`examples/echo-plugin.ts`](examples/echo-plugin.ts) 作为最小 TypeScript SDK authoring 示例。完全不依赖 SDK 的 raw protocol reference 仍然位于 WebCodex 仓库的 [`examples/native-tool-plugin.mjs`](https://github.com/yyjeqhc/webcodex/blob/main/examples/native-tool-plugin.mjs)。
