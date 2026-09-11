# Claude Code MCP provider (experimental)

`webcodex-runner` can use `claude mcp serve` for one allowlisted, single-call
search capability while WebCodex remains the online MCP/API, authorization,
session, project, permission, timeout, and audit boundary.

This provider is experimental, disabled by default, and not recommended for
ordinary deployments. Native execution is the default strategy. Enable the
provider only through an explicit Runner configuration (not the server
configuration):

```toml
[tool_providers]
strategy = "claude_code_then_native"

[tool_providers.claude_code]
enabled = true
command = "claude"
args = ["mcp", "serve"]
timeout_secs = 30

[tool_providers.claude_code.mapping]
search_project_text = "project_search"
```

The only allowlisted capability in ordinary configured provider routing is
`search_project_text`. The mapping value is an explicit example, not a built-in
assumption. Configure it only after a probe confirms that the installed Claude
Code version exposes a schema-compatible search tool; otherwise leave it
unmapped so `claude_code_then_native` uses bounded Native `rg`/`grep`. On every
project process start the provider performs MCP `initialize`, sends
`notifications/initialized`, and calls `tools/list`. The search capability is
available only when its configured tool name is present and the discovered
input schema contains all fields required by the adapter. Use the names exposed
by the installed Claude Code version. This ordinary routing path never executes
WebCodex file writes or edits.

Strategies:

- `native` — existing WebCodex execution only; this is the default.
- `claude_code` — return a structured provider error when Claude is disabled,
  missing, incompatible, or fails.
- `claude_code_then_native` — use Claude first. Searches may fall back to the
  existing bounded Native `rg`/`grep` command after failure. Because configured
  provider routing exposes only read-only search, it never routes a WebCodex
  write to Claude and has no uncertain-write state.

Claude Code builds do not necessarily expose a search tool. The real smoke with
Claude Code 2.1.220 exposed `Edit` but no schema-compatible search tool. This
does not disable WebCodex search when using `native` or `claude_code_then_native`:
the latter falls back to the existing bounded Native `rg`/`grep` command. Strict
`claude_code` strategy instead returns a capability error when no mapped search
tool is available. Ordinary search routing does not use Claude's `Bash` tool and
never invokes Claude's `Edit`; the configured provider surface is limited to
`search_project_text`.

The agent runs one lazy child per canonical registered project root, fixes the
child `cwd` to that root, bounds requests/responses/pending calls, discards
child stderr, and terminates a timed-out child so a late request cannot keep
running. The next
call starts a fresh child lazily. It passes only a small environment allowlist
needed for executable lookup, locale, temporary files, and Claude's local
configuration. It does not inherit API-key or WebCodex credential variables.

The search adapter normalizes provider output into project-relative records.
A recognized, completed response with no records uses search exit status 1, so
the server can distinguish affirmative no-match evidence from malformed or
incomplete output. A provider record containing an absolute, parent-traversing,
or otherwise untrusted path fails with a bounded provider error (or uses the
configured Native fallback); it is never silently converted into an empty
successful search.

WebCodex `read_files` always keeps its Native file-read implementation in ordinary
provider routing. That routing path does not discover, map, or call Claude's
`Read` tool.

Claude tools remain an agent-internal implementation detail. WebCodex builds
its public MCP `tools/list`, runtime registry, OAuth policy, and OpenAPI from
the static WebCodex tool definitions only. Claude `tools/list` output is never
inserted into those registries. A Claude upgrade may therefore add `Read`,
`Bash`, `Write`, `Edit`, or other names to provider discovery without making
any of them visible to an external WebCodex client. Public names and input
schemas, including `write_project_file` and `apply_text_edits`, are identical
with the provider disabled or enabled.

The bounded version reported by MCP `initialize.serverInfo` is exposed in
provider status after a successful start. A version is retained only when it
matches a small version-string character allowlist. Status queries are passive:
`runtime_status`, `list_runners`, and local snapshot reads never start Claude.
With the default Runner configuration, the expected snapshot is
`strategy=native`, `enabled=false`, and `process_state=not_started`; this confirms
that observability is deployed but does not mean Claude has been configured or
started. The executable path is not exposed, and a missing command never
prevents the agent from starting.

`runtime_status` and `list_runners` expose the current bounded snapshot under
`tool_providers`. Registration and reconnect carry a complete snapshot. Later
changed revisions reuse the existing agent transport: polling agents attach
them to their next poll, while WebSocket/QUIC agents send a changed-only
`runtime_metadata` envelope after a result or on the existing keepalive tick.
There is no extra blocking round trip per tool call. Repeated identical state
is not resent, and only one metadata snapshot may be in flight at a time so an
older snapshot cannot overwrite a newer `last_call`. Metadata send failure
releases the claim for a later keepalive/reconnect retry, does not change a tool
result, and network I/O occurs after the provider state lock has been released.

## Explicit Runner config reload

`runner.toml` is loaded at startup. On Unix, an operator can explicitly reload
the same config path without disconnecting the agent:

```bash
sudo systemctl reload webcodex-runner
```

The generated unit maps this to `SIGHUP`. A valid reload atomically replaces
one request-time generation for `policy` (`allow_raw_shell`,
`allow_cwd_anywhere`, `allowed_roots`, `max_timeout_secs`, `max_output_bytes`),
`shell` (`default_profile`, `profiles`, `program`, `args`, `path_prepend`,
`env`, `init_script`), and `tool_providers` (strategy plus the Claude Code
enabled flag, command, args, mapping, and timeout). Requests and jobs that
already captured the old generation keep its
policy, timeout, shell environment, and Provider route. In-flight Claude search
calls are allowed to finish; their old Provider process is shut down when the
last old-generation caller releases it. New calls cannot enter a disabled
Provider.

Identity, server/auth, registration, project source, concurrency, and transport
fields still require restart: `server_url`, `token`, `client_id`,
`display_name`, `owner`, `hostname`, `project_registry_dir`, `poll_interval_ms`,
`capabilities`, `max_concurrent_jobs`, `transport`, `websocket_connect_timeout_secs`,
and `quic.*`. A mixed reload applies the hot
sections and reports these field names as `restart_required_fields`; it never
reports their values. Read, parse, validation, or Provider-config failure keeps
the active generation unchanged.

The latest bounded result is exposed as `tool_providers.config_reload`
(`generation`, result/error code, and restart-required summary). Generation
starts at 1 and advances only after a valid reload. `project-registry/*.toml` keeps
its existing independent cache refresh. Reload does not change public MCP
tools, refresh MCP metadata, or add an OpenAPI operation.

The opt-in process-level smoke exercises the real Server-to-Agent dispatch and
Unix signal path:

```bash
WEBCODEX_E2E_AGENT_RELOAD=1 \
./scripts/test-runner-config-reload-e2e.sh
```

It runs a temporary Server, Agent, project, Git fixture, and config without
systemd or Claude Code. It verifies valid, invalid, mixed, and recovery SIGHUP
reloads, then checks Agent/Server process groups, the loopback port, fixture,
and temporary-directory cleanup.

The provider lifecycle uses `not_started`, `starting`, `initializing`,
`discovering`, `mapping`, `running`, and `stopped`. State revisions are produced
when configuration is initialized, the child starts, initialize succeeds,
tools/list succeeds, mappings are validated, a call succeeds or fails, a
timeout/EOF/process exit occurs, shutdown runs, or a later call restarts the
child lazily. `available=true` means an initialized/discovered provider process
is reusable; a timeout or connection loss makes it unavailable until lazy
restart succeeds.

`claude_code.last_call` is one bounded summary, not an unbounded history:

```json
{
  "capability": "search_project_text",
  "selected_provider": "claude_code",
  "fallback_used": false,
  "result": "success",
  "duration_ms": 14,
  "error_code": null
}
```

`write_state` is absent for ordinary search-routing calls, so their
`last_call.write_state` is never present. A successful Native search fallback
records `selected_provider=native`, `fallback_used=true`, and no final error
code; `last_error_code` still identifies the Claude-side reason that caused the
fallback. Strict `claude_code` mode records `selected_provider=claude_code`,
`fallback_used=false`, `result=failure`, and a capability error code when no
compatible search tool is mapped.

All provider strings are allowlisted or bounded, discovered names are sorted,
deduplicated, and capped at 64, and only the configured WebCodex capability
key (`search_project_text`, with `edit_file` accepted from older Runners for
mixed-version compatibility) is accepted by the server. Provider status never
contains environment variables, authentication data, Claude configuration,
executable/project paths, request arguments, file contents, user code, stderr,
raw RPC responses, tokens, or cookies.

For a non-mutating active probe, use the opt-in diagnostic test. It creates an
empty temporary directory, starts `claude mcp serve`, performs only initialize
and tools/list, prints the safe status object, and shuts the process group down.
It does not call a tool, read a project file, run Bash, install Claude, log in,
or start a model conversation:

```bash
WEBCODEX_PROBE_CLAUDE_PROVIDER=1 \
cargo test -p webcodex-runner --bin webcodex-runner opt_in_real_claude_mcp_probe -- --nocapture
```

The default test suite uses a standalone fake stdio MCP server. A real local
smoke check is opt-in:

```bash
WEBCODEX_TEST_CLAUDE_MCP=1 cargo test -p webcodex-runner --bin webcodex-runner opt_in_real_claude_mcp_smoke -- --nocapture
```

This search-only smoke test prints a bounded tool/schema inventory, resolves a
configured or schema-compatible search mapping, calls the search tool only
inside a temporary fixture, reports search as unavailable when the installed
version has none, and confirms provider shutdown reaps its Claude process. It
does not install Claude Code or perform login, and this smoke never invokes
Claude `Edit`.

The full server/agent path is also opt-in:

```bash
WEBCODEX_E2E_CLAUDE_PROVIDER=1 \
./scripts/test-claude-provider-e2e.sh
```

It builds a temporary Git fixture, uses independent Server/Runner configuration,
an automatically selected loopback port, and a temporary HOME/XDG/Claude config
directory. It checks the public MCP tool set before and after Claude discovery
(including that the removed `replace_in_file` never re-enters the surface),
Native read, Native `rg`/`grep` search fallback, strict Claude search
capability-error behavior, a clean worktree, provider process-group cleanup,
and port release. The default suite never requires Claude Code, login, network,
a fixed fixture path, or user-global configuration. Default `native` strategy
behavior is unchanged.
