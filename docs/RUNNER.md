# Runner

The Runner is the component that executes the actual work. The executable is
`webcodex-runner`; the CLI namespace that manages it is `webcodex runner ...`.
`webcodex` and `webcodex-runner` are separate executables. Operator lifecycle
commands use the `runner` namespace; historical `agent` terminology remains only
where it is part of a compatibility-facing token, storage, identity, project-id,
or wire contract. This page explains what the Runner does, how it connects, how it
registers projects, how to operate it as a service, and its main runtime
concepts.

For installation and service setup, see [Deployment](DEPLOYMENT.md). For the
commands that manage the Runner, see [CLI](CLI.md#runner-lifecycle).

## What the Runner does

The Runner runs on the machine that owns the repositories. It connects out to
a WebCodex Server, registers the projects it is allowed to serve, and executes
bounded operations — file reads and edits, Git inspection, structured
validation, shell commands, and long-running Jobs — inside those project
boundaries.

The Runner is the trust boundary closest to your repository. Configure it with
narrow allowed roots and explicit shell profiles rather than broad interactive
shell state.

## Core terms

| Term | Meaning |
| --- | --- |
| **Server** | Authenticates callers, stores shared runtime state, and routes work. |
| **CLI** | The `webcodex` operator/developer command. |
| **Runner** | The `webcodex-runner` process that executes repository work. |
| **profile** | A named local Runner/client configuration. |
| **client_id** | Stable logical name for one Runner/device. |
| **Project** | A repository/workspace registered by that Runner. |

Some compatibility-facing values still use the historical word `agent`, including the `wc_agent_*` Runner-token prefix and `agent:<client_id>:<project_id>` runtime Project address. They do not refer to WebCodex's separate Durable Agent domain, and ordinary users do not need the process-level lease identifiers behind Runner recovery.

### Runner config filename compatibility

`runner.toml` is the canonical config filename. A legacy directory containing only `agent.toml` remains readable for compatibility; if both names exist in the same config directory WebCodex fails closed and asks the operator to resolve the ambiguity. `WEBCODEX_RUNNER_CONFIG` is the current path override; the old `WEBCODEX_AGENT_CONFIG` remains a compatibility alias.

## Connecting to the Server

The Runner connects out to the Server using one of four transports, selected by
the `transport` setting in `runner.toml`:

| Transport | Config value | Use |
| --- | --- | --- |
| Auto | `auto` | Recommended for production when `[quic]` is configured: QUIC first, then WebSocket, then polling. |
| QUIC | `quic` | Strict QUIC only. Separate UDP listener on the Server. |
| WebSocket | `websocket` | Stable fallback for simple deployments without UDP. |
| Polling | `polling` | Last-resort fallback for constrained networks. |

The Runner authenticates with its Runner token (compatibility prefix `wc_agent_*`) or, in hosted shared-key mode, the matching shared key. This credential is for Runner transport only; it is not an MCP, REST, or GPT Actions credential.

WebSocket and polling authenticate the first-party Runner with
`Authorization: Bearer <token>`; query-string Runner credentials are not
accepted. QUIC keeps its credential in the transport-specific v1 first-register
frame while the shared Runner envelope remains credential-free.

### Server/Runner compatibility

When upgrading an older installation across the 0.4 boundary, upgrade the first-party Server and Runner together. Within `0.4.x`, first-party releases keep a stable protocol baseline and add new optional capabilities explicitly; when an older compatible Runner lacks one of those capabilities, that feature fails closed instead of being guessed or emulated.

The exact protocol-generation field names, baseline capability list, registration grammar, and compatibility-test matrix are maintainer/wire-contract details and are intentionally omitted from this operations guide.

If you use QUIC, keep Server and Runner QUIC settings compatible. `[quic].keepalive_interval_secs` defaults to 20 seconds and accepts `1..=25`; invalid values are rejected rather than silently clamped.

## Registering projects

Projects live on the Runner machine. The Runner registers allowed directories
with the Server; the Server does not scan the filesystem and does not invent
project paths.

Each registered project is a one-file-per-project TOML file in the Runner's
`project_registry_dir` (default `project-registry`). The format:

```toml
id = "webcodex"
path = "/srv/webcodex/projects/webcodex"
name = "WebCodex"
kind = "repo"
allow_patch = true
```

`id` and `path` are the important fields; `kind` is optional descriptive metadata.
The registry directory is storage for Project records, not a workspace root.

New configurations use `project-registry/` and `project_registry_dir`. A legacy
installation that has only `projects.d/` / `projects_dir` remains readable. If
both old and new locations/fields are configured, WebCodex fails closed instead
of merging or guessing precedence. Use `--project-registry-dir` in new CLI
commands.

Runtime project ids take the shape `agent:<client_id>:<project_id>`, for
example `agent:workstation:my-repo`. A project-bound Connector resolves this
internally; ordinary users do not type it.

### Allowed roots

`allowed_roots` in the Runner policy controls where projects may be registered
or created:

- Missing or empty `allowed_roots` defaults to `$HOME`.
- An explicit `allowed_roots` overrides that default.
- Use explicit roots to narrow a Runner to one workspace tree, for example:

```toml
[policy]
allow_cwd_anywhere = false
allowed_roots = ["/root/git"]
```

### Registering projects at runtime

The runtime tools `register_project` and `create_project` let a client register
an existing directory or create a new one on an online Runner, subject to the
Runner's `allowed_roots` policy.

## Skill sources

`skill_list` presents one catalog while preserving three distinct ownership and
lifecycle models:

| Source | Location / owner | Trust | Version semantics |
| --- | --- | --- | --- |
| Project Skills | `<project>/.agents/skills/<package>/SKILL.md` | `project_content` | Live project content; no package revision. |
| Configured live Runner Skill roots | Operator-selected absolute directories on the Runner host | `operator_configured_guidance` | Live read-only filesystem content; no install, activation, rollback, or package revision. |
| Managed Runner Skill Store | Runner state under `runner-skills-v1` | `operator_installed_guidance` | Immutable package revisions with install, activation, removal, and rollback-oriented Store semantics. |

Configured live roots are optional and have no implicit defaults. Each configured
root contains normal Agent Skill packages directly:

```toml
[skills]
roots = [
    "/home/alice/.codex/skills",
    "/home/alice/.agents/skills",
    "/opt/company/agent-skills",
]
```

On Windows, use absolute local paths; TOML literal strings are convenient for
backslashes:

```toml
[skills]
roots = [
    'C:\Users\alice\.codex\skills',
    'C:\Users\alice\.agents\skills',
]
```

A root has the form `<root>/<package>/SKILL.md`, with optional package resources
such as `references/`. These directories are read directly by the Runner. WebCodex
does not copy them into the managed Store, and `skill_install`, `skill_activate`,
and `skill_remove_revision` continue to mutate only that Store.

The configured paths belong to the **Runner host**, even when the Server is on a
different machine. They are not added to `[policy].allowed_roots`, do not grant
ordinary Project file/shell/process tools access to those directories, and native
root paths are not projected through the model-facing Skill catalog. Skill reads
accept only an opaque `skill_id` plus a package-relative resource path; the Runner
resolves the root from its trusted configuration and rejects traversal or link
escapes.

Skill files remain live: editing `SKILL.md` or a resource is visible to the next
discovery/read without any reload. Changing the configured `roots` list is a
hot-reloadable Runner configuration change: edit `runner.toml`, run
`runner_config_check`, then `runner_config_reload` with the current generation.
No Runner process restart is required.

## Local MCP providers

The Runner can directly host persistent stdio MCP providers for WebCodex's built-in MCP gateway:

```toml
[mcp]
request_timeout_secs = 30

[[mcp.providers]]
id = "github"
name = "GitHub"
executable = "/absolute/path/to/github-mcp-server"
args = []
cwd = "/absolute/provider/workdir"
env_from_env = { GITHUB_TOKEN = "GITHUB_TOKEN", PATH = "PATH", HOME = "HOME" }
timeout_secs = 30
```

`executable` and optional `cwd` must be absolute host-local operator configuration. Invalid paths fail closed. `[mcp]` is restart-required configuration; changing a provider does not hot-reload it.

Provider processes do not inherit the Runner environment wholesale. `env_from_env` copies only explicitly named variables, and WebCodex's own sensitive transport/account credential variables cannot be mapped. A missing configured source variable fails before provider start.

Mapping a credential delegates that credential to the configured provider process. The provider can use it according to its own implementation and can choose to return derived or raw values through normal tool results; WebCodex does not attempt to redact arbitrary provider output. Treat configured providers as credential recipients, use least-privilege provider credentials, and remember that any caller authorized for `mcp:local` can exercise the provider capabilities that those credentials enable.

A provider starts on first real interaction and is then reused. The Server sees the logical provider `id`/`name`, not its executable path, environment values, PID, stderr, or Runner credential. `mcp_tool(action=list)` reports whether a provider id can be routed; `list(server=...)` and `describe` interact with the provider.

### Provider-side gateway V1 compatibility

The built-in Runner-to-provider gateway is intentionally a bounded stdio tool subset, not a transparent bridge to every MCP feature:

- provider-side tool behavior is based on MCP `2025-06-18`;
- `tools/list` and `tools/call` are supported;
- callbacks, list pagination, media/resources, and end-to-end progress forwarding are not supported;
- text tool results and bounded `structuredContent` are supported.

Unsupported protocol/content shapes fail closed instead of being silently translated.

## Shell profiles

Ordinary project shell/process execution defaults to `[shell] environment_mode =
"inherit"`: PATH, HOME/USERPROFILE and toolchain variables come from the process
that started the Runner. WebCodex transport/account credentials are filtered.
Shell `env` overrides inherited values; profile `env` overrides shell `env`.
Profiles cache this environment per project/config generation. An explicit
`init_script` can modify the snapshot; no startup script runs otherwise.

Set `environment_mode = "isolated"` for a minimal environment: `/usr/bin:/bin` on
Unix, or SystemRoot and its System32 PATH on Windows, plus configured env and
path_prepend. This is environment isolation, not a filesystem sandbox.
WebCodex does not automatically source `~/.bashrc` or `~/.profile`.
Configured MCP credential delegation remains explicit; this setting does not
expand its `env_from_env` allowlist. Native Plugins continue to use the existing
filtered shell/profile environment and native-only executable contract.

Windows `run_process` and `run_detached_process` accept `.cmd`/`.bat` shims through
Runner-owned `cmd.exe /d /s /v:off /c` conversion. Each argument is quoted; spaces,
empty arguments, `&`, `|`, and parentheses are supported. Quotes, `%`, `!`, `^`,
control characters and trailing backslashes are rejected before startup. The
command is bounded to 8000 UTF-16 units. Batch shims require a local drive cwd;
UNC cwd is rejected before spawn because cmd.exe cannot preserve it. Use a native runtime executable when
arguments fall outside this contract. Batch scripts remain responsible for how
they handle their own arguments (for example, forwarding with `%*`). Process
ownership, stdin, cwd, timeout, cancellation and detached reconciliation are
unchanged; a rejected argument never starts a Job payload.

Structured exact reads can inspect `node_modules` and `target`; recursive
search/listing skip those trees. Structured edits still reject generated trees.
Credentials, `.env*`, Runner configuration and `.git` control data remain protected
for both reads and writes.

Example Rust/Cargo profile in `runner.toml`:

```toml
[shell]
default_profile = "rust"

[shell.profiles.rust]
program = "sh"
args = ["-c"]

[shell.profiles.rust.env]
PATH = "/root/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
CARGO_HOME = "/root/.cargo"
RUSTUP_HOME = "/root/.rustup"
```

Example Python venv profile:

```toml
[shell.profiles.py-venv]
program = "bash"
args = ["-lc"]
init_script = '''
source .venv/bin/activate
'''
```

The `init_script` is project-relative: it is resolved from the project root,
so each project activates its own venv. A project can pin a profile:

```toml
id = "paper-exp"
path = "/root/git/paper-exp"
shell_profile = "conda-ml"
```

Resolution order: `project.shell_profile`, then `shell.default_profile`, then
the plain shell config (no snapshot).

`open_session_shell` is the separate long-lived shell path. On Unix it keeps a
real `sh`/`bash` process; on Windows the local path keeps the configured
`powershell.exe`/`pwsh.exe`-compatible process and reuses the same profile/env
selection. Windows PowerShell shell/profile args must retain their normal final
`-Command` flag in configuration; the persistent transport replaces that
one-shot payload mode internally with its private bootstrap. Named SSH resources
support `run_shell`/`run_job` on Unix and Windows through the separate `ssh_shell`
capability. Persistent SSH requires `persistent_shell` + `ssh_persistent_shell`:
Unix may reuse a Runner-local OpenSSH mux, while Windows owns one direct long-lived
`ssh.exe` channel. PTY/ConPTY terminal emulation is not implied.

Security notes for profiles:

- Never put tokens in `init_script`, and never `echo` secrets into it — the
  script's stdout is parsed as part of the snapshot.
- Status and runtime APIs expose only sanitized profile metadata (name,
  `has_init_script`, env key count, program, dialect) — never `init_script`
  bodies or environment values.
- Profiles run with a cleared environment plus an explicit allowlist; declare
  the env they need.

### Typed `run_script` languages

`run_script` accepts `sh`, `bash`, `powershell`, and `javascript`. JavaScript is
external Node.js execution on the Runner: WebCodex resolves `node` from the
prepared shell/profile PATH (or uses the configured shell/profile program when
it is `node`/`node.exe`), writes the typed body to a Runner-owned `.mjs` file,
and launches `node <temporary.mjs> <args...>` with native argv. `.mjs` fixes
Node ESM module semantics independently of project `package.json` or temporary
directory metadata. Script args and stdin remain separate inputs, and the child
uses the same resolved project cwd, timeout/cancellation, policy, and Job
lifecycle as other typed scripts.
For rolling upgrades, JavaScript also requires the Runner to advertise the
additive `structured_script_javascript` capability; that bit means the Runner
understands this typed-script language, not that `node` is installed. Node
availability is checked separately before script startup.

WebCodex does not install or bootstrap npm dependencies, inject `node_modules`
or `NODE_PATH`, or fall back to Bun, Deno, `tsx`, or TypeScript. Because the
entry `.mjs` lives in a Runner-owned temporary directory, relative ESM imports
resolve from that temporary module rather than the project cwd; use Node
built-ins or explicit project paths/file URLs when importing project code.
Modern JavaScript, promises, and async code are available according to the
Runner's installed Node.js runtime.

## Jobs and concurrency

A Job is a long-running command or validation that continues after the
initiating call returns. Jobs have a stable `job_id`, bounded stdout/stderr
tails, and can be stopped. Structured execution (`run_process`,
`run_script`) and validation Jobs hand off a single execution to a Job when it
outlives the synchronous grace period; the same process continues — it is never
restarted.

The Runner executes up to `max_concurrent_jobs` Jobs at once (default 4, valid
range 1..64). Values outside that range are rejected as configuration errors.
This is an operational tuning control, not a security boundary, and requires a
Runner restart to change:

```toml
max_concurrent_jobs = 4
```

When all slots are occupied, an accepted Job remains the same queryable Job
with the same `job_id` and reports `agent_queued`.

## Transports in more detail

### Server requirements for QUIC

Enable the QUIC listener on the Server and open the chosen UDP port:

```sh
WEBCODEX_QUIC_ENABLED=true
WEBCODEX_QUIC_LISTEN=0.0.0.0:8443
WEBCODEX_QUIC_CERT=/etc/letsencrypt/live/<host>/fullchain.pem
WEBCODEX_QUIC_KEY=/etc/letsencrypt/live/<host>/privkey.pem
WEBCODEX_QUIC_ALPN=webcodex-runner/1
```

The certificate SAN must match the `server_name` configured on the Runner.
`auto` tries QUIC first when `[quic]` is present, then WebSocket, then polling.

`[quic].keepalive_interval_secs` controls Quinn's transport-level UDP/QUIC
keepalive. It defaults to 20 seconds and accepts 1 through 25 seconds; values
outside that range are rejected rather than clamped. This is separate from
WebCodex application `Ping`/`Pong` liveness, which remains on its own 30-second
cadence. QUIC connects directly over UDP and does not use the Runner's HTTP
proxy settings.

### Outbound proxy for the Runner

If the Runner host needs an outbound HTTP proxy, set the proxy variables in
the Runner's service environment, not only in an interactive shell. WebSocket
honors `HTTPS_PROXY`/`https_proxy`, then `HTTP_PROXY`/`http_proxy`, then
`ALL_PROXY`/`all_proxy`; `NO_PROXY`/`no_proxy` bypasses matching hosts. The
supported proxy transport is `http://host:port` via HTTP `CONNECT`. QUIC does
not use proxy settings.

## Reconnect and recovery

A Runner disconnect is a liveness fact, not a lost-work fact. Accepted active
Jobs enter a bounded `recovering` state (default grace 120 seconds) and are
restored from the Runner's inventory when the same Runner instance reconnects.
Ordinary Jobs remain owned by that exact Runner process: a replacement instance
does not inherit their child processes, so they become `lost`. Explicit
`run_detached_process` Jobs are different: after a one-shot durable ownership
handoff, a narrow supervisor owns the payload tree. If that exact supervisor and
its fenced execution identity remain live, a replacement Runner can reconstruct
the same logical detached Job and route observation or stop through its durable
control state. This does not make ordinary process execution detachable, and it
does not promise survival across a machine reboot.

The Server distinguishes the stable Runner `client_id` from the current live process lease. A stale or replacement process cannot keep submitting results under the old lease, and ordinary child-process Jobs are not adopted by a replacement Runner. The exact lease identifier is an internal wire detail.

Reconnect happens automatically with a short delay. Authentication failure and
other fatal errors stop the Runner rather than looping forever.

## Shutting down and restarting

`webcodex-runner` stops cleanly on `SIGINT`/`SIGTERM`. It does not daemonize
itself. For a supervised deployment, use `webcodex runner install --scope
user|system` to install and supervise it as a user or system service, and keep
the token in the service environment.

After a machine reboot, a hosted `connect` profile is restarted by rerunning
`webcodex connect` or `webcodex runner start --profile <profile>`. Automatic
startup at logon is not implemented for hosted profiles.

On Windows, each Runner process also writes one small bounded lifecycle record under
`%LOCALAPPDATA%\webcodex\runner-exit-diagnostics-v1\<runner-hash>\` (falling
back to `%USERPROFILE%\.local\state\webcodex` and then `%TEMP%\webcodex` through
the normal Runner state-path rules). Only the newest eight process records are
retained. A record contains the local PID, process start time, build identity,
transport, shutdown-signal observation, transport return class, and clean/fatal
terminal classification. A sibling `*.panic.json` file is written best-effort
when a Rust panic hook runs and contains only the thread name and source location;
it never stores the panic payload. These diagnostics never contain credentials,
commands, Job output, request payloads, or Server response bodies, and a state
write failure never changes Runner lifecycle behavior.

For an unexpected supervised exit, correlate the supervisor's PID/timestamp with
the matching lifecycle record. `terminal=null` means the process disappeared
before Rust recorded a clean/fatal return. A panic sibling narrows that to a Rust
panic; `shutdown_signal_received_at_unix_ms` without a terminal record points to
an interrupted graceful-shutdown path. If neither is present, investigate an
external/native process termination or supervisor action before adding broader
Runner telemetry.

## SSH session resources (advanced)

A Runner with an available local OpenSSH client advertises the `ssh_shell`
capability. A Workflow Session may select a named SSH resource so `run_shell`
and `run_job` execute on a remote host through the Runner's own OpenSSH client
on both Unix and Windows. Unix may reuse a Runner-local ControlMaster transport;
Windows starts one direct `ssh.exe` process per one-shot/background execution and
does not use `ControlMaster`, `ControlPersist`, or `-S`. The separate
`ssh_persistent_shell` capability allows the same resource to be used by
`open_session_shell`: Unix may reuse its mux, while Windows owns one direct
long-lived `ssh.exe` channel.

```toml
[ssh.resources.tmp]
host = "tmp"
default_cwd = "/opt/webcodex-edge"
```

The `host` value is passed to the Runner machine's OpenSSH client, so normal
`~/.ssh/config`, keys, `ssh-agent`, and `ProxyJump` configuration remain on
that machine. Do not put credentials, private keys, or complete SSH
configuration into session data, Server storage, or tool input. A Session's
`execution_context.resource` routes `run_shell`, `run_job`, and supported
`open_session_shell` calls through that resource; file, Git, and LSP tools
remain local. Configuration reloads bind future commands to the current resource
generation; already-started SSH commands keep their own bounded lifecycle and
are never redirected, replayed, or blindly retried.

Authorized model clients can also onboard Runner-local SSH resources with the
`ssh_resource` MCP tool. `list` returns only safe logical names plus
`static|managed`, active/pending-restart state, and an opaque exact-Runner /
registry-revision binding. `register` accepts one explicit OpenSSH destination
argv and optional default cwd; `remove` deletes only managed desired state.
Raw targets, usernames, addresses, SSH options, credentials, and identity paths
are never returned by the tool. Static `[ssh.resources.*]` names are reserved
and cannot be overwritten or removed through this path.

Managed mutations are durable desired-state changes, not live configuration
edits. When a mutation returns `restart_required=true`, restart that Runner,
then `list` again before binding the resource into a Workflow Session. An
idempotent operation already aligned with the frozen startup snapshot may
return `restart_required=false`. Access is separately gated by the optional
`ssh:local` permission; hosted OAuth clients opt in with
`webcodex connect ... --oauth-local-ssh`.

The managed target is still consumed by the existing SSH transport. In
particular, registering a Windows OpenSSH destination does not imply that
PersistentShell can start there: the current remote persistent-shell contract
requires the existing remote `sh`/`bash` path. Remote PowerShell PersistentShell
is not part of this capability.

## LSP navigation (read-only)

The Runner can serve read-only semantic navigation through language servers
run on the repository machine:

| Language | Server | Markers |
| --- | --- | --- |
| Rust | `rust-analyzer` | `Cargo.toml` |
| Go | `gopls` | `go.mod`, `go.work` |
| Python | `pyright` | `pyproject.toml`, `setup.py`, `requirements.txt`, … |
| TypeScript / JavaScript | `typescript-language-server` | `tsconfig.json`, `package.json`, … |

The tools are `lsp_status`, `document_symbols`, `goto_definition`,
`find_references`, `document_diagnostics`, `hover`, and `workspace_symbols`.
The distinct `call_hierarchy` operation performs prepare plus bounded
incoming/outgoing breadth-first traversal inside the Runner. The canonical
Connector projects it as `code_impact`; raw protocol methods and opaque LSP
item data are never exposed.
They are read-only, project-bound, and constrained so that starting a language
server never executes repository code or fetches dependencies. Paths are
project-relative; external/dependency locations are omitted. Servers must be
installed on the Runner machine or pointed to by env overrides such as
`WEBCODEX_RUST_ANALYZER` and `WEBCODEX_GOPLS`. The gopls profile also forces
module/toolchain network access off and uses `-mod=readonly`; WebCodex never
installs gopls or fetches missing Go dependencies for semantic navigation.

Call hierarchy requires the separately advertised `lsp_call_hierarchy`
capability and the selected server's `callHierarchyProvider`. Missing support
fails explicitly without grep, AST, shell, or reference fallback.

## Operating the Runner

Minimal commands:

```bash
webcodex runner status --profile <profile>
webcodex runner logs --profile <profile> --lines 100
webcodex runner restart --profile <profile>
```

For a user service:

```bash
webcodex runner install --scope user --config <login-reported-runner-config>
webcodex runner status --scope user --config <login-reported-runner-config>
```

For an administrator-managed system service:

```bash
sudo webcodex runner install --scope system --profile <profile> \
  --user <runner-user> --working-directory /home/<runner-user>
sudo webcodex runner status --scope system --profile <profile>
```

Use the same `--scope` for install, status, start, stop, restart, logs, and
uninstall. User scope uses `systemctl --user`; system scope uses
`/etc/systemd/system`.

For an already-running Runner, use the first-class configuration workflow instead
of finding its PID or sending signals manually:

1. Edit the Runner's existing startup-bound `runner.toml`.
2. Call `runner_config_check(client_id=...)`. It reads only that bound path, does
   not activate the candidate, and returns the current generation plus bounded
   validation/restart metadata.
3. If valid, call
   `runner_config_reload(client_id=..., expected_generation=<current_generation>)`.
   The optimistic generation fence rejects stale callers before activation.
4. Inspect `runtime_status(client_id=...)` (or `list_runners`) after reload.

`runner_config_reload` never writes `runner.toml`; it only activates the candidate
already on disk. Hot-reloadable policy, shell, configured Skill roots, Native Plugin, and static SSH-resource changes can
become active immediately, while fields reported in `restart_required_fields`
remain startup-only until the Runner restarts. Invalid candidates leave the active
snapshot and generation unchanged. Managed `ssh_resource` mutations are different:
they use a frozen startup snapshot and require a Runner restart exactly when the
tool reports `restart_required=true`.

Plugin configuration is live-applied through the same validated Plugin candidate
admission/commit primitive used by `plugin_tool reload`; changing `[plugins]` is
not a restart-only operation. `plugin_tool reload` remains the narrower
`plugin:manage`-scoped entry when only Plugin state should be reloaded.

On Unix, service reload/SIGHUP remains an optional compatibility trigger and calls
the same authoritative reload primitive. Windows and macOS use the first-class
operation directly; no signal emulation or PID management is required. When a
validation failure is safely classifiable, config operations report only closed
non-secret atoms such as `field=max_concurrent_jobs` and `reason=out_of_range`;
raw TOML, configured values, paths, credentials, parser text, and shell environment
values are not projected.
