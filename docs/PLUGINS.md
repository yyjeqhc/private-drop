# Native Tool Plugins

[English](PLUGINS.md) | [简体中文](PLUGINS.zh-CN.md)

Native Tool Plugins let a WebCodex Runner expose tools implemented by an
arbitrary local executable. A Plugin can be Node.js, Bun, Deno, Python, `uv`,
Ruby, a Rust/Go/C/C++ binary, or another program that can read stdin and write
stdout. It does **not** need to implement an MCP server or depend on an MCP SDK.

The Plugin process runs on the Runner machine. The Server never receives its
command, argv, cwd, prepared environment, PID, stderr, or local credentials.

Native Plugins are **trusted local executables**. WebCodex does not sandbox,
contain, sign, or otherwise make an untrusted executable safe. A Plugin has the
same practical local-process trust implications as launching that executable
directly with the prepared Runner environment.

## Configure a Plugin

Plugins have their own `runner.toml` section; they are not MCP providers:

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

`id`, `name`, and `command` are required. `args`, `cwd`, `profile`, and
`timeout_secs` are optional. `cwd`, when present, is an absolute path.

Plugin execution deliberately reuses the Runner shell environment model:

1. `plugins.providers[].profile`, when set;
2. otherwise `shell.default_profile`;
3. otherwise the base Runner shell environment.

The selected shell profile prepares an environment snapshot, including
`shell.path_prepend`, `shell.env`, profile environment, and profile
`init_script`. The Plugin itself is then launched as a **native argv process**,
not through a shell. A bare command such as `node`, `python`, `uv`, or `bun` is
resolved from the prepared snapshot's `PATH`, not only from the Runner parent
process PATH. Sensitive WebCodex process credentials are filtered from that
environment.

Plugin candidate preparation may read the complete startup-bound `runner.toml`
because a provider can reference shell/profile inputs. The committed Plugin
state does not retain a second generic `ShellConfig` truth: it stores Plugin
provider config plus a derived Plugin-environment snapshot containing only the
base program/argv/dialect/PATH/env and referenced/default profile runtime,
environment, and init-script inputs. Unrelated persistent-shell controls do not
replace Plugin providers. Conversely, Plugin-relevant profile/environment
changes create a new provider instance on Plugin reload. `plugin:manage` still
cannot activate generic Runner shell configuration.

On Windows, native executables follow the Runner's normal `PATH`/`PATHEXT`
rules. `.cmd` and `.bat` commands are rejected for this ABI because they require
shell semantics; configure the native runtime executable instead.

## Runner-owned gateway model

Native Plugins are Runner-owned capabilities. Provider tools never join the
Server-global WebCodex tool namespace and are never appended to outer MCP
`tools/list`. A Plugin may define `safe_delete`, `runtime_status`, or any other
valid provider-local name without colliding with WebCodex built-ins or Plugin
tools on another Runner/provider.

The stable model-facing entry is the first-class WebCodex tool `plugin_tool`.
Its ToolSpec is static and registered in the same canonical tool metadata path
as other WebCodex tools; its schema does not depend on Runner availability or
Plugin inventory. `tool_manifest(tool_name="plugin_tool")` therefore describes
the exact gateway contract even when no Plugin-capable Runner is online.

The same canonical `plugin_tool` request parser and action-aware gateway executor
serve MCP and the generic Tool Runtime used by OpenAPI/GPT Actions. A surface
that advertises `plugin_tool` can therefore call it; MCP does not have a separate
Plugin implementation. For generic `callRuntimeTool`, use the canonical nested
`params` envelope for the complete Plugin contract because the outer `tool`
field already selects `plugin_tool` and the provider-local `tool` name belongs
inside Plugin arguments:

```json
{"tool":"plugin_tool","params":{"action":"describe","runner":"my-runner","plugin":"repo-tools","tool":"safe_delete"}}
{"tool":"plugin_tool","params":{"action":"call","binding":"wc_pbind_...","arguments":{"path":"build/old.bin"}}}
```

The static ToolDefinition is intentionally a worst-case discovery contract.
Execution policy is classified from the validated action before Session or
permission governance: list/describe require `plugin:inspect` and are read-only;
call requires `plugin:invoke` and uses local-execution governance; check/reload
require `plugin:manage` and use management governance. One shared specialized
executor owns the authoritative Workflow Session lifecycle for both MCP and API
transports, so one Plugin invocation records one lifecycle. A
`recording_session_id` is always explicit provenance and is never inferred from
transport, window, credential, Runner, or a previous call.

Routing always starts from the exact caller-visible Runner:

```text
plugin_tool(action="list")
    -> caller-visible Plugin-capable Runners
plugin_tool(action="list", runner="my-runner")
    -> current committed providers on that exact Runner
plugin_tool(action="list", runner="my-runner", plugin="repo-tools")
    -> bounded current tool names/titles for that exact provider
plugin_tool(action="check", runner="my-runner", plugin="repo-tools")
    -> disposable initialize + tools/list validation; no commit and no tools/call
plugin_tool(action="reload", runner="my-runner")
    -> reread runner.toml, admit candidates, atomically replace committed provider set
plugin_tool(action="describe", runner="my-runner", plugin="repo-tools", tool="search_symbol")
    -> { ..., "binding": "wc_pbind_..." }
plugin_tool(action="call", binding="wc_pbind_...", arguments={"query":"foo"})
```

The identity hierarchy is always:

```text
exact Runner instance
    -> exact provider instance
        -> provider-local tool + frozen schema observation
```

Same provider ids on different Runners, same tool names across Runners, and same
tool names in different providers on one Runner are normal. Only tool names
inside one provider catalog must be unique.

At Runner startup, configured providers are eagerly prepared, initialized, and
listed once to form the first committed provider set. Every successful provider
instance owns a frozen validated catalog for its lifetime. Ordinary
list/describe/call reads that frozen catalog and never asks the same instance to
re-list. Catalog/schema changes require a new provider instance through reload.

`list(runner, plugin)` observes only the currently committed provider instance.
It does not reread `runner.toml`, start a candidate, run check/reload, create a
binding, or call the Plugin tool. Full schemas remain exclusive to `describe`;
list returns bounded names/titles and safe health metadata only.

`check` is the recommended preflight before `reload`. The Runner rereads its
current `runner.toml`, locates only the requested provider, prepares the same
shell/profile environment used by the normal Plugin runtime, resolves and
**really starts** the configured executable, performs `initialize` and
`tools/list`, validates the normal Plugin protocol/bounds, then terminates that
candidate process tree. It never calls provider `tools/call` and never commits
the candidate into the current provider set. Because a Native Plugin is an arbitrary
local executable, its own startup/initialize/list behavior can still have
external side effects; `check` is not a purely static config linter.

A successful check returns `ready=true` plus bounded tool summaries containing
only names and optional titles. A broken candidate is normally a successful
check operation with `ready=false`, a structured `phase`, a stable error `code`,
and a bounded WebCodex-generated `detail`. Tool-definition validation failures
also include a small `diagnostic` with a finite WebCodex-defined code and, when
safe, a validated tool name and finite field label such as `inputSchema`.
Diagnostics come only from WebCodex protocol parsing/validators; raw serde
errors, protocol lines, schema fragments, Plugin stdout/stderr, executable
configuration, environment, and process identities are never copied into the
report. Plugin stderr remains Runner-local. The Runner keeps only a bounded,
control-sanitized local stderr ring for live providers and the most recent
disposable `check` candidate; this local projection is not part of the Plugin
gateway response.

`call` has one dispatch identity: the opaque `binding` returned by that exact
`describe`. It does not accept `runner`, `plugin`, or `tool` as call-time
routing fields. Each describe creates an independent binding for the exact
Runner instance, provider instance, tool name, and schema observation seen at
that moment. The handle does not encode or expose those internal identities.

`reload` is the cross-platform authoritative reload path on Windows, macOS, and
Linux. The Runner rereads its own `runner.toml`; the Server does not upload an
executable, environment, or raw Plugin config. Candidate management is
serialized across both `check` and `reload`: a second check returns
`plugin_check_busy`, while reload keeps the existing `plugin_reload_busy` result.
Candidate operations are rejected with `NotStarted` instead of being queued.
This gate does not block list/describe/call from using the currently committed
provider while a candidate is being prepared.

A reload prepares the complete candidate provider set before commit. If any
candidate admission fails, the previous committed set remains intact. On
success, the committed set is replaced atomically; removed providers disappear
immediately. Old provider instances are retired, so old bindings fail closed.
There is no fallback to a retired or removed provider instance.

This creates the normal development loop:

```text
edit Plugin code/config
    -> plugin_tool check
    -> fix the bounded diagnostic until the disposable candidate is ready
    -> plugin_tool reload
    -> plugin_tool list(runner, plugin)
    -> plugin_tool describe
    -> receive opaque binding
    -> plugin_tool call(binding, arguments)
    -> finish debugging
```

Running `check` before reload never replaces the current provider set and never
creates a binding. The successful check candidate is disposed instead of being
reused by a later reload.

`runner_config_reload` and `plugin_tool reload` share the same Plugin candidate
admission/commit primitive. Editing `[plugins]` does not require a Runner restart:
generic Runner config reload live-applies the Plugin candidate as part of the
same activation, while `plugin_tool reload` provides the narrower
`plugin:manage`-scoped operation. Plugin management authority never grants
authority to change unrelated Runner configuration.

`runner_config_check` remains a structural Runner-config check: it reads/parses
the startup-bound `runner.toml`, validates configuration bounds, and classifies
restart-only fields without starting disposable Plugin processes. Use
`plugin_tool check(runner, plugin)` when you need executable resolution plus the
Plugin `initialize -> tools/list` protocol/admission preflight.

## Plugin authoring/operator CLI

`webcodex plugin` now has two deliberately separate paths. `plugin init` is a
local-only scaffold generator and does not use Server authentication, a Runner,
`plugin_tool`, or `/api/tools/call`. The four inspection/management commands remain
operator-friendly adapters over the same canonical `plugin_tool` path; they do not
add a Plugin endpoint, Runtime, supervisor, or admission implementation. Each
network command issues one authenticated `POST /api/tools/call` with
`tool="plugin_tool"` and the corresponding canonical `params`:

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

Create a standalone authoring project locally with:

```text
webcodex plugin init ./my-plugin
webcodex plugin init ./MyPlugin --id my-plugin
```

The generated project contains `.gitignore`, `package.json`, `tsconfig.json`,
`src/plugin.ts`, and `README.md`. It pins the public npm package
`@yyjeqhc/webcodex-plugin-sdk` **exactly** to `0.1.0`; it does not use a `file:`,
workspace, Git, source-checkout, or floating version dependency. `plugin init`
does not install packages, execute generated code, edit `runner.toml`, register a
provider, reload a Runner, or create credentials. The destination must be absent or
an empty ordinary directory, and existing data is never overwritten.

After creating the scaffold, the human output also prints a copy-ready
`[[plugins.providers]]` block whose `args` points at the generated project's real
absolute `dist/plugin.js` path. The value is TOML-serialized rather than interpolated,
so spaces, Windows backslashes, quotes, and other TOML-sensitive characters are
escaped correctly. This is guidance only: the operator still copies the block into
the target Runner's startup-bound `runner.toml` on that Runner host. The portable
scaffold README keeps an absolute-path placeholder and never records the author's
machine path. If the active local config is unclear, inspect the Runner profile or
service selection on the Runner host, for example with
`webcodex runner status --profile <profile>`; the Server never discovers or returns
Runner-local config/executable paths for this workflow.

A practical author loop is therefore:

```text
webcodex plugin init ./my-plugin
    -> npm install
    -> npm run build
    -> copy the printed provider block into the Runner-local startup config
    -> webcodex plugin check --runner special --plugin my-plugin --token-file /path/to/plugin-authoring-pat
    -> webcodex plugin reload --runner special --token-file /path/to/plugin-authoring-pat
    -> webcodex plugin list --runner special --plugin my-plugin --token-file /path/to/plugin-authoring-pat
    -> webcodex plugin describe --runner special --plugin my-plugin --tool echo --token-file /path/to/plugin-authoring-pat
```

The identity and lifecycle rules are unchanged. Runner ids are exact; the CLI does
not fuzzy-match or infer one from a provider/project. `list` never checks/reloads a
provider, and `describe` consumes the canonical describe response without an extra
list. `check` still starts and disposes the Runner-owned candidate without commit.
`reload` still rereads that exact Runner's `runner.toml` and atomically replaces the
**complete** configured provider set; there is intentionally no `reload --plugin`.
Bindings returned by describe are printed as opaque observations and are never
cached or promoted into credentials. Bindings created before a successful reload
still fail closed when their provider instance is retired.

Network options follow the existing CLI Server conventions: `--server-url`,
`--proxy`, `--no-system-proxy`, `--env-file`, `--token-file`, `--token`, and
`--json`. For Plugin authoring, prefer a dedicated explicit token file such as
`--token-file /path/to/plugin-authoring-pat`. The shared user/API resolver keeps its
backward-compatible precedence: explicit `--token`; `--token-file`; selected
`--env-file` `WEBCODEX_TOKEN`, then `WEBCODEX_PAT`; process `WEBCODEX_TOKEN`, then
`WEBCODEX_PAT`. `WEBCODEX_PAT` is only an additive user/API CLI input alias; it does
not redefine the Server bootstrap meaning of `WEBCODEX_TOKEN`, and when both names
exist the legacy `WEBCODEX_TOKEN` input still wins. If a separate authoring PAT is
needed, use the existing `webcodex tokens create-local` / Server token-management
flow rather than a Plugin-specific credential path.

Runner transport tokens are rejected before the user/API HTTP request. `list` and
`describe` require `plugin:inspect`; canonical `plugin_tool` invocation requires
`plugin:invoke`; `check` and `reload` require an explicitly granted `plugin:manage`.
The existing `--oauth-local-plugins` setup option still grants only
`plugin:inspect + plugin:invoke` and **does not** grant management authority. A
401/403 is a normal CLI failure; the CLI never mints, upgrades, or mutates a
credential to make the request pass.

`--json` prints the canonical `plugin_tool` output object rather than a second CLI
Plugin schema. Human output renders only bounded canonical fields. A completed
`check` exits 0 only for `ready=true`; `ready=false` exits non-zero while retaining
its phase/code/detail/diagnostic. Reload exits 0 only when its canonical `failures`
array is empty; known rejection is non-zero.

The CLI never auto-retries `check` or `reload`. Once a request has begun, an HTTP
timeout, connection reset, malformed post-send response, or lost response cannot
prove the management operation did not reach the Server/Runner. The CLI therefore
reports that the outcome may be unknown and directs the operator to observe current
Plugin state before retrying. This is intentionally conservative even for `check`,
because arbitrary Plugin startup/initialize/list behavior may itself have side
effects.

There is deliberately no `webcodex plugin call` in this authoring phase. The raw
`plugin_tool describe -> call` contract remains the canonical invocation path with
its existing binding, effect, retry, and `OutcomeUnknown` semantics.

Phase 1 intentionally deferred `webcodex plugin init` until the SDK had a truthful
external dependency contract. That prerequisite is now satisfied:
`@yyjeqhc/webcodex-plugin-sdk@0.1.0` is publicly distributed through npm, and Phase 3
adds the local scaffold using that exact compatibility pin. A generated project
therefore works independently of a WebCodex source checkout. Repository first-party
dogfood such as `plugins/safe-delete` and [`plugins/repo-info`](../plugins/repo-info/README.md)
intentionally continues to use the local SDK source so it tests the checkout under
development; external projects created by `plugin init` use the published package.

## TypeScript Plugin SDK

`@yyjeqhc/webcodex-plugin-sdk` is an optional TypeScript authoring layer for Native
Tool Plugins. Version `0.1.0` is publicly distributed through npm; because the SDK
is still pre-1.0, generated projects use an exact compatibility pin rather than a
minor-compatible range:

```text
raw executable protocol
    -> @yyjeqhc/webcodex-plugin-sdk
    -> still webcodex-plugin-v1
    -> Runner-authoritative check / reload / call
```

The SDK supplies the small v1 schema builder, `defineTool`, `definePlugin`, result
helpers, and serial newline-delimited JSON-RPC stdio runtime so Plugin authors do
not need to hand-write framing and dispatch boilerplate. It is **not** an MCP SDK,
does not change Plugin authority, and does not sandbox the trusted executable.
The Server and Runner do not need Node because the SDK exists; only a Plugin that
chooses this SDK needs Node on its Runner machine. TypeScript is an authoring/build
dependency: production executes the compiled ESM JavaScript, with no TypeScript
runtime requirement.

SDK types are authoring assistance, not a second admission authority. The Rust
Runner still owns protocol/schema admission, frozen catalog semantics, bounds,
timeout/process lifecycle, output validation, and `OutcomeUnknown`. In particular,
`plugin_tool check` remains the authoritative admission check. An explicit SDK
`errorResult(...)` is a known completed application result; an unhandled handler
throw/rejection stops the provider without fabricating a ToolResult so the Runner
can retain send ambiguity for effectful calls.

See [`../npm/plugin-sdk/README.md`](../npm/plugin-sdk/README.md) and the TypeScript
[`echo-plugin.ts`](../npm/plugin-sdk/examples/echo-plugin.ts) example. The raw,
zero-dependency [`native-tool-plugin.mjs`](../examples/native-tool-plugin.mjs)
remains the protocol reference and does not depend on the SDK.

For the architectural boundary, development-stage compatibility policy, and staged
authoring roadmap, see [`architecture/native-tool-plugins.md`](architecture/native-tool-plugins.md).

## WebCodex Plugin Protocol v1

The native protocol is newline-delimited JSON-RPC 2.0 framing with the protocol
version `webcodex-plugin-v1`. Each request or response is exactly one JSON line.
It is a WebCodex Plugin protocol, not MCP.

Initialization:

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"webcodex-plugin-v1"}}
```

```json
{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"webcodex-plugin-v1"}}
```

Tool discovery:

```json
{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}
```

```json
{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"add","description":"Add two numbers","inputSchema":{"type":"object","properties":{"a":{"type":"number"},"b":{"type":"number"}},"required":["a","b"]}}]}}
```

Tool call:

```json
{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"add","arguments":{"a":2,"b":5}}}
```

```json
{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"7"}],"structuredContent":{"sum":7},"isError":false}}
```

Tool definitions support `name`, optional `title`, `description`,
`inputSchema`, optional `outputSchema`, and optional `annotations`. The first
version of tool results supports only text content, optional object
`structuredContent`, and `isError`. Images, resources, sampling, callbacks,
streaming, pagination, and arbitrary JSON-RPC tunneling are not part of v1.

Messages, schemas, arguments, JSON structure, tool counts, and results are
bounded. Malformed or unsupported data fails closed instead of being truncated
into a different tool contract.

### Native Plugin Schema Profile v1

`inputSchema` and `outputSchema` use a deliberately small WebCodex profile; they
are **not** advertised as full JSON Schema 2020-12. Every schema node requires a
single string `type`. Supported types are `object`, `array`, `string`, `number`,
`integer`, `boolean`, and `null`. Supported keywords are:

- all nodes: `type`, optional `title`, `description`, `enum`, `const`;
- object: `properties`, `required`, boolean `additionalProperties`;
- string: `minLength`, `maxLength`;
- array: `minItems`, `maxItems`, `items`.

Input/output schema roots must be `type: "object"`. Property counts, required
entries, enum size, schema bytes/depth/nodes/strings, and declared length/item
bounds are finite. Unknown keywords are rejected at provider admission rather
than silently ignored. In particular v1 does not support `$ref`/remote refs,
`$defs`, recursive refs, `pattern`, `format`, numeric `minimum`/`maximum`, schema
forms of `additionalProperties`, union `type`, `anyOf`, `oneOf`, `allOf`, `not`,
or arbitrary draft-specific keywords.

See [`examples/native-tool-plugin.mjs`](../examples/native-tool-plugin.mjs) for
a minimal no-dependency Node example. The repository also ships two first-party
SDK dogfood Plugins: [`plugins/safe-delete`](../plugins/safe-delete/README.md) is an
optional project-root-fenced Plugin that moves one file or directory to the operating
system Trash/Recycle Bin without adding permanent deletion to WebCodex's built-in
tool surface; [`plugins/repo-info`](../plugins/repo-info/README.md) is a read-only
authoring example whose single `git_summary` tool observes only the provider's
configured repository `cwd`.

## Calling and failure semantics

Each provider handles one request at a time. Concurrent calls receive a
provider-busy result instead of being silently queued without bound.

Before `tools/call` can enter the provider connection, WebCodex resolves the
exact frozen catalog entry, checks the caller's exact schema observation, and
validates `arguments` against that frozen input schema. Any failure here is
`NotStarted`; the executable sees zero `tools/call` bytes. No `tools/list` is
performed on this path. After a provider response arrives, ordinary text/result
bounds still apply. When an `outputSchema` exists, `structuredContent` is also
required to match that exact frozen output schema. A mismatch is a completed
provider contract violation and retires that provider instance fail-closed.

`plugin_tool call` requires an opaque binding from a preceding `describe`.
Bindings are bounded server-side observations, not bearer authorization tokens:
every call still requires current `plugin:invoke` authority and current access to
the logical Runner. A binding can also be evicted. If its Runner/provider
instance disappears, the tool is removed, or its schema changes, the stale call
fails `NotStarted` and must be described again. WebCodex never re-resolves the
binding to a newer same-named provider/tool, never manufactures a replacement
binding, and never replays the call. Internal Runner/provider instance ids and
schema revision machinery are not exposed in the handle.

After the local frozen-schema preflight succeeds, the provider timeout is one
total RPC budget. It starts before provider-request encoding and covers bounded
stdin queue admission, the complete frame write plus flush confirmation, and the
response wait. A Plugin that stops reading stdin therefore cannot make
`write_all` escape the provider timeout.
For effectful `tools/call`, once a frame may have started writing, a write
timeout/failure, connection loss, process death, or response timeout is
`OutcomeUnknown`; WebCodex does not automatically retry or replay it. Failures
proven to happen before any possible send are `NotStarted`.

Plugin stdout is protocol-only. Plugin stderr is drained continuously on a
separate worker so stderr flooding cannot backpressure stdout protocol progress.
The local ring retains at most 64 lines, 1 KiB per line, and 32 KiB aggregate;
control/non-UTF-8 bytes are projected safely and overlong lines are marked
truncated. Stderr is never treated as protocol, inserted into a model ToolResult
or Workflow Session ledger, or automatically copied into Runner registration.

## OAuth

Native Plugin authority is operation-specific:

- `plugin:inspect` allows metadata observation such as list and describe.
- `plugin:invoke` allows `plugin_tool call`.
- `plugin:manage` allows development/management operations that can start or
  change local Plugin processes, currently check and reload. It does not imply
  `plugin:invoke`.
- None of these scopes is part of the direct shared-key model baseline.
- For the shared-key OAuth bridge, opt in explicitly with
  `webcodex connect ... --auth oauth --oauth-local-plugins`; that opt-in grants
  only `plugin:inspect` + `plugin:invoke`, never `plugin:manage`.

`mcp:local` does not grant Plugin access, and Plugin scopes do not grant
Runner-owned MCP provider access. Effectful Plugin operations also pass the
same Workflow Session guard and authority-mode permission policy as other
consequential WebCodex execution when an explicit `recording_session_id` is
supplied; WebCodex never infers that Session from MCP transport identity.

## Troubleshooting

If a Plugin does not start, verify the configured profile, prepared `PATH`,
absolute `cwd`, runtime executable, and that stdout contains only protocol JSON
lines. Use stderr for local diagnostics.

Provider tools are intentionally absent from outer MCP `tools/list`; use
`plugin_tool list -> describe -> call`. If an old binding stops working after a
Runner/provider replacement or schema change, re-list and describe again. Never
blindly retry a call whose dispatch certainty is `outcome_unknown`.
