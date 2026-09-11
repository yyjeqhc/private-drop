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

## Next phase: Plugin authoring CLI

The next implementation phase should reduce the manual author loop without
creating another Plugin runtime. The target experience is conceptually:

```text
webcodex plugin init ./my-plugin
    -> edit/build/test locally
    -> configure the provider on an exact Runner
webcodex plugin check --runner <runner> --plugin <provider>
    -> fix bounded admission diagnostics
webcodex plugin reload --runner <runner>
webcodex plugin list/describe ...
    -> verify the committed catalog
    -> exercise the tool through the normal model/operator path
```

Exact command spelling can follow the current CLI conventions during
implementation, but the architectural split should remain as follows.

### `plugin init`: local authoring only

`init` should be a deterministic scaffold generator. It may create a minimal
TypeScript Plugin project containing, for example:

```text
package.json
tsconfig.json
src/plugin.ts
README.md
```

The generated Plugin should use the current SDK, build to ordinary ESM JavaScript,
and contain one small example tool plus a documented `runner.toml` provider
snippet. It should not edit a Runner config automatically, start a provider, make
network calls, install packages implicitly, or execute repository code as part of
project discovery.

Template generation is convenience, not authority. A raw protocol Plugin remains
a fully supported peer of an SDK-authored Plugin.

### `plugin check/reload`: thin adapters over current authority

Authoring CLI management commands should call the existing WebCodex management
path rather than reimplementing Plugin admission in the CLI or SDK.

Conceptually:

```text
webcodex plugin check
    -> authenticated Server request
    -> existing plugin_tool/check semantics
    -> exact Runner
    -> disposable Runner-owned candidate

webcodex plugin reload
    -> authenticated Server request
    -> existing plugin_tool/reload semantics
    -> exact Runner
    -> atomic candidate-set replacement
```

The CLI must not directly spawn the configured provider as a substitute for
Runner `check`, because doing so would use a different environment, process-tree,
bounds, and admission path. It also must not silently widen credentials. Management
operations continue to require the existing `plugin:manage` authority; inspect and
invoke scopes remain distinct.

The CLI should reuse existing WebCodex HTTP/auth/profile primitives and provide a
bounded human-readable view plus machine-readable JSON where current CLI
conventions support it. Runner-generated diagnostic codes remain canonical; the
CLI should not parse raw provider stderr into a new public error contract.

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

This is the highest-value next step because the SDK has already proved it can
carry a real effectful Plugin; the remaining obvious cost is the author/operator
workflow around that Plugin.

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

### Phase 3: package/distribution contract

After the SDK and authoring loop have real consumers, decide the publication and
versioning contract for the SDK and any scaffolding templates. At that point the
project can define which package names, generated layouts, Node versions, and CLI
flags become compatibility commitments.

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

## Explicit non-goals for the next phase

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

## Acceptance criteria for the authoring CLI phase

The phase is complete when all of the following are true:

1. A new TypeScript Plugin can be scaffolded without hand-writing protocol
   boilerplate.
2. The generated Plugin builds to ordinary JavaScript and passes SDK tests without
   requiring the Runner to understand TypeScript.
3. An operator can perform authoritative check and reload for one exact Runner
   without manually constructing `plugin_tool` JSON.
4. Failed admission preserves the bounded Runner diagnostic and does not replace
   the currently committed provider set.
5. A successful reload produces the same frozen catalog and binding behavior as
   direct `plugin_tool` use.
6. CLI credentials do not gain Plugin management scope implicitly.
7. No CLI/SDK code becomes a second provider supervisor, schema authority, or
   effect-lifecycle owner.
8. `safe-delete` can use the workflow without reintroducing legacy entrypoint or
   source compatibility shims.
9. Default CI remains deterministic and path-aware; authoring-only changes do not
   trigger unrelated Desktop/native/release lanes.

Once these conditions hold, the project will have a coherent extension path:
Runner-authoritative executable Plugins, a typed authoring SDK, a real first-party
dogfood Plugin, and an operator workflow that is convenient enough to use without
compromising the underlying execution model.
