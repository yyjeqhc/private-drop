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

### `plugin init` is deferred until SDK distribution is real

A public `webcodex plugin init` is **not** part of Phase 1. Today
`@yyjeqhc/webcodex-plugin-sdk` is a repository development package used by
first-party dogfood through a local `file:` dependency. It has no established npm
publication/versioning contract, and normal WebCodex binary/npm distribution does
not carry reusable SDK template/package assets. Generating a project that appears
standalone but depends on the current source checkout or a build-machine absolute
path would therefore be misleading.

Do not paper over that boundary with embedded SDK source, per-project vendoring,
an npm workspace, automatic pack/install, temporary `--sdk-path`, or generated
absolute paths. Once the SDK has a truthful repeatable external dependency source,
`plugin init` can be added as a deterministic local scaffold generator. At that
point it may create a minimal TypeScript/ESM project and documented `runner.toml`
snippet, but it still must not install packages implicitly, edit Runner config,
register/reload a provider, create credentials, or execute generated code.

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

### Phase 3: SDK distribution contract and `plugin init`

After the SDK and authoring loop have real consumers, establish the publication
and versioning contract for the SDK first. Only once a generated project has a
truthful, repeatable dependency source should `webcodex plugin init` become a
public command and scaffolding templates become a compatibility surface. At that
point the project can define which package names, generated layouts, Node versions,
and CLI flags become compatibility commitments.

Do not make WebCodex product releases depend on SDK version equality unless a
concrete distribution requirement appears. Native Plugin protocol versioning and
SDK package versioning are different concerns.

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
    contract; its later acceptance criteria belong to the distribution phase.

Once these conditions hold, the project will have a coherent extension path:
Runner-authoritative executable Plugins, a typed authoring SDK, a real first-party
dogfood Plugin, and an operator workflow that is convenient enough to use without
compromising the underlying execution model.
