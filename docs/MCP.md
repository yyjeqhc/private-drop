# MCP

[English](MCP.md) | [简体中文](MCP.zh-CN.md)

WebCodex exposes an MCP endpoint so ChatGPT, Claude, and other MCP clients can work with repositories through the Runner that owns them. Ordinary users only need to choose between **full use** and a **temporary trial**; protocol surfaces, scopes, and credential taxonomy are reference material, not onboarding prerequisites.

## ChatGPT: recommended full setup

For everyday use, run a regular Server + Runner. Follow the [Full Setup guide](PERSONAL_SETUP.md) for one-time login and project registration, and use `--print-mcp-config` during that same `webcodex login` to obtain regular HTTPS MCP connection values. Public HTTPS, Cloudflare Tunnel, and OpenAI Secure MCP Tunnel are reachability choices; they do not change the capabilities of this full development path.

If you only want to try one repository temporarily, use the `share` path below.

## ChatGPT: temporary `share`

Explicit `share` is supported on Linux, macOS, and Windows and owns a temporary single-project environment for that foreground run. Windows x64 can use the managed default Cloudflare Quick Tunnel; Windows ARM64 needs a trusted explicit/PATH `cloudflared` because the pinned Cloudflare release publishes no official ARM64 artifact. Managed OpenAI `tunnel-client` supports both Windows x64 and arm64.

For the default temporary public path, WebCodex reuses an explicit/PATH `cloudflared` or downloads its pinned verified managed copy automatically, then run:

```bash
npm install -g @yyjeqhc/webcodex
cd /path/to/your/repository
webcodex share
```

When the CLI says **WebCodex ready**:

1. In ChatGPT Developer Mode, create a custom app using MCP.
2. Paste the printed **MCP URL**.
3. Choose **Access token / API key** (Bearer token) for the default share.
4. Paste the printed temporary **Credential**.
5. Run **Scan Tools**.
6. Try: `Inspect this repository and summarize its structure. Do not make changes.`

The command performs project setup itself. Hosted ChatGPT cannot reach a
loopback-only `webcodex run`, so `setup`, `doctor`, and `run` are not required
steps before `share`. ChatGPT UI labels can vary by rollout; use the CLI output
as the source of truth for URL and authentication. Developer Mode, custom MCP
apps, and write/modify actions are controlled independently by the ChatGPT plan,
workspace, and admin settings; those client-side permissions are not widened by
WebCodex scopes.

## Claude and other MCP clients

Use the same printed `/mcp` URL and authentication values. In Claude, add a
custom connector and paste the MCP URL. Other MCP clients should be configured
with the same endpoint and the authentication mechanism reported by the CLI.
When a client cannot set a Bearer header, `webcodex share --auth query-token`
provides an explicit temporary-share fallback: paste the printed sensitive
`/mcp?token=...` URL and choose No authentication. The query accepts only the
current share Project Credential; it is not a general PAT/OAuth/shared-key query
auth mechanism. Treat the full URL as a secret because URL queries may be logged.
For local-only clients, `webcodex share --tunnel none` exposes the loopback MCP
endpoint without `cloudflared`.

For an OpenAI-only private transport, create/select a Secure MCP Tunnel, export
`CONTROL_PLANE_TUNNEL_ID` plus a Restricted `CONTROL_PLANE_API_KEY` with Tunnels
Read + Use, and run `webcodex share --tunnel openai`. ChatGPT uses Connection:
Tunnel + No authentication; the temporary WebCodex Bearer stays local and is
injected by the pinned verified OpenAI `tunnel-client`.

For a regular independent Windows Server + Runner reached through OpenAI Tunnel, or to troubleshoot a case where local `/readyz` is healthy but ChatGPT Connector creation still fails, see the [Windows + OpenAI Secure MCP Tunnel deep dive](WINDOWS_OPENAI_TUNNEL.md). It is advanced setup/troubleshooting material, not required reading for a first-time user.

## Result cards

On operator surfaces, clients advertising MCP Apps HTML support can display
read-only result cards for `list_jobs`, `observe_jobs`, `cargo_check`, `cargo_test`,
`go_test`, and `validation_summary`. Validation cards show execution outcomes,
bounded diagnostics, and current versus historical validation evidence.
Cards do not poll, retry, or invoke tools; the canonical tool result remains
available independently. `WEBCODEX_MCP_APPS_ENABLED=false` disables App metadata
and resources without disabling the underlying tools.

## Existing Server

For an existing hosted Server intentionally configured for shared-key clients,
use the long-lived shared-key path with the credential supplied by its operator:

```bash
webcodex connect https://webcodex.example --key-file /private/path/shared-key
```

`connect` starts/reuses the local Runner and prints the MCP URL and credential
source after the connection is verified. This is separate from fresh self-hosted
Docker enrollment: keep the Docker Server bootstrap administrator token on the
Server, create a short-lived pairing code there, and use `webcodex login` on the
repository machine. Self-hosting is documented in [Deployment](DEPLOYMENT.md).

Bearer/shared-key authentication is the simplest path. When a client requires
OAuth, use `share --auth oauth` or `connect --auth oauth` with that client's exact
callback URL and follow the CLI output. Managed-user OAuth remains a separate
advanced identity flow.

## Advanced / reference

### Runtime surface selection

A Server chooses its model-facing MCP surface at startup. Ordinary users do not need to select or understand the internal routing names; use the tools shown by the connected Server. Maintainers who intentionally change that surface should use the internal architecture/configuration contract; routing never changes the target tool's normal authentication, project, or safety checks.

### Tool result framing

Machine-readable MCP tool results are returned in `structuredContent`; `content` is a concise human-readable/protocol-native fallback. Clients that need fields should consume `structuredContent` rather than parse text.

Recovery fields in a result describe the next safe **explicit** call. They never grant authority and never trigger a hidden retry. In particular, an uncertain outcome must be reconciled before repeating an effect.

### Built-in local MCP gateway

A hosted Server can expose Runner-owned local stdio MCP providers through the same `/mcp` endpoint. Authorized callers use the single `mcp_tool` entry to list, describe, and call configured providers; provider process/instance identities and schema-revision state stay internal.

Configure local providers on the Runner under `[mcp]`. Access requires the explicit `mcp:local` permission; hosted OAuth clients opt in with `webcodex connect ... --oauth-local-mcp`. See [Runner](RUNNER.md#provider-side-gateway-v1-compatibility) for provider compatibility details.

### Managed SSH resource onboarding

The `ssh_resource` tool provides a narrow Runner-local onboarding path for
named SSH resources. `list` observes safe logical names and returns an opaque
exact-Runner/revision binding; `register` and `remove` consume that binding and
change only durable desired state. They never silently retarget a replacement
Runner or replay after an uncertain outcome. Raw SSH targets are not returned
in tool results or normal/full trace bodies. When `restart_required=true`,
restart the Runner and list again before selecting the resource in a Workflow
Session.

This surface requires the optional `ssh:local` permission. It is not part of the
ordinary hosted OAuth baseline; opt in explicitly with
`webcodex connect ... --oauth-local-ssh`. See
[Runner](RUNNER.md#ssh-session-resources-advanced) for static-vs-managed and
PersistentShell details.

### OAuth2

When OAuth is enabled, MCP clients can use the authorization-code flow instead of a static token. Register the client's exact callback URL, keep `offline_access` when the host requests refresh-token support, and follow the connection values produced by `share --auth oauth` or `connect --auth oauth`. Server setup is in [Deployment](DEPLOYMENT.md#oauth2).

For ordinary hosted `connect --auth oauth`, the Runner keeps its hosted credential while the MCP client receives a separate OAuth credential. Add `--oauth-computer-permissions`, `--oauth-local-mcp`, or `--oauth-local-ssh` only when those optional capabilities are needed. Existing clients are not silently widened; a real permission change requires reauthorization.

Project-first `share --auth oauth` remains bound to that temporary share environment. Managed-user OAuth is a separate advanced flow (`connect --auth managed-oauth`). OAuth credentials are never valid on Runner transport.

For the credential and scope model, see [Authentication](AUTH_MODEL.md#oauth2).

### Grok custom connector (OAuth)

Grok supports custom MCP connectors and can complete the OAuth flow required by
the MCP server. For a self-hosted WebCodex Server, first expose
`https://your-domain.example/mcp` over public HTTPS and enable OAuth:

```text
WEBCODEX_OAUTH2_ENABLED=true
WEBCODEX_OAUTH2_ISSUER=https://your-domain.example
WEBCODEX_PUBLIC_URL=https://your-domain.example
```

For the current Grok web connector flow (verified in August 2026), register this
redirect URI exactly:

```text
https://grok.com/connectors-oauth-exchange-code/
```

If Grok later presents or uses a different callback, register that exact value
instead. Create a dedicated OAuth client; the client secret is returned only
once:

```bash
curl -fsS -X POST https://your-domain.example/api/oauth/clients/create \
  -H "Authorization: Bearer $WEBCODEX_PAT" \
  -H "Content-Type: application/json" \
  -d '{"name":"Grok MCP","redirect_uris":["https://grok.com/connectors-oauth-exchange-code/"],"allowed_scopes":["runtime:read","project:read","project:write","job:run"]}'
```

In Grok's **Custom Connector** form, use:

| Field | Value |
| --- | --- |
| MCP server URL | `https://your-domain.example/mcp` |
| Client ID | the returned `wc_client_*` value |
| Client Secret | the returned one-time `wc_csec_*` value |
| Authorization Endpoint | `https://your-domain.example/oauth/authorize` |
| Token Endpoint | `https://your-domain.example/oauth/token` |
| Scopes | `runtime:read`, `project:read`, `project:write`, `job:run`, `offline_access` |
| Token Auth Method | `client_secret_post` |

WebCodex advertises PKCE `S256`; Grok can use PKCE together with
`client_secret_post`. Do not select `none (PKCE only)` for a WebCodex OAuth
client that has a client secret. `offline_access` is a protocol-level scope for
refresh tokens and is intentionally not stored in the OAuth client's
`allowed_scopes` permission list. The MCP Protected Resource Metadata omits
`scopes_supported` because pre-registered clients can have different scope
ceilings. General-purpose MCP clients can therefore omit `scope` and let WebCodex
default the authorization request to that client's registered `allowed_scopes`.

When the WebCodex authorization page opens, sign in with a current user PAT
(`wc_pat_*`) for the user whose authority Grok should receive. A Runner token
(`wc_agent_*`) is not a user login token. The resulting OAuth access token is
bound to that user and remains constrained by the registered/requested scopes.

Common setup failures:

- **Save & Connect is disabled:** Grok requires a Client ID before it can start
  the OAuth flow.
- **`invalid token`:** the PAT must authenticate against the same current
  WebCodex Server database. Do not use a Runner token or a stale PAT left from
  an older Server/database.
- **`invalid scope`:** every requested WebCodex permission scope must be in the
  OAuth client's `allowed_scopes`. For normal Grok MCP use, do not request
  `account:manage`; `offline_access` is accepted separately as a protocol scope.
- **redirect mismatch:** the redirect URI must match the registered value
  exactly, including path and trailing slash.

See xAI's [Connector documentation](https://docs.x.ai/grok/connectors) for the
current Grok Custom MCP UI and availability.

## Project-bound Connector workflow

`webcodex run` and `webcodex share` bind one configured repository and expose a small task-oriented MCP surface:

```text
task_start
task_list
task_resume
files_list
files_read
files_search
code_navigate
edits_apply
checks_run
commands_run
task_review
task_cancel
task_finish
code_impact
```

Start with `task_start`. The Connector already knows the project, so prompts do not need runtime project ids or project discovery. A returned `task_id` is the durable handle for that Connector task; use `task_resume(task_id)` when you explicitly want to continue it. Do not assume that the same chat, HTTP/MCP connection, or credential automatically resumes prior work.

The exact MCP Tasks-extension materialization/polling protocol is an implementation compatibility detail and is intentionally omitted from this user-facing guide.

## Golden coding loop

```text
task_start
→ files_list
→ files_read / files_search / code_navigate / code_impact
→ edits_apply
→ checks_run
→ task_finish
→ task_review
```

`task_start` has two execution modes:

- `normal` (default) is writable coding. WebCodex prepares a managed isolated Git
  worktree outside the target checkout, runs edits/commands/checks there, and
  `task_finish` captures a stable result. The target checkout changes only after
  the project owner accepts that result locally. If isolation cannot be prepared
  or verified, `normal` fails closed; it never falls back to writing the target.
- `read_only` is analysis only. Reads, search, LSP navigation, and impact analysis
  remain available; structured writes, commands, and checks are rejected.

A task may remain in its current mode, and `read_only` may upgrade to `normal`
after write authority and isolated-workspace preparation succeed. A `normal` task
cannot downgrade to `read_only`: finish or reject the writable task, then start a
new `read_only` task. Any isolated writable result requires structured checks
before `task_finish`, independent of the persisted mode label.

- `files_list` answers "what is in this project" from the Git index, so
  ignored directories never appear. Call it before guessing paths.
- `code_navigate` provides read-only language-server status, document/workspace
  symbols, definitions, references, diagnostics, and hover. It accepts only
  project-relative paths and 1-based Unicode scalar positions; the Connector
  chooses the bound executor project. Arguments are operation-specific:
  `status` takes no extras; document symbols and diagnostics take `path`;
  workspace symbols takes `query`; definition, references, and hover take
  `path` + `line` + `column`. Unsupported fields are rejected. It is available
  in normal and read-only tasks.
- `code_impact` performs one bounded call-hierarchy operation from a
  project-relative source position. It accepts `incoming`, `outgoing`, or
  `both`, breadth-first depth 1 or 2, and a global edge limit of 1..100. It
  returns only normalized project-local roots, edges, and bounded call-site
  ranges; unsupported language servers fail explicitly with no grep or AST
  fallback. It is available in normal and read-only tasks.
- `edits_apply` is the guarded edit tool; `commands_run` is the bounded escape
  hatch for commands that need a shell.
- `checks_run` performs structured validation. Follow its returned retry/status guidance rather than rebuilding internal operation identity by hand.
- `task_finish` produces a stable result; a human reviews and accepts or
  rejects it locally with `webcodex task accept <id>` / `webcodex task reject
  <id>`. The model can never accept its own work.

### Validation recipes

`checks_run` accepts `format`, `check`, and `test` plus an optional `recipe`
enum (`rust`, `node`, `python`, `go`). Omit `recipe` for automatic resolution
from the nearest `Cargo.toml`, `package.json`, `pyproject.toml`, or `go.mod`
relative to the task `cwd`. Recipes do not install dependencies, mutate
lockfiles, or use the network. A missing tool is an executor failure; a
started validator returning non-zero is an assertion failure.

| Recipe | Marker | `format` | `check` | `test` |
| --- | --- | --- | --- | --- |
| Rust | `Cargo.toml` | `cargo fmt -- --check` | `cargo check --all-targets` | `cargo test` |
| Node | `package.json` | first of `format:check`, `format-check`, `check:format` | first of `check`, `typecheck`, `lint` | exact `test` |
| Python | `pyproject.toml` | configured Ruff/Black | configured Ruff/Mypy | configured pytest |
| Go | `go.mod` | unavailable | `go vet ./...` | `go test -json ./...` |

### Long validation continues durably

`checks_run` and `commands_run` use durable executions and may quick-yield
after about 8 seconds while work continues. On the fourteen-tool Connector
surface, call `task_review` with `after_cursor` / `wait_ms` (and
`include_output_tail=true` when output is needed) until the execution becomes
terminal; use `task_cancel` to stop it. Do not re-run an operation merely to
poll it.

On regular runtime surfaces, long-running work may instead be exposed as a WebCodex Job. Use the Job observation/recovery guidance returned by the connected Server rather than starting another copy. Opaque observation tokens should be returned unchanged; they are read cursors, not credentials or execution authority.

## First safe prompt

```text
Use the configured WebCodex project. Start a read-only task, read README.md,
summarize the project, review the result, and finish. Do not edit files.
```

No project discovery or runtime identifier belongs in this prompt.

## Read and search bounds

- `read_file` is a bounded streaming range reader: `start_line` (default 1),
  `limit` (default 2000, max 2000), returns the range plus the complete-file
  SHA-256 and line metadata, and a `next_start_line` to continue.
- `read_files` batches up to 8 single-file reads with independent item
  results.
- `search_project_text` is the default search tool (ripgrep first, bounded in
  work and bytes); `search_project_texts` batches up to 8 queries.

An empty search result is affirmative no-match evidence only after a recognized
backend reports a successful completed/no-match status. Missing or malformed
backend identity, missing completion status, status/output disagreement,
backend failure, Runner failure, timeout, request drop, and provider failure
return failures instead. Search failures retain the compatibility `code` and
add bounded `failure_stage` plus a specific `reason_code`. Batch failure items
retain their broad `reason_code` and preserve the single-search provenance as
`failure_stage` and `detail_code`; successful batch items remain sparse.

Failures return small structured errors with project-relative paths only —
never absolute paths, commands, Runner/provider stderr, or arbitrary provider
prose.

## Common errors

| Code | Meaning | Action |
| --- | --- | --- |
| `project_not_configured` | No canonical setup exists | Run `webcodex setup` |
| `project_credential_invalid` | Private Project Credential is missing or mismatched | Restore both matching private files or recreate the profile |
| `project_credential_rejected` | The reachable server rejected the credential | Restore the server-matching credential |
| `workspace_unavailable` | The configured Git workspace is unavailable | Restore the workspace, then run doctor |
| `server_unreachable` / `agent_offline` | The project Runner/runtime is unavailable | Run `webcodex run` / `webcodex doctor` |
| `required_capability_unavailable` | The current Runner/runtime lacks a required coding capability | Upgrade all binaries |
| `task_not_active` | The task can no longer mutate or execute | Start a new task |
| `execution_not_terminal` | Finish is blocked by active/unknown work | Review/wait/cancel |
| `checks_required` | A normal task has not run checks | Call `checks_run` |
| `checks_stale` | The workspace changed after the last check | Run a new check |

## Advanced runtime surface

Beyond the project-bound Connector, WebCodex can run as a multi-project
management ToolRuntime with discovery, session, LSP, raw job, and artifact
tools. That is an advanced surface for operators, not the canonical project
Connector and not a prerequisite for ordinary coding.

### ChatGPT file bridge

On broader MCP operator surfaces that expose artifact tools, WebCodex supports
host-native file transfer in both directions without routing complete binary
payloads through model text:

- `import_conversation_files_to_project` imports 1..10 files supplied by the
  ChatGPT host through `openai/fileParams`. This applies to user-selected
  conversation attachments and to newly generated files when the host binds
  them as file parameters. The Control downloads the referenced bytes and
  commits them through the existing bounded artifact-write path; callers should
  not construct download URLs or manually Base64-transfer those files.
- `export_project_artifact` prepares one bounded project artifact for download
  and returns a short-lived authenticated MCP `ResourceLink` plus metadata.
  `tools/call` does not contain the complete binary. The host follows
  `resources/read` to obtain the binary resource; authentication and current
  project-read authority are checked again, and the artifact metadata is
  revalidated before the bytes are returned.
- The resource URI is not standalone bearer authority. Export handles are
  short-lived process-local presentation state, and the normal project artifact
  size, MIME, path, and authorization bounds remain in force.

`read_project_artifact` remains the bounded chunk-inspection API; it is not the
large-file download path. Office artifacts such as DOCX/PPTX/XLSX and PDFs use
the same artifact transport and can therefore move between a project and a
supporting ChatGPT host without a model manually carrying their Base64.

When a broader model coding surface exposes `work_on_project`, use the
[Coding Workflow](CODING_WORKFLOW.md) for the canonical bootstrap, behavioral-role
mental model, and validation/closeout guidance. See [Architecture](ARCHITECTURE.md)
and the `webcodex` CLI for operator tooling.
