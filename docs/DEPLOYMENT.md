# Deployment

[English](DEPLOYMENT.md) | [简体中文](DEPLOYMENT.zh-CN.md)

This guide is for **production and advanced self-hosting**: long-lived Servers, multiple machines/users, system services, reverse proxies, Docker, and operator-managed networking. If you are installing WebCodex on a normal Windows or macOS workstation, **do not start here**; the recommended path is [WebCodex Desktop + the official OpenAI Secure Tunnel](desktop-install.md). For CLI or an existing Server, use the [Full Setup guide](PERSONAL_SETUP.md). For a few-minute one-repository trial, use the [Quick Trial](QUICK_START.md).

## Components

- `webcodex` — the unified CLI for project workflows, Server/Runner lifecycle,
  enrollment, and operations.
- `webcodex-server` — the Server process exposing REST, GPT Actions OpenAPI,
  MCP, and Runner endpoints.
- `webcodex-runner` — the long-lived worker on the machine that owns the
  repositories.

Execution configuration belongs to the Runner that performs the work. The
retired Server `CODEX_*` settings do not select a coding-agent executable,
approval mode, timeout, or argument allowlist. Configure coding-agent providers
in the Runner's `[acp]` / `[[acp.agents]]` settings instead; see the
[ACP coding-agent guide](agent/acp-coding-agent-run.md). The Server needs its
writable data directory, not a separate legacy `uploads` directory.

## Build and install

The documented distribution path is the npm thin installer/wrapper:

```bash
npm install -g @yyjeqhc/webcodex
```

Supported package platforms are Linux x64, Linux arm64, macOS x64, macOS arm64, Windows x64, and Windows arm64. Windows supports CLI + Runner, explicit foreground Server, and explicit local `webcodex share --tunnel cloudflare|openai|none`. Windows x64 supports managed Cloudflare acquisition; Windows ARM64 Cloudflare requires a trusted explicit/PATH binary because the pinned upstream release has no official ARM64 artifact. Managed OpenAI `tunnel-client` supports Windows x64/arm64. WebCodex-managed Windows Server and Runner services remain unsupported; run them explicitly in the foreground instead. The npm wrapper
requires Node.js 18 or newer. The native Linux x64 artifact targets glibc 2.17
or newer.

Build from source:

```bash
cargo build --release --workspace --bins
export PATH="$PWD/target/release:$PATH"
```

This produces `webcodex`, `webcodex-server`, and `webcodex-runner`.

## Windows foreground Server and Runner

The npm package installs all three Windows executables. Windows can run both the Server and Runner directly without Linux, WSL, or a Windows Service. Keep the foreground terminals open while the processes are in use.

On the Windows Server machine, initialize an explicit env file and start the Server from PowerShell:

```powershell
$envFile = Join-Path $HOME ".config\webcodex\webcodex.env"
$dataDir = Join-Path $HOME ".local\share\webcodex"
webcodex server init --listen 127.0.0.1:8080 --data-dir $dataDir --env-file $envFile
webcodex server run --env-file $envFile
```

Pass the same `--env-file` explicitly when starting the foreground Server; do not rely on a managed-service default. For a local same-machine enrollment, open another PowerShell window and create a short-lived pairing code:

```powershell
webcodex pairing create --server-url http://127.0.0.1:8080 --env-file $envFile --username workstation --display-name "Windows Workstation" --ttl-secs 600
```

Then, on the Windows machine that owns the repositories, redeem only that short-lived code and run the generated Runner config in the foreground:

```powershell
webcodex login http://127.0.0.1:8080 --code <wc_pair_...> --allowed-root C:\src --project C:\src\my-repo
webcodex runner run --config <login-reported-runner-config>
```

When Server and Runner are on different machines, replace the loopback URL with the Server's reachable HTTPS URL and configure the Server listener/public URL plus a trusted reverse proxy or tunnel as described below. Do not copy the Server bootstrap token or env file to the Runner machine. `webcodex server install/start/stop/restart/logs/uninstall` and `webcodex runner install` remain unsupported on Windows; Ctrl-C or Ctrl-Break ends the foreground runtime.

To keep a Windows Server loopback-only while exposing MCP privately through an OpenAI Secure MCP Tunnel and operating an independent Runner like a normal long-lived Runner, see the [Windows + OpenAI Secure MCP Tunnel deep dive](WINDOWS_OPENAI_TUNNEL.md). It is advanced setup/troubleshooting material; ordinary users do not need it before understanding the full setup path.

## Connect a repository to an existing shared-key Server

The hosted shared-key path needs no local Server, database, reverse proxy, or
systemd unit, but it does require a shared key accepted by that Server (normally
supplied by its operator or recovered from an existing protected profile):

```bash
cd /path/to/your/repository
webcodex connect https://your-server.example --key-file /private/path/shared-key
```

`connect` uses the current directory as the project, writes an owner-only
profile, starts a detached Runner, and waits until the Server sees both the
Runner and the project. Use the printed `/mcp` URL and credential in your MCP
client. After a machine reboot, rerun the same `connect` or use
`webcodex runner start --profile <profile>`.

This is not first enrollment for a freshly self-hosted Server. For that case,
keep the bootstrap administrator token on the Server and follow the pairing /
`webcodex login` flow below. For automation of a shared-key deployment, prefer
`--key-file <path>` over `--key`. Do not pass both.

## First production deployment

A first-time operator does not need OAuth, QUIC, or account credentials. The
minimum production path:

1. Use a Linux x64 host with systemd, `sudo`, and a public HTTPS domain or
   trusted tunnel.
2. Install `@yyjeqhc/webcodex`, run `webcodex server init`, and install the
   `webcodex-server` service.
3. Configure the reverse proxy and set `WEBCODEX_PUBLIC_URL` to the exact
   public HTTPS origin.
4. Create a short-lived pairing code on the server and run
   `webcodex login <server-url> --code <code>` on the machine that owns the
   repositories.
5. Install the `webcodex-runner` service on that repository machine.
6. Run `webcodex ops status --strict`; only then import the GPT Actions schema
   or add the MCP connector.

### Server setup

Initialize the Server env file (creates the bootstrap `WEBCODEX_TOKEN` and the
listen/data settings):

```bash
sudo webcodex server init \
  --listen 127.0.0.1:8080 \
  --data-dir /var/lib/webcodex \
  --env-file /etc/webcodex/webcodex.env \
  --public-url https://your-domain.example
```

`server init` creates the selected data directory and the server-side bootstrap/admin token. It does not create user API tokens or Runner tokens. Its next-step guidance preserves the exact env/data paths supplied above.

Install and start the managed systemd socket/service pair:

```bash
sudo webcodex server install \
  --env-file /etc/webcodex/webcodex.env \
  --working-directory /var/lib/webcodex \
  --bin /usr/local/bin/webcodex-server
webcodex server status --env-file /etc/webcodex/webcodex.env
```

When `--working-directory` is omitted, `server install` uses `WEBCODEX_DATA` from the selected env file before falling back to the platform default. Install preflight rejects a missing/non-executable binary, missing working directory, unreadable env file, or unknown explicit User/Group before mutating systemd. `server status` likewise derives its default local HTTP probe from that env file's `WEBCODEX_ADDR`; an explicit `--url` still wins.

The managed Linux layout is intentionally split: `webcodex.socket` owns the
fixed `WEBCODEX_ADDR` listener, while `webcodex.service` owns only the Server
process. For ordinary binary replacement after this socket-activated layout is
established, stage the replacement and run:

```bash
sudo systemctl restart webcodex.service
```

Do **not** restart `webcodex.socket` during the normal Server replacement path.
The socket remains bound while the old Server drains and the new process later
inherits the same listener, eliminating the listener/`ECONNREFUSED` ownership
gap under normal bounded-backlog conditions. On SIGTERM (managed restart/stop)
or Ctrl-C/SIGINT (foreground), WebCodex first makes a process-local drain fence
authoritative, then asks Salvo to stop accepting connections and gracefully close
existing HTTP connections. A request admitted before that fence may run for up to
315 seconds and flush its response; a request that still reaches the old process
after the fence receives a retriable HTTP 503 without entering its handler. The
315-second bound is derived from the 300-second ordinary HTTP hard timeout plus a
15-second response/teardown margin. The generated systemd service uses
`TimeoutStopSec=330s`, leaving another 15-second margin so systemd does not
SIGKILL the process before the application-owned bound.

This is an availability-preserving graceful restart, not overlapping generations.
During a long drain, new TCP connections can remain queued in the systemd socket
backlog until the old process exits and the new Server inherits the listener, so
restart latency may approach the finite-request bound. Existing WebSocket,
HTTP keep-alive, and streaming connections may still disconnect/reconnect; there
is no WebSocket continuity or literal zero-interruption guarantee.

Use `--overwrite` on `server install` only when replacing an existing managed
pair. Migrating an already-active legacy direct-bind `webcodex.service` is a
one-time migration boundary: the installer fails closed rather than competing
for the live address. Stop the legacy Server first, then rerun the install with
`--overwrite`. This boundary does not provide a gap-free first migration.

### Tool invocation tracing

Tool-request tracing is an **operator diagnostic** and is disabled by default. Use metadata mode for lightweight lifecycle diagnostics; use `full` only when you explicitly need request/response payload capture:

```text
WEBCODEX_TOOL_REQUEST_TRACE=full
WEBCODEX_TOOL_REQUEST_TRACE_DIR=/var/lib/webcodex/tool-request-traces
WEBCODEX_TOOL_REQUEST_TRACE_RETENTION_HOURS=168
WEBCODEX_TOOL_REQUEST_TRACE_MAX_TOTAL_BYTES=2147483648
```

`full` tracing can contain source text, patches, command/script input and output, user messages, and secrets that were themselves present inside captured tool payloads. Protect the trace directory like other sensitive diagnostic data and keep retention/budget bounded. Trace capture is diagnostic only; trace-write failure does not change tool execution.

The exact trace file layout, correlation ids, model-facing forensic reader, queueing, compression, and payload-validation rules are implementation/maintainer details and are intentionally omitted here.

### Public HTTPS

Hosted MCP clients and GPT Actions require a public HTTPS URL. Set
`WEBCODEX_PUBLIC_URL` in the Server env file and put a reverse proxy in front
of `127.0.0.1:8080`. Nginx is supported; a named Cloudflare Tunnel is also a
valid front door. The same hostname must carry ordinary HTTPS requests and
`/api/agents/ws` (Cloudflare supports WebSocket upgrades). WebCodex CLI does
not automate reverse proxy or tunnel setup.

### Enroll a repository machine

On the machine that owns the repositories, as the ordinary user who will run
project commands (do not use `sudo`):

```bash
webcodex login https://your-domain.example --code <wc_pair_...> \
  --allowed-root "$HOME/git" \
  --project "$HOME/git/my-repo"
webcodex runner install --scope user \
  --config <login-reported-runner-config>
webcodex runner status --scope user \
  --config <login-reported-runner-config>
webcodex ops status --server-url https://your-domain.example \
  --token-file <login-reported-webcodex-user-token> --strict
```

`webcodex login` is the canonical client entry: it derives a unique device name, redeems the pairing code, and writes the client-side `webcodex-user-token` and a `runner.toml`. `--allowed-root` grants registration authority only; `--project` names the actual existing workspace to register. The generated `project_registry_dir` is a registry directory, not the workspace root. If login is performed without `--project`, use `webcodex project register --config <login-reported-runner-config> /path/to/repo` before project-bound work. Use the documented `login --device` and `--dir` options when an explicit device identity or alternate local base directory is required; there is no separate compatibility enrollment command.

The pairing code is created server/admin-side:

```bash
webcodex pairing create \
  --server-url https://your-domain.example \
  --env-file /etc/webcodex/webcodex.env \
  --username friendname \
  --display-name "Friend Name" \
  --ttl-secs 600
```

Copy only the short-lived `wc_pair_*` code to the client. Do not copy
`WEBCODEX_TOKEN`, user API tokens, Runner tokens, env files, or complete
`runner.toml` files between machines. Each friend should use a unique
`username`.

## Runner service scopes

`webcodex runner install` supports user or system scope. Non-root users default
to user scope; root defaults to system scope.

**User scope** uses `systemctl --user`, writes the unit under
`$XDG_CONFIG_HOME/systemd/user`, stores config under `$XDG_CONFIG_HOME/webcodex`,
and needs no `sudo`:

```bash
webcodex runner install --scope user --profile workstation
webcodex runner status --scope user --profile workstation
webcodex runner logs --scope user --profile workstation --lines 100
```

An enabled user unit follows that account's user manager. For unattended boot
persistence, an administrator may explicitly run
`sudo loginctl enable-linger <runner-user>`; WebCodex never changes lingering
automatically.

**System scope** uses `/etc/systemd/system` and requires a named non-root
`--user`:

```bash
sudo webcodex runner install \
  --scope system \
  --profile workstation \
  --user <runner-user> \
  --working-directory /home/<runner-user> \
  --config /etc/webcodex/clients/workstation/runner.toml
sudo webcodex runner status --scope system --profile workstation
```

A root Runner is refused unless `--allow-root-runner` is explicit (discouraged).
Use the same `--scope` for every lifecycle command. Example files live in
`deploy/` (`webcodex.env.example`, `webcodex.service.example`,
`webcodex-runner.toml.example`, `webcodex-runner.service.example`,
`nginx.webcodex.example.conf`).

## Docker (server-only)

The repository includes a server-only Dockerfile and Compose deployment that
runs `webcodex-server` plus the admin CLI; it intentionally excludes the
Runner, project repositories, and toolchains. Official releases publish one
multi-architecture image for `linux/amd64` and `linux/arm64` at
`ghcr.io/yyjeqhc/webcodex-server`.

An ordinary Server deployment does not require a repository clone or a Rust
toolchain. Download the self-contained bootstrap from the latest public Release
and run it:

```bash
mkdir -p webcodex-server && cd webcodex-server
curl -fLO https://github.com/yyjeqhc/webcodex/releases/latest/download/webcodex-server-bootstrap.sh
sh webcodex-server-bootstrap.sh https://webcodex.example.com
```

The Release-generated bootstrap embeds a Compose definition pinned to that
Release's immutable multi-arch image digest and materializes it locally as
`webcodex-server-compose.yaml`. Thus even the `latest/download` convenience URL
produces a fixed deployment after download, without a race between multiple
Release assets. To select a specific release instead, replace
`releases/latest/download` with `releases/download/v<VERSION>`. The bootstrap
records the materialized `COMPOSE_FILE` and exact image reference in its private
`.env`; later plain `docker compose` commands in that directory therefore reuse
the same pinned deployment.

Bootstrap is recoverable rather than all-or-nothing. It validates the strict
HTTPS origin, Compose/source assets, host port, Docker/Compose availability, and
published image before creating an administrator secret. It then advances the
private `.webcodex-bootstrap.receipt` through `AssetsPrepared`,
`SecretCommitted`, `ContainerStarted`, `ServerHealthy`, and `PairingReady`. The
receipt contains hashes and state, not the administrator token. `.env` is written
through a 0600 temporary file, synced, and atomically renamed. Success is printed
only after the Compose healthcheck and `/openapi.json` verification succeed; a
short-lived pairing code is created only after that readiness barrier.

If an install is interrupted or a startup/health check fails, keep `.env` and use
the same downloaded bootstrap in that directory:

```bash
sh webcodex-server-bootstrap.sh status
sh webcodex-server-bootstrap.sh resume
# Remove runtime effects while preserving the committed administrator token
# and named data volume:
sh webcodex-server-bootstrap.sh rollback
```

`resume` reuses a committed administrator token; it never regenerates one merely
because `docker compose up` or the health check failed. `rollback` deliberately
returns a post-secret installation to `SecretCommitted` instead of deleting
`.env`, because the Server may already have initialized durable data with that
token. An unmanaged or pre-transaction `.env` without a matching receipt is
never overwritten or deleted automatically.

For development or before the first public GHCR image has been activated, clone
the source and choose the explicit build path:

```bash
git clone https://github.com/yyjeqhc/webcodex.git
cd webcodex
./deploy/docker/bootstrap.sh https://webcodex.example.com --build-from-source
# Later source rebuilds use the same explicit override:
docker compose -f compose.yaml -f compose.build.yaml up -d --build
```

The default binding is `127.0.0.1:8080`. Put an HTTPS reverse proxy in front;
the bootstrap has already verified the local Server and created the first
short-lived pairing code. Use that code to enroll the machines that hold your
repositories. The first ever GHCR publication creates a private package by
GitHub default; a maintainer must make that package public once before the
workflow's anonymous-pull gate can succeed. End users do not need registry
credentials after that one-time activation.

## Runner configuration

Client enrollment generates the Runner config. Important settings in
`runner.toml`:

| Setting | Notes |
| --- | --- |
| `server_url` | Public WebCodex URL. |
| `token` | Runner credential. Do not commit or print it. |
| `client_id` | Stable id used in `agent:<client_id>:<project_id>`. |
| `owner` | Owner principal for this Runner. |
| `transport` | Prefer `auto` with `[quic]` configured. |
| `project_registry_dir` | Directory of project registry files. |
| `[policy]` | Local execution boundary (`allowed_roots`, etc.). |
| `[shell]` | Optional shell profile definitions and bounded persistent-shell limits. |
| `[ssh.resources.<name>]` | Optional named SSH target for Session-bound `run_shell` / `run_job`. |

Policy defaults: missing or empty `allowed_roots` defaults to `$HOME`; an
explicit `allowed_roots` overrides it. Use explicit roots to narrow a Runner,
for example to one workspace tree:

```toml
[policy]
allow_raw_shell = true
allow_cwd_anywhere = false
allowed_roots = ["/root/git"]
max_timeout_secs = 3600
max_output_bytes = 262144
```

After editing the already-running Runner's startup-bound `runner.toml`, use
`runner_config_check(client_id=...)`, then pass its `current_generation` to
`runner_config_reload(client_id=..., expected_generation=...)`, then inspect
`runtime_status(client_id=...)`. Check never activates the candidate; reload never
writes the file. Invalid candidates preserve the active snapshot/generation, and
`restart_required_fields` names startup-only changes that are not claimed live
until restart. Unix service reload/SIGHUP remains a compatibility trigger for the
same reload primitive, but is not required for first-class config control. Identity,
server/auth, project source, concurrency, capabilities, and transport changes
remain restart-only where reported.

`[plugins]` is live-reloadable: generic Runner config reload and `plugin_tool reload`
share the same Plugin candidate admission/atomic-commit primitive. Plugin provider
tools remain Runner-local capabilities behind `plugin_tool`; they are never promoted
into outer MCP `tools/list` and do not require a Runner restart for discovery.

For a foreground test, run `webcodex-runner --profile workstation`. Advanced
manual config generation uses `webcodex runner init`.

## OAuth2

OAuth2 remains disabled by default when a Server has no public origin. `webcodex server init --public-url https://your-domain.example` writes the public URL, enables OAuth with that exact issuer, and enables the shared-key OAuth bridge for ordinary hosted connect. For a hand-managed env file, the equivalent settings are:

```text
WEBCODEX_PUBLIC_URL=https://your-domain.example
WEBCODEX_OAUTH2_ENABLED=true
WEBCODEX_OAUTH2_ISSUER=https://your-domain.example
WEBCODEX_OAUTH2_SHARED_KEY_BRIDGE=true
```

For ordinary repository machines, no managed login is required. Connect with the MCP client's exact callback:

```bash
cd /path/to/your/repository
webcodex connect https://your-domain.example --auth oauth \
  --oauth-redirect-uri https://client.example/callback --project .
```

To let this ordinary shared-key OAuth client offer the fixed optional Computer consent set, explicitly opt in:

```bash
webcodex connect https://your-domain.example --auth oauth \
  --oauth-redirect-uri https://client.example/callback \
  --oauth-computer-permissions --project .
```

The Runner continues using its hosted credential while the MCP client receives a separate OAuth credential. `--oauth-computer-permissions` and `--oauth-local-mcp` are explicit opt-ins for optional capabilities; ordinary reconnect never silently adds them. A real OAuth permission change requires the client to authorize again. ChatGPT never receives the Runner/shared-key credential, and OAuth tokens are not valid on Runner transport.

If a managed-user OAuth identity is specifically required, use the advanced `webcodex login` flow followed by `webcodex connect ... --auth managed-oauth --oauth-redirect-uri ...`; `--user` applies only there.

Create an OAuth client (the `client_secret` is returned only once; only its
hash is stored):

```bash
curl -fsS -X POST https://your-domain.example/api/oauth/clients/create \
  -H "Authorization: Bearer $WEBCODEX_PAT" \
  -H "Content-Type: application/json" \
  -d '{"name":"ChatGPT MCP","redirect_uris":["https://chatgpt.com/connector/oauth/<callback-id>"],"allowed_scopes":["runtime:read","project:read","project:write","job:run"]}'
```

`allowed_scopes` limits what an OAuth client may request. Existing clients are not silently widened when WebCodex adds new permissions. To change an existing client, submit the complete desired non-empty allow-list to `POST /api/oauth/clients/update_scopes`. A real change invalidates the client's old OAuth grants and requires reauthorization; submitting the same canonical list is a no-op. See [Authentication](AUTH_MODEL.md#oauth2) for the security model.

If ChatGPT MCP host-file import is enabled, configure the exact server-generated OAuth client id in `WEBCODEX_OAUTH2_TRUSTED_MCP_FILE_CLIENT_IDS`. Reprovisioning the client creates a new id, so update this setting as part of that explicit trust rotation. Client display names and redirect URIs are not substitutes for the configured client id.

List and revoke clients with `POST /api/oauth/clients/list` and
`POST /api/oauth/clients/revoke`. OAuth uses the authorization-code flow;
dynamic client registration, OIDC, and the device-code flow are not
implemented. Keep `offline_access` enabled when a host offers it — it is a
protocol-level refresh-token scope and grants no extra WebCodex permission.

## GPT Actions and MCP

- **MCP:** connect a client to `https://your-domain.example/mcp` with a user
  API token (`wc_pat_*`) or, when OAuth is enabled, the OAuth flow.
- **GPT Actions:** import `https://your-domain.example/openapi.json` into a
  Custom GPT with HTTP Bearer authentication.

Both use the same user API token and the same ToolRuntime. The OpenAPI schema
intentionally excludes users, token, pairing/enrollment, setup, doctor, npm,
server-management, and audit endpoints. Use `webcodex` for those tasks.

MCP and GPT Actions are documented in [MCP.md](MCP.md) and the client-specific
setup in [AI Onboarding](AI_ONBOARDING.md).

## Operations

### Authority mode

`WEBCODEX_AUTHORITY_MODE` controls whether consequential runtime tools
auto-execute or require human approval:

| Value | Behavior |
| --- | --- |
| unset / empty | `trusted_agent` (default for self-hosted single-operator deployments). |
| `trusted_agent` | Project work, shell, jobs, git, and validation auto-execute after hard safety checks, with no approval interruptions. Push/tag/publish/release/deploy still require an explicit user task action. |
| `restricted` | Consequential tools are denied unless a human approves them (`webcodex task approve/deny`). |

Hard safety boundaries (project roots, read-only sessions, path policy,
credential redaction, job cancel semantics) are never relaxed by
`trusted_agent`. Legacy `WEBCODEX_PERMISSION_MODE` supports the unambiguous
`dev_auto_approve` → `trusted_agent` and `require_approval` → `restricted` mappings.
Unknown or conflicting old/new settings remain invalid.

### Operator checks

```bash
webcodex ops status --server-url "$SERVER_URL" --token-file "$USER_TOKEN_FILE" --strict
webcodex ops runners --server-url "$SERVER_URL" --token-file "$USER_TOKEN_FILE"
webcodex ops projects --server-url "$SERVER_URL" --token-file "$USER_TOKEN_FILE"
webcodex ops smoke-preflight --server-url "$SERVER_URL" \
  --token-file "$USER_TOKEN_FILE" --project agent:workstation:my-repo
```

`ops` commands are read-only and never print token or env values. `--strict`
makes a FAIL report exit with status 2. `WARN` means worth reviewing but not a
deploy blocker.

### Smoke checks

Recommended production smoke sequence:

1. `webcodex ops status ... --strict` passes.
2. `POST /api/runtime/status` returns `service=webcodex` and the expected
   public URL.
3. `list_runners` shows at least one online Runner.
4. `listProjects` shows `agent:<client_id>:<project_id>` ids.
5. Read-only project tools work on a known project.
6. Write/replace/validate tests are limited to disposable smoke projects.

### Runtime console

The Server serves a host-local browser console at `/console`. It shows project
readiness, the work queue, Workflow Session activity, visible Runners, and recent
mutating activity. For Connector tasks, the same host-local human can send task
guidance, decide pending approvals, cancel work, and Accept or Reject a stable
result. These actions use the same authority boundaries as the CLI; the online
model still cannot accept its own work. The console also shows non-secret client
connection targets, with ChatGPT Developer Mode MCP custom apps as the primary
ChatGPT path. Credentials are deliberately never returned by the console API.

### Runtime job API trust model

`job_status`, `job_log`, `list_jobs`, and `job_tail` are intended for trusted
single-operator deployments. They are not a tenant boundary between mutually
untrusted users. Do not expose one runtime to multiple untrusted users without
adding job-owner isolation; use separate server/runtime instances instead.

## Troubleshooting

See [Troubleshooting](TROUBLESHOOTING.md) for the operational checklist and
common fixes, including existing systemd services, `HTTP reachable: no`,
missing client CLI on `PATH`, server-side pairing vs client-side enrollment,
and `client online: no`.
