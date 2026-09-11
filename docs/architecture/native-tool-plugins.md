# Native Tool Plugin architecture and development plan

Status: active development design note. The current runtime and operator contract
remains [`../PLUGINS.md`](../PLUGINS.md). This document records the architectural
boundary and staged development direction so authoring ergonomics can improve
without moving execution authority out of the Runner.

## Purpose

Native Tool Plugins let a Runner expose trusted local executable capabilities
without turning every provider tool into a Server-global MCP tool. The design
should make a useful local Plugin inexpensive to author and iterate while keeping
one authoritative execution path for admission, routing, lifecycle, bounds, and
uncertain effects.

The intended stack is:

```text
Plugin domain code
    -> optional TypeScript authoring SDK
    -> webcodex-plugin-v1 over newline-delimited JSON-RPC stdio
    -> Runner-owned provider process and frozen catalog
    -> plugin_tool exact Runner/provider/tool binding
    -> existing WebCodex permission, audit, and model surfaces
```

The TypeScript SDK is an authoring layer, not a second runtime authority. Native
Plugins also remain language-neutral: a provider can continue to implement the
same protocol directly in Python, Rust, Go, JavaScript, or another executable.

## Current foundation

The following pieces are already implemented and should be treated as the current
baseline rather than redesigned by the next authoring work:

- `webcodex-plugin-v1` defines `initialize`, `tools/list`, and `tools/call` over
  bounded newline-delimited JSON-RPC 2.0.
- The Runner resolves the configured native executable from its prepared local
  environment, starts and owns the provider process tree, and keeps command,
  argv, cwd, environment, PID, stderr, and local credentials off the Server.
- Startup and reload create a validated frozen provider catalog. Ordinary
  list/describe/call operations do not re-list a live provider instance.
- `plugin_tool check` starts a disposable candidate and performs the same
  executable resolution, initialize, catalog parsing, schema validation, and
  bounds checks used by normal admission, without committing the candidate.
- `plugin_tool reload` prepares the complete candidate provider set before an
  atomic replacement. A failed candidate leaves the previous committed set
  intact, and retired bindings fail closed.
- `plugin_tool describe` returns an opaque binding for one exact Runner instance,
  provider instance, provider-local tool, and schema observation. `call` accepts
  that binding plus arguments and never retargets implicitly.
- Rust remains authoritative for schema/profile admission, catalog and payload
  bounds, timeout/process lifecycle, output validation, and uncertain effect
  handling.
- `@yyjeqhc/webcodex-plugin-sdk` provides a small TypeScript schema builder,
  `defineTool`, `definePlugin`, result helpers, and serial stdio protocol runtime.
  It intentionally does not duplicate the Runner's admission policy.
- `plugins/safe-delete` is the first effectful first-party SDK dogfood Plugin. It
  keeps destructive filesystem authority in its own domain implementation while
  using the SDK only for authoring/protocol boilerplate.
- `plugins/repo-info` is the first read-only authoring-loop dogfood Plugin. Its
  single `git_summary` tool accepts no path and observes only the provider-configured
  `cwd`, keeping repository selection in Runner configuration rather than Plugin input.

This produces one important ownership rule:

> Authoring helpers may make the protocol easier to use, but they must not become
> a second source of truth for whether a Plugin is admitted or what happened to
> an effectful invocation.

## Authority and lifecycle boundary

The boundary between SDK/Plugin code and the Runner should remain explicit.

### Plugin and SDK own

- provider-local domain behavior;
- declared tool names, descriptions, schemas, and annotations;
- application-level success and known application failures;
- authoring-time TypeScript types and schema construction;
- protocol framing/dispatch convenience when the TypeScript SDK is used.

An explicit SDK `errorResult(...)` is a known completed application result. It is
appropriate only when the Plugin can truthfully say the application outcome is
known.

### Runner owns

- configured executable/profile/cwd/environment preparation;
- process creation, process-tree ownership, shutdown, and timeout;
- protocol and schema admission;
- frozen catalog identity and reload commit;
- model/operator permission checks around `plugin_tool`;
- exact Runner/provider/tool binding identity;
- payload and result bounds;
- output-schema validation;
- transport/provider death after send and resulting `OutcomeUnknown` semantics.

An unhandled effectful handler exception must therefore not be normalized by the
SDK into a fabricated `isError=true` ToolResult. If the provider dies after the
request may have been accepted, the Runner must retain uncertainty instead of
inventing a retry-safe failure.

## Development-stage compatibility policy

The Plugin SDK and first-party Plugin authoring layout are still under active
development. Until a package, entrypoint, or authoring workflow is explicitly
published as stable, source-layout compatibility is not a design goal.

In particular, development work should not keep compatibility shims solely for:

- superseded JavaScript/TypeScript source layouts;
- old generated artifact paths;
- temporary first-party Plugin entrypoints;
- unpublished SDK helper names;
- hypothetical external consumers that do not yet exist.

When a development representation is replaced, remove the obsolete source,
configuration, documentation, and dedicated tests together when practical.

This does **not** permit weakening real boundaries. Compatibility or stability
must be preserved where there is a concrete contract, especially:

- the currently declared `webcodex-plugin-v1` wire behavior;
- model-facing tool schemas and effect annotations when callers rely on them;
- destructive-action authority and path fencing;
- retry/uncertainty semantics;
- published package/version contracts once publication begins;
- immutable release artifacts.

When the project later declares an SDK/CLI/package surface stable, that decision
should add an explicit compatibility policy at that boundary rather than carrying
pre-stability migration code indefinitely.

## Phase 1: Plugin authoring/operator CLI

The first CLI phase reduces the manual author loop without creating another Plugin
runtime. Its canonical public surface is intentionally limited to:

```text
webcodex plugin list [--runner <runner> [--plugin <provider>]]
webcodex plugin describe --runner <runner> --plugin <provider> --tool <tool>
webcodex plugin check --runner <runner> --plugin <provider>
webcodex plugin reload --runner <runner>
```

The normal author loop is:

```text
edit/build Plugin
    -> webcodex plugin check --runner special --plugin safe-delete
    -> fix bounded Runner admission diagnostics until ready
    -> webcodex plugin reload --runner special
    -> webcodex plugin list --runner special --plugin safe-delete
    -> webcodex plugin describe --runner special --plugin safe-delete --tool safe_delete
```

`list` and `describe` mirror the existing `plugin_tool` inspection semantics and
require `plugin:inspect`. `check` and `reload` mirror the existing management
semantics and require `plugin:manage`. The CLI does not widen credentials;
`--oauth-local-plugins` continues to mean only `plugin:inspect + plugin:invoke` and
never supplies `plugin:manage`.

### Thin adapter boundary

Every network command goes through the existing authenticated Server runtime path:

```text
webcodex plugin ...
    -> POST /api/tools/call
    -> {"tool":"plugin_tool","params":{...canonical action arguments...}}
    -> existing Server permission/audit gateway
    -> exact caller-selected Runner
```

The CLI reuses the existing Server HTTP client, proxy behavior, bearer-token
resolution, and token redaction. It does not connect directly to Runner transport,
read `runner.toml` to resolve executables, spawn Plugin processes, validate Plugin
schemas, inspect provider stderr, create bindings itself, or implement provider
lifecycle. `describe` consumes the one canonical describe result rather than
performing an extra list, and bindings are displayed only as opaque observations;
they are not cached or treated as authorization credentials.

`reload` remains an exact-Runner **complete provider-set** operation. There is no
per-provider reload flag: the Runner rereads its own `runner.toml`, prepares every
candidate, and atomically replaces the committed set only if all candidates are
admitted. `check` likewise remains Runner-owned disposable admission and never
commits its candidate.

The CLI performs exactly one Server request per command and adds no hidden retry.
This matters especially for `check` and `reload`: after an HTTP timeout, connection
reset, malformed post-send response, or lost response, the CLI cannot prove the
request was not processed. It therefore reports that the outcome may be unknown
and instructs the operator to observe current Plugin state before retrying rather
than inventing retry authority.

Machine output (`--json`) is the canonical `plugin_tool` output object with no
parallel Plugin domain model. Human output is a bounded rendering of the same safe
fields. A completed `check` exits successfully only when `ready=true`; a known
`ready=false` result is non-zero. Reload succeeds only when the canonical
`failures` array is empty. HTTP/auth/runtime failures remain non-zero without
flattening canonical failure codes or exposing credentials.

There is intentionally no `webcodex plugin call` in this phase. Effectful
invocation, binding/retry behavior, and `OutcomeUnknown` remain on the normal
model/operator `plugin_tool describe -> call` path.

### Why Phase 1 deferred `plugin init`

A public `webcodex plugin init` was deliberately **not** part of Phase 1. At that
time `@yyjeqhc/webcodex-plugin-sdk` was only a repository development package used
by first-party dogfood through a local `file:` dependency; it had no established
external npm publication/versioning contract. Generating a project that appeared
standalone while depending on a source checkout or build-machine path would have
been misleading.

Phase 1 therefore refused to paper over that boundary with embedded SDK source,
per-project vendoring, an npm workspace, automatic pack/install, temporary
`--sdk-path`, or generated source-checkout paths. Phase 3 begins only after the SDK
has a truthful repeatable external dependency source. The local scaffold still must
not install packages implicitly, edit Runner config, register/reload a provider,
create credentials, or execute generated code.

A raw protocol Plugin remains a fully supported peer of an SDK-authored Plugin.

### Why this phase matters

Today the runtime loop is already safe but mechanically expensive: authors edit a
Plugin, build it, remember the exact management calls, copy Runner/provider ids,
and manually inspect the committed catalog. A thin CLI can collapse that operator
friction while leaving every consequential step on the same existing runtime
path.

The expected effect is:

```text
less JSON/tool-call ceremony
+ fewer config/identity mistakes
+ easier SDK examples and first-party dogfood
+ scriptable admission/reload checks
- no new execution authority
- no second Plugin runtime
```

This phase addresses the highest-value remaining authoring cost after the SDK
proved it can carry a real effectful Plugin: repeated operator ceremony around the
same authoritative runtime path.

## Follow-up phases

### Phase 2: dogfood the authoring CLI

Use `safe-delete` and one small read-only Plugin to exercise the new CLI end to
end. The goal is to validate command shape, diagnostic quality, exact Runner
selection, build/reload iteration, and failure behavior before expanding the
surface.

Do not add a generic `dev` daemon or file watcher until repeated manual dogfood
shows a concrete need. A future `dev` convenience command should only compose
existing build/check/reload primitives; it must not invent a separate hot-reload
runtime.

### Phase 3: SDK distribution contract and `plugin init` — implemented

The distribution gate is now satisfied: `@yyjeqhc/webcodex-plugin-sdk@0.1.0` is
publicly distributed through npm. `webcodex plugin init <DIRECTORY> [--id PROVIDER_ID]`
therefore creates a deterministic local TypeScript/ESM project that depends on that
published package with an **exact `0.1.0` compatibility pin** and does not require a
WebCodex source checkout. The scaffold compatibility version is intentionally a CLI
choice; it is not required to equal either the WebCodex product version or every
newer SDK version that may subsequently exist.

`plugin init` is a local-only action, structurally separate from the network
`PluginCommand::{List, Describe, Check, Reload}` path. It performs no Server/token
resolution, `/api/tools/call`, Runner request, package-manager execution, generated
code execution, or Runner configuration edit. It derives the provider id from the
destination basename only when that basename already satisfies the canonical
`validate_provider_id`; otherwise the author must provide `--id`. No second slugging
or normalization contract exists.

Repository first-party dogfood (`safe-delete` and `repo-info`) consumes the local SDK
source, while external projects generated by `plugin init` consume the exact public
npm package. `repo-info` was itself bootstrapped through the real Phase 3 scaffold
before its repository-local dependency was switched to that local SDK source. This
preserves deterministic repository CI without making normal validation depend on npm
registry availability.

Do not make WebCodex product releases depend on SDK version equality. Native Plugin
protocol versioning, SDK package versioning, and the scaffold compatibility pin are
distinct concerns.

### Phase 4: additional Plugin capabilities only from concrete demand

Potential future protocol work such as richer result types, streaming, resources,
or additional lifecycle hooks should start from a demonstrated Plugin use case.
Those changes require explicit protocol and Runner admission design; they should
not be added as SDK-only fields that the Runner does not understand.

A higher-level TypeScript control/extension runtime, if later needed for Skills,
Memory, orchestration, or integrations, is also a separate architectural layer.
It may consume canonical WebCodex primitives, but Native Tool Plugins should not
silently evolve into that runtime.

## Explicit non-goals for the authoring workflow

The authoring CLI should not introduce:

- Plugin marketplace or registry;
- automatic Plugin download or installation;
- automatic execution of project-local Plugin code when a repository is opened;
- background file watching or implicit reload;
- a second schema validator or provider supervisor;
- Server-side storage of local Plugin executable paths or environments;
- arbitrary provider retargeting at call time;
- a new Plugin protocol version;
- embedded Node/Bun/Deno inside the Runner;
- Plugin sandbox claims;
- automatic npm/pnpm/yarn dependency installation;
- compatibility shims for unpublished development layouts.

## Acceptance criteria for the Phase 1 authoring CLI

Phase 1 is complete when all of the following are true:

1. An operator can list visible Plugin Runners, inspect committed providers/tools,
   and describe one exact tool without manually constructing `plugin_tool` JSON.
2. Authoritative check and reload target one exact caller-provided Runner through
   the existing authenticated Server `/api/tools/call` path.
3. Failed admission preserves the bounded Runner diagnostic and does not replace
   the currently committed provider set; reload remains whole-set atomic.
4. A successful reload produces the same frozen catalog and binding behavior as
   direct `plugin_tool` use, and describe does not secretly re-list the provider.
5. CLI credentials do not gain Plugin management scope implicitly; inspect and
   manage remain distinct.
6. Check/reload transport ambiguity never becomes automatic retry authority.
7. JSON output preserves canonical Plugin gateway fields while human output is
   bounded and never projects raw provider stderr or credentials.
8. No CLI/SDK code becomes a second provider supervisor, schema authority, or
   effect-lifecycle owner, and no `plugin call` surface is introduced.
9. `safe-delete` can use the workflow without reintroducing legacy entrypoint or
   source compatibility shims.
10. Default CI remains deterministic and path-aware; authoring-only changes do not
    trigger unrelated Desktop/native/release lanes.
11. `plugin init` remains absent until the SDK has a truthful external distribution
    contract. Phase 3 satisfied this historical gate with the public npm `0.1.0`
    distribution before adding the scaffold.

## Acceptance criteria for Phase 3 `plugin init`

Phase 3 is complete when the local scaffold uses only the public SDK distribution,
never overwrites existing user data, reuses canonical provider-id validation,
performs no network/token/Runner/package-manager/generated-code action, and a fresh
consumer outside the WebCodex checkout can `npm install`, typecheck/build, and run
the generated Plugin against `webcodex-plugin-v1` using the published exact SDK
version.

Once these conditions hold, the project will have a coherent extension path:
Runner-authoritative executable Plugins, a typed authoring SDK, a real first-party
dogfood Plugin, and an operator workflow that is convenient enough to use without
compromising the underlying execution model.
