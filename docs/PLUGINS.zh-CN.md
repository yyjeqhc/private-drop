# Native Tool Plugins

[English](PLUGINS.md) | [简体中文](PLUGINS.zh-CN.md)

Native Tool Plugin 允许 WebCodex Runner 把任意本地可执行程序变成工具。Plugin
可以使用 Node.js、Bun、Deno、Python、`uv`、Ruby，也可以是 Rust、Go、C/C++
编译后的二进制。它**不需要**实现 MCP Server，也不需要依赖 MCP SDK。

Plugin 进程只运行在 Runner 所在机器。Server 不会收到它的 command path、argv、
cwd、prepared environment、PID、stderr 或本地凭据。

Native Plugin 是**受信任的本地 executable**。WebCodex 不会 sandbox、隔离、签名或把
不可信 executable 自动变安全；启动 Plugin 与在 prepared Runner environment 下直接运行
这个本地程序具有同等级别的本机信任含义。

## 配置 Plugin

Plugin 使用独立的 `runner.toml` 配置，不复用 MCP provider：

```toml
[shell]
default_profile = "node"

[shell.profiles.node]
program = "bash"
args = ["-lc"]
init_script = """
source ~/.nvm/nvm.sh
nvm use 22
"""

[plugins]
request_timeout_secs = 30

[[plugins.providers]]
id = "repo-tools"
name = "Repo Tools"
command = "node"
args = ["tools/plugin.mjs"]
cwd = "/root/git/example"
profile = "node"
timeout_secs = 30
```

`id`、`name`、`command` 必填；`args`、`cwd`、`profile`、`timeout_secs`
可选。配置 `cwd` 时必须使用绝对路径。

Plugin 不再发明一套 runtime/PATH/env 机制，而是真正复用 Runner 的 shell profile：

1. 优先使用 `plugins.providers[].profile`；
2. 否则使用 `shell.default_profile`；
3. 都没有时使用 Runner base shell environment。

选中的 profile 会先得到 prepared environment snapshot，包括
`shell.path_prepend`、`shell.env`、profile env 和 `init_script`。随后 Plugin
本身按 **native argv process** 启动，不再额外套一层 shell。`node`、`python`、
`uv`、`bun` 这样的 bare command 会从 prepared snapshot 的 `PATH` 解析，而不是
只看 Runner 父进程的 PATH。敏感 WebCodex 进程凭据会被过滤。

Plugin candidate preparation 可以读取完整的 startup-bound `runner.toml`，因为 provider
可能引用 shell/profile 输入；但 committed Plugin state 不再保存第二份 generic
`ShellConfig` truth。它只保存 Plugin provider config，以及派生出的 Plugin environment
snapshot：base program/argv/dialect/PATH/env，加上 default/referenced profile 真正需要的
runtime、env 和 init-script 输入。`shell.max_persistent_shells` 等无关 persistent-shell
控制不会替换 Plugin provider；Plugin 相关 profile/env 改动则会在 reload 时产生新的
provider instance。`plugin:manage` 因此仍然不能激活 generic Runner shell configuration。

Windows 继续使用 Runner 已有的 `PATH` / `PATHEXT` native executable 规则；
`.cmd` / `.bat` 需要 shell 语义，因此 Native Plugin ABI 会明确拒绝它们。

## Runner-owned gateway 模型

Native Plugin 是 Runner-owned capability。provider 的具体 Tool 永远不会加入
Server-global WebCodex tool namespace，也不会被追加到外层 MCP `tools/list`。Plugin
可以定义 `safe_delete`、`runtime_status` 或其他合法 provider-local 名称；它们不需要
和 WebCodex builtin、其他 Runner 或同一 Runner 的其他 provider 做全局避让。

唯一稳定的 model-facing 入口是一等 WebCodex 工具 `plugin_tool`。它的 ToolSpec 走和
其他 WebCodex 工具相同的 canonical metadata/registry 链路，schema 与 Runner 是否在线、
安装了哪些 Plugin 无关。因此即使当前没有 Plugin-capable Runner，
`tool_manifest(tool_name="plugin_tool")` 也能返回准确 gateway contract。

MCP 与 OpenAPI/GPT Actions 使用的 generic Tool Runtime 都复用同一个 canonical
`plugin_tool` parser 和 action-aware gateway executor；不存在 MCP Plugin 实现和 GPT
Plugin 实现两套逻辑。任何声明暴露 `plugin_tool` 的 canonical model surface 都可以实际
调用它。对 generic `callRuntimeTool`，完整 Plugin contract 使用 canonical nested
`params`：外层 `tool` 已经用于选择 `plugin_tool`，provider-local `tool` 必须留在 Plugin
业务参数里：

```json
{"tool":"plugin_tool","params":{"action":"describe","runner":"my-runner","plugin":"repo-tools","tool":"safe_delete"}}
{"tool":"plugin_tool","params":{"action":"call","binding":"wc_pbind_...","arguments":{"path":"build/old.bin"}}}
```

静态 ToolDefinition 只表达 worst-case discovery contract。真正执行 policy 会在校验后的
`action` 上先分类，再进入 Session/permission governance：list/describe 只要求
`plugin:inspect` 且是 read-only；call 要求 `plugin:invoke` 并走 local-execution governance；
check/reload 要求 `plugin:manage` 并走 management governance。MCP 与 API transport 共用一个
specialized executor，它也是 Workflow Session lifecycle 的唯一 owner，因此一次 Plugin
调用只产生一套 authoritative lifecycle。`recording_session_id` 必须显式提供；不会从
transport、window、credential、Runner 或之前的调用推断。

routing 永远从 caller-visible 的 exact Runner 开始：

```text
plugin_tool(action="list")
    -> 当前 caller 可见且支持 Plugin 的 Runners
plugin_tool(action="list", runner="my-runner")
    -> 该 exact Runner 当前 committed providers
plugin_tool(action="list", runner="my-runner", plugin="repo-tools")
    -> 该 exact provider 当前有界 tool name/title
plugin_tool(action="check", runner="my-runner", plugin="repo-tools")
    -> disposable initialize + tools/list 校验；不提交，也不执行 tools/call
plugin_tool(action="reload", runner="my-runner")
    -> reread runner.toml，admit candidates，原子替换 committed provider set
plugin_tool(action="describe", runner="my-runner", plugin="repo-tools", tool="search_symbol")
    -> { ..., "binding": "wc_pbind_..." }
plugin_tool(action="call", binding="wc_pbind_...", arguments={"query":"foo"})
```

identity 层级固定为：

```text
exact Runner instance
    -> exact provider instance
        -> provider-local tool + frozen schema observation
```

不同 Runner 使用同名 provider、不同 Runner 使用同名 Tool、同一 Runner 的不同 provider
使用同名 Tool 都是正常情况。只要求单个 provider 自己的 Tool name 唯一。

Runner 启动时会 eager prepare 配置中的 provider，执行 initialize + 一次 tools/list，形成
第一版 committed provider set。成功 provider instance 的 validated catalog 在其生命周期内
冻结；普通 list/describe/call 只读这份 frozen catalog，不会偷偷 re-list。schema/catalog
变化必须通过 reload 产生新的 provider instance。

`list(runner, plugin)` 只观察当前 committed provider instance；不会重新读取
`runner.toml`、启动 candidate、执行 check/reload、创建 binding 或调用 Plugin Tool。完整
schema 仍只由 `describe` 返回；list 只返回 bounded name/title 与安全 health metadata。

`check` 是开发时 `reload` 之前推荐使用的预检。Runner 会重新读取当前
`runner.toml`，只定位 requested provider，使用正常 Plugin runtime 的同一套
shell/profile environment preparation 和 native executable resolution，**真实启动**配置的
executable，完成 `initialize`、`tools/list` 与普通 Plugin protocol/bounds validation，随后
终止整个 disposable candidate process tree。WebCodex 在 check 中绝不会调用 provider
`tools/call`，也不会把 candidate 提交到当前 provider set。由于 Native Plugin 本身就是任意
本地 executable，其 startup/initialize/list 自身仍可能产生外部副作用，因此 `check` 不是
纯静态配置 lint。

成功预检返回 `ready=true`，tool summary 只包含 name 和可选 title。坏 candidate 通常仍然
表示“check 操作成功完成了诊断”，因此返回 `ready=false`、结构化 `phase`、稳定 `code`
以及有界、由 WebCodex 生成的安全 `detail`，而不是把所有坏 Plugin 都变成
`plugin_tool isError=true`。Tool definition validation failure 还会带一个很小的
`diagnostic`：使用有限、稳定的 WebCodex-defined code，并在安全时携带 validated tool name
以及 `inputSchema` 等有限 field label。diagnostic 只来自 WebCodex protocol parser/validator；
不会透传 raw serde error、protocol line、schema fragment、Plugin stdout/stderr、executable
配置、environment 或 process identity。Plugin stderr 始终留在 Runner 本机。Runner 只为
live provider 与最近一次 disposable `check` candidate 保留 bounded、control-sanitized 的
本机 stderr ring；这个 local projection 不属于 Plugin gateway response。

`call` 的 dispatch identity 只有这个 opaque `binding`；call 阶段不再接受
`runner`、`plugin`、`tool` 作为路由字段。每次 describe 都会创建独立 binding，精确
记录当时观察到的 Runner instance、provider instance、tool name 和 schema；handle
本身不会编码或暴露这些内部 identity。

`reload` 是 Windows、macOS、Linux 都可用的 authoritative reload 入口。Runner
自己重新读取自己的 `runner.toml`；Server 不会上传 executable、env 或 raw Plugin
config。candidate management 在 `check` 与 `reload` 之间共同串行：第二个 check 返回
`plugin_check_busy`，reload 保持已有 `plugin_reload_busy`；都以 `NotStarted` 明确拒绝，
不排队等待。candidate 准备期间不会长期持有 current committed provider session，因此
list/describe/call 仍可继续使用当前已提交 provider。

reload 会先准备完整 candidate provider set。任意 candidate admission 失败时，旧 committed
set 原样保留；全部成功后才原子替换。被删除的 provider 会立即消失，旧 provider instance
会 retire，因此旧 binding fail closed。不会 fallback 到任何已 retire 或已删除的 provider instance。

标准开发闭环是：

```text
修改 Plugin code/config
    -> plugin_tool check
    -> 按 bounded diagnostic 修复直到 disposable candidate ready
    -> plugin_tool reload
    -> plugin_tool list(runner, plugin)
    -> plugin_tool describe
    -> 收到 opaque binding
    -> plugin_tool call(binding, arguments)
    -> 调试完成
```

reload 前运行 `check` 不会替换当前 committed provider set，也不会创建 binding。成功的
check candidate 会被销毁，不会偷偷复用成下一次 reload provider。

`runner_config_reload` 与 `plugin_tool reload` 共用同一个 Plugin candidate
admission/commit primitive。修改 `[plugins]` 不需要为了 Plugin 生效而重启 Runner：generic
Runner config reload 会在同一次 activation 中 live apply Plugin candidate；`plugin_tool
reload` 则提供更窄、只需要 `plugin:manage` 的专门入口。Plugin management authority 不会
因此获得修改其他 Runner config 的权限。

`runner_config_check` 仍然只是 Runner config 的结构性检查：读取/解析 startup-bound
`runner.toml`、验证配置边界并分类 restart-only 字段，不会启动 disposable Plugin process。
需要检查 executable resolution 以及 Plugin `initialize -> tools/list` protocol/admission 时，
使用 `plugin_tool check(runner, plugin)`。

## Plugin authoring/operator CLI

`webcodex plugin` 现在明确分成两条路径：`plugin init` 是纯本地 scaffold generator，不使用
Server auth、Runner、`plugin_tool` 或 `/api/tools/call`；另外四个 inspect/manage 命令仍是
同一条 canonical `plugin_tool` 链路的 operator-friendly adapter，不新增 Plugin endpoint、
Runtime、supervisor 或 admission implementation。每个网络命令都只向现有
`POST /api/tools/call` 发一次 authenticated request，外层固定为 `tool="plugin_tool"`，
`params` 严格映射已有 action：

```text
webcodex plugin list
    -> {"action":"list"}
webcodex plugin list --runner special
    -> {"action":"list","runner":"special"}
webcodex plugin list --runner special --plugin safe-delete
    -> {"action":"list","runner":"special","plugin":"safe-delete"}
webcodex plugin describe --runner special --plugin safe-delete --tool safe_delete
    -> {"action":"describe","runner":"special","plugin":"safe-delete","tool":"safe_delete"}
webcodex plugin check --runner special --plugin safe-delete
    -> {"action":"check","runner":"special","plugin":"safe-delete"}
webcodex plugin reload --runner special
    -> {"action":"reload","runner":"special"}
```

本地创建一个可独立使用的 authoring project：

```text
webcodex plugin init ./my-plugin
webcodex plugin init ./MyPlugin --id my-plugin
```

生成目录只包含 `.gitignore`、`package.json`、`tsconfig.json`、`src/plugin.ts`、`README.md`。
其中 `@yyjeqhc/webcodex-plugin-sdk` **精确固定**为公开 npm 版本 `0.1.0`，不会生成
`file:`、workspace、Git、源码 checkout 或 floating version dependency。`plugin init` 不会
安装依赖、执行生成代码、修改 `runner.toml`、注册 provider、reload Runner 或创建 credential。
目标目录只能不存在或是空的 ordinary directory；任何已有用户数据都不会被覆盖。

实际 author loop 可以直接写成：

```text
编辑/构建 Plugin
    -> webcodex plugin check --runner special --plugin safe-delete
    -> webcodex plugin reload --runner special
    -> webcodex plugin list --runner special --plugin safe-delete
    -> webcodex plugin describe --runner special --plugin safe-delete --tool safe_delete
```

identity/lifecycle 语义没有变化。Runner id 必须精确提供，CLI 不做 fuzzy match，也不会从
provider/project 推断 Runner。`list` 不会偷偷 check/reload；`describe` 直接消费 canonical
describe response，不额外 list。`check` 仍由 Runner 启动并销毁 disposable candidate，且不
commit。`reload` 仍由 exact Runner 重新读取自己的 `runner.toml`，准备**完整 provider set**，
只有全部 admission 成功才原子替换；因此刻意不存在 `reload --plugin`。describe 返回的
binding 只作为 opaque observation 输出，不缓存、不提升为 credential；reload retire provider
后，旧 binding 仍按原 contract fail closed。

网络参数沿用现有 CLI Server conventions：`--server-url`、`--proxy`、
`--no-system-proxy`、`--env-file`、`--token-file`、`--token`、`--json`。Bearer token
优先级依次为显式 `--token`、token file、指定 env file 中的 `WEBCODEX_TOKEN`、当前进程
`WEBCODEX_TOKEN`；Runner transport token 会在 user/API 路径前被拒绝。`list`/`describe`
要求 `plugin:inspect`；`check`/`reload` 必须由用户显式提供具有 `plugin:manage` 的 credential。
现有 `--oauth-local-plugins` 仍然只表示 `plugin:inspect + plugin:invoke`，**不会**授予 manage。
401/403 会正常 non-zero 失败；CLI 不会为了通过请求自动 mint、升级或修改 credential。

`--json` 直接打印 canonical `plugin_tool` output object，不再设计第二套 CLI Plugin JSON
模型；human output 只渲染有界 canonical fields。`check` 只有在 `ready=true` 时 exit 0；
`ready=false` non-zero，但仍保留 phase/code/detail/diagnostic。reload 只有 canonical
`failures` 为空时 exit 0；known rejection non-zero。

CLI 不会自动 retry `check` 或 `reload`。请求开始后如果遇到 HTTP timeout、connection reset、
post-send malformed response 或 response lost，CLI 无法证明 management operation 没有到达
Server/Runner，因此会保守报告 outcome may be unknown，并要求先观察当前 Plugin state 再决定
是否 retry。`check` 也遵循这一点，因为 arbitrary Plugin startup/initialize/list 本身仍可能
有外部副作用。

本阶段刻意没有 `webcodex plugin call`。raw `plugin_tool describe -> call` 仍是 canonical
invocation path，并继续拥有原有 binding、effect、retry 和 `OutcomeUnknown` 语义。

Phase 1 曾有意暂缓 `webcodex plugin init`，直到 SDK 建立真实 external dependency contract。
这个前置条件现在已经满足：`@yyjeqhc/webcodex-plugin-sdk@0.1.0` 已通过 npm 公开分发，Phase 3
因此加入使用该**精确兼容版本**的本地 scaffold。生成项目不依赖 WebCodex 源码 checkout。
仓库内 first-party dogfood（例如 `plugins/safe-delete` 和
[`plugins/repo-info`](../plugins/repo-info/README.zh-CN.md)）仍有意使用同 checkout 的 local
SDK source，以持续测试正在开发的 SDK；外部 `plugin init` 项目则使用 published package。

## TypeScript Plugin SDK

`@yyjeqhc/webcodex-plugin-sdk` 是 Native Tool Plugin 的可选 TypeScript authoring layer。
`0.1.0` 已通过 npm 公开分发；由于 SDK 仍处于 pre-1.0，`plugin init` 生成项目使用 exact
compatibility pin，而不是允许自动跨 minor 的版本范围：

```text
raw executable protocol
    -> @yyjeqhc/webcodex-plugin-sdk
    -> 仍然是 webcodex-plugin-v1
    -> Runner-authoritative check / reload / call
```

SDK 提供小型 v1 schema builder、`defineTool`、`definePlugin`、result helper，以及严格
串行的 newline-delimited JSON-RPC stdio runtime，让 Plugin 作者不用重复手写 framing 和
dispatch 样板代码。它**不是** MCP SDK，不会改变 Plugin authority，也不会 sandbox 受信任
executable。WebCodex Server / Runner 不会因为 SDK 而要求 Node；只有主动选择这个 SDK 的
Plugin 自己需要 Runner 机器提供 Node。TypeScript 只用于 authoring/build，生产环境执行编译
后的 ESM JavaScript，不要求 TypeScript runtime。

SDK 类型只是 authoring assistance，并不是第二套 admission authority。Rust Runner 继续权威
拥有 protocol/schema admission、frozen catalog、bounds、timeout/process lifecycle、output
validation 与 `OutcomeUnknown`。`plugin_tool check` 仍然是 authoritative admission check。
其中显式 `errorResult(...)` 表示确定的 completed application result；未处理的 handler
throw/rejection 会终止 provider，并且不会伪造 ToolResult，因此 effectful call 的 send
ambiguity 仍可由 Runner 正确保留。

完整 SDK 说明见 [`../npm/plugin-sdk/README.zh-CN.md`](../npm/plugin-sdk/README.zh-CN.md)，
TypeScript 示例见 [`echo-plugin.ts`](../npm/plugin-sdk/examples/echo-plugin.ts)。原有无 SDK
依赖的 raw [`native-tool-plugin.mjs`](../examples/native-tool-plugin.mjs) 继续作为 protocol
reference 保留。

Plugin 的架构边界、开发阶段兼容策略和后续 authoring roadmap 见
[`architecture/native-tool-plugins.md`](architecture/native-tool-plugins.md)。

## WebCodex Plugin Protocol v1

Native protocol 使用 newline-delimited JSON-RPC 2.0 framing，protocol version 是
`webcodex-plugin-v1`。每个 request/response 各占一行 JSON。它是 WebCodex Plugin
Protocol，不是 MCP。

初始化：

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"webcodex-plugin-v1"}}
```

```json
{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"webcodex-plugin-v1"}}
```

发现工具：

```json
{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}
```

调用工具：

```json
{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"add","arguments":{"a":2,"b":5}}}
```

```json
{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"7"}],"structuredContent":{"sum":7},"isError":false}}
```

Tool definition 支持 `name`、可选 `title`、`description`、`inputSchema`、可选
`outputSchema` 和 `annotations`。v1 result 只支持 text content、可选 object
`structuredContent` 和 `isError`。暂不支持 image/resource、sampling、callback、
streaming、pagination 或 arbitrary JSON-RPC tunneling。

message、schema、arguments、JSON depth/node/string、tool count 和 result 都有明确边界；
非法或超限数据 fail closed，不会通过截断把它变成另一份 Tool contract。

### Native Plugin Schema Profile v1

`inputSchema` / `outputSchema` 使用 WebCodex 明确定义的小型 profile，**不宣称**支持完整
JSON Schema 2020-12。每个 schema node 都必须有单一字符串 `type`，支持：`object`、
`array`、`string`、`number`、`integer`、`boolean`、`null`。支持的 keyword 只有：

- 所有 node：`type`，以及可选 `title`、`description`、`enum`、`const`；
- object：`properties`、`required`、boolean `additionalProperties`；
- string：`minLength`、`maxLength`；
- array：`minItems`、`maxItems`、`items`。

input/output schema root 必须是 `type: "object"`。property 数量、required 数量、enum
长度、schema bytes/depth/nodes/string 以及声明的 length/item bounds 都有有限上限。未知
keyword 会在 provider admission 时明确拒绝，不会 silently ignore。v1 特别**不支持**
`$ref` / remote ref、`$defs`、recursive ref、`pattern`、`format`、numeric
`minimum` / `maximum`、schema 形式的 `additionalProperties`、union `type`、`anyOf`、
`oneOf`、`allOf`、`not` 或任意 draft-specific keyword。

最小无依赖 Node 示例见
[`examples/native-tool-plugin.mjs`](../examples/native-tool-plugin.mjs)。仓库还提供两个
first-party SDK dogfood Plugin：[`plugins/safe-delete`](../plugins/safe-delete/README.zh-CN.md)
把删除权限限制在配置的项目根内，只把单个文件或目录移入系统 Trash / Recycle Bin，
不会把永久删除能力加入 WebCodex 内建工具面；
[`plugins/repo-info`](../plugins/repo-info/README.zh-CN.md) 是只读 authoring 示例，它唯一的
`git_summary` 只观察 provider 配置的 repository `cwd`。

## 调用与失败语义

每个 provider 同一时间只处理一个 request。并发请求会得到 provider-busy，而不是无限
排队。

`tools/call` 进入 provider connection 之前，WebCodex 会先从 exact frozen catalog
解析 Tool、核对 caller 的 exact schema observation，再使用 frozen input schema 校验
`arguments`。这里任何失败都是 `NotStarted`，provider executable 会看到 **0 个**
`tools/call` byte；call path 也不会执行 `tools/list`。provider response 返回后仍执行已有
text/result bounds；如果 Tool 声明了 `outputSchema`，则 `structuredContent` 必须存在并
匹配 exact frozen output schema。违约属于已完成 dispatch 后的 provider contract failure，
WebCodex 会 fail closed 并 retire 该 provider instance。

`plugin_tool call` 必须使用前一次 `describe` 返回的 opaque binding。binding 是
Server 端有界保存的一次 exact observation，不是 bearer authorization token：每次 call
仍然重新要求当前 credential 具有 `plugin:invoke`，并且当前 caller 仍有权访问对应 logical
Runner。binding 也可能因为容量上限被 eviction。Runner/provider instance 被替换、tool 被
删除或 schema 改变时，旧 binding 以 `NotStarted` fail closed，必须重新 describe；
WebCodex 不会把它 re-resolve 到新的同名 provider/tool，不会自动生成新 binding，也不会
replay。内部 Runner/provider instance id 和 schema revision 不会编码到 handle 里或暴露给
模型。

local frozen-schema preflight 成功后，provider timeout 才作为一次 RPC 的总预算开始：
它从 provider request encode 前开始，同时覆盖 bounded stdin queue admission、完整
frame 的 write + flush confirmation，以及 response wait。因此 Plugin 即使停止读取 stdin，
`write_all` 也不能逃逸 provider timeout。对于
effectful `tools/call`，只要 frame 已经可能开始写入，write timeout/failure、connection
loss、process death 或 response timeout 都返回 `OutcomeUnknown`，不会自动 retry/replay；
只有能证明任何 byte 都不可能发送的失败才是 `NotStarted`。

stdout 只用于 protocol。stderr 由独立 worker 持续 drain，因此 stderr flood 不会反向
阻塞 stdout protocol。local ring 最多保留 64 行、每行 1 KiB、aggregate 32 KiB；control /
非 UTF-8 byte 会做安全 projection，超长行会标记 truncated。stderr 不会被当作 protocol，
不会进入 model ToolResult 或 Workflow Session ledger，也不会自动复制进 Runner registration。

## OAuth

Native Plugin authority 按 operation 拆分：

- `plugin:inspect` 允许 list、describe 等纯 metadata observation；
- `plugin:invoke` 允许 `plugin_tool call`；
- `plugin:manage` 允许会启动或改变本地 Plugin process 的开发/管理操作，目前是 check 和
  reload；它本身不会隐式授予 `plugin:invoke`；
- 以上 scope 都不属于 direct shared-key model baseline；
- 使用 shared-key OAuth bridge 时，需要显式
  `webcodex connect ... --auth oauth --oauth-local-plugins`；该 opt-in 只授予
  `plugin:inspect` + `plugin:invoke`，绝不会授予 `plugin:manage`。

`mcp:local` 不授予 Plugin 权限；Plugin scopes 也不授予 Runner-owned MCP provider 权限。
effectful Plugin operation 如果携带显式 `recording_session_id`，还必须通过与其他
consequential WebCodex execution 相同的 Workflow Session guard 和 authority-mode
permission policy；WebCodex 不会从 MCP transport identity 推断 Workflow Session。

## 排障

Plugin 无法启动时，优先检查 profile、prepared `PATH`、绝对 `cwd`、runtime executable，
以及 stdout 是否只包含 protocol JSON line；本地诊断写 stderr。

provider Tool **本来就不会**直接出现在外层 MCP `tools/list`；使用
`plugin_tool list -> describe -> call`。Runner/provider replacement 或 schema 变化后旧
binding 失效时，重新 list + describe。只要 dispatch certainty 是 `outcome_unknown`，就不要
盲目重试。
