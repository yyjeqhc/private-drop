# Private Drop

A self-hosted private long-text/file drop box. Replace QQ/WeChat's "File Transfer Assistant" with your own service.

## Features

- Token authentication (Bearer token or query param)
- 6 default channels: inbox, xline, thesis, packfix, omo, files
- Send long text (up to 2MB per message)
- Upload files (up to 100MB)
- Mobile-friendly web UI (server-rendered HTML)
- REST API with OpenAPI 3.0 spec
- SQLite storage (bundled, no system dependency)
- GPT Actions compatible
- File download with Content-Disposition header
- Path traversal protection

## Quick Start

### 1. Build

```bash
cargo build --release
```

### 2. Run

For local development you can still pass environment variables directly:

```bash
DROP_TOKEN="your-secret-token" ./target/release/private-drop
```

For deployment, put settings in an env file so startup does not require a long inline command:

```bash
cat > /opt/private-drop/private-drop.env << 'EOF'
RUST_LOG=info,codex.metrics=info
DROP_TOKEN=your-secret-token
DROP_ADDR=127.0.0.1:8080
DROP_DATA=/var/lib/private-drop
PROJECTS_CONFIG=/opt/private-drop/projects.toml
DROP_PUBLIC_URL=https://example.com
EOF

./private-drop
```

By default private-drop loads env files from:

1. `./private-drop.env`
2. `/opt/private-drop/private-drop.env`
3. `/etc/private-drop/private-drop.env`

Set `DROP_ENV_FILE=/path/to/file` to load one explicit file instead.
Existing process environment variables take precedence over env-file values.

### 3. Access

- Web UI: http://localhost:8080
- API: http://localhost:8080/api
- OpenAPI: http://localhost:8080/openapi.json

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `DROP_ADDR` | `0.0.0.0:8080` | Listen address |
| `DROP_DATA` | `./data` | Data directory (SQLite DB + uploads) |
| `DROP_TOKEN` | (none) | Auth token. **Required for production.** If unset, runs in dev mode with warning. |
| `DROP_ENV_FILE` | (none) | Optional explicit env file to load before reading other settings. |

## API Examples

### Health Check (no auth)

```bash
curl http://localhost:8080/api/health
```

### Send Text Message

```bash
curl -X POST http://localhost:8080/api/messages \
  -H "Authorization: Bearer your-secret-token" \
  -H "Content-Type: application/json" \
  -d '{"channel": "inbox", "title": "My Note", "text": "Hello, this is a long text..."}'
```

### Send 10K Text

```bash
python3 -c "print('A' * 10240)" | \
curl -X POST http://localhost:8080/api/messages \
  -H "Authorization: Bearer your-secret-token" \
  -H "Content-Type: application/json" \
  -d @- --json '{"channel":"inbox","title":"Long","text":"PLACEHOLDER"}'
```

Or inline:

```bash
LONG=$(python3 -c "print('A' * 10240)")
curl -X POST http://localhost:8080/api/messages \
  -H "Authorization: Bearer your-secret-token" \
  -H "Content-Type: application/json" \
  -d "{\"channel\":\"inbox\",\"title\":\"10K Text\",\"text\":\"$LONG\"}"
```

### List Messages

```bash
curl -H "Authorization: Bearer your-secret-token" \
  "http://localhost:8080/api/messages?channel=inbox&limit=10"
```

### Get Message

```bash
curl -H "Authorization: Bearer your-secret-token" \
  http://localhost:8080/api/messages/{id}
```

### Delete Message

```bash
curl -X DELETE -H "Authorization: Bearer your-secret-token" \
  http://localhost:8080/api/messages/{id}
```

### Upload File

```bash
curl -X POST \
  -H "Authorization: Bearer your-secret-token" \
  -F "file=@myfile.pdf" \
  "http://localhost:8080/api/files?channel=files"
```

### Download File

```bash
curl -H "Authorization: Bearer your-secret-token" \
  http://localhost:8080/api/files/{id} -o downloaded.file
```

## Web UI

The web UI is server-rendered HTML, mobile-friendly, no JavaScript framework required.

- `GET /` - Home page with channel list and recent messages
- `GET /c/{channel}` - Channel messages (e.g., `/c/inbox`)
- `GET /m/{id}` - Message detail
- `GET /send` - Send text / upload file form

Web UI auth: pass `?token=your-secret-token` in the URL, or use the login page which stores the token in localStorage.

## GPT Actions Setup

1. Deploy Private Drop with HTTPS (see Deployment section below)
2. In ChatGPT, create a GPT with Actions
3. Import your OpenAPI spec URL: `https://your-server/openapi.json`
4. Set authentication to "API Key" / "Bearer" with your `DROP_TOKEN`
5. Key operations for GPT:
   - `createMessage` - Send text to any channel
   - `listMessages` - Read messages from channels
   - `getMessage` - Get specific message details
   - `deleteMessage` - Delete a message

## Channels

Default channels for organizing messages:

- **inbox** - General inbox
- **xline** - Xline related
- **thesis** - Thesis work
- **packfix** - Packfix tasks
- **omo** - OMO items
- **files** - File uploads

## Deployment

### Systemd Service

Example service file: `deploy/private-drop.service.example`

```bash
# Create user
sudo useradd -r -s /usr/sbin/nologin private-drop

# Copy binary
sudo mkdir -p /opt/private-drop
sudo cp target/release/private-drop /opt/private-drop/
sudo chown -R private-drop:private-drop /opt/private-drop

# Install service
sudo cp deploy/private-drop.service.example /etc/systemd/system/private-drop.service
# Edit DROP_TOKEN in the service file
sudo systemctl daemon-reload
sudo systemctl enable --now private-drop
```

### Nginx Reverse Proxy

Example config: `deploy/nginx.example.conf`

```bash
# Install nginx and certbot
sudo apt install nginx certbot python3-certbot-nginx

# Copy config
sudo cp deploy/nginx.example.conf /etc/nginx/sites-available/private-drop
# Edit server_name
sudo ln -s /etc/nginx/sites-available/private-drop /etc/nginx/sites-enabled/
sudo nginx -t && sudo systemctl reload nginx

# Get SSL cert
sudo certbot --nginx -d drop.example.com
```

## E2E Testing

Run the automated end-to-end smoke test:

```bash
bash scripts/e2e_test.sh
```

This script:
- Builds the project (`cargo fmt`, `cargo test`, `cargo build --release`)
- Starts the server on a random free port with a temporary data directory
- Runs 47+ test cases covering:
  - Health check endpoint
  - Token authentication (401 on missing/wrong token)
  - Create, list, get, delete text messages
  - 10K and 100K long text messages
  - File upload and download with content verification
  - Content-Disposition header on file download
  - OpenAPI spec validation
  - Channel listing
  - Web UI pages: home, channel, message detail, send
- Cleans up the server and temporary data on exit

## Unit Tests

```bash
cargo test
```

37 unit tests covering UUID generation, config defaults, token validation, filename sanitization, message serialization, SSH path validation, edit path validation, replace_nth, replace_line_range, shell escape safety, and remote edit script execution.

## Codex-like GPT Actions API

Private Drop includes a set of coarse-grained APIs designed for ChatGPT GPT Actions to operate on whitelisted projects. These APIs enable GPT to act as a "Codex brain" — observing projects, generating patches, running checks, and writing reports.

### Why 4 Coarse Interfaces?

Instead of exposing fine-grained dangerous APIs (readFile, writeFile, runShell, deleteFile), we expose 4 coarse operations:

1. **getProjectContext** — Read-only observation (overview, tree, search, read_file, git_status, git_diff)
2. **applyProjectPatch** — Apply a unified diff to a whitelisted project
3. **runProjectCheck** — Run pre-configured check commands (fmt, test, build, e2e, full)
4. **writeProjectReport** — Write operation reports and post messages to channels

This design prevents arbitrary shell access, arbitrary file I/O, and git push while still giving GPT enough capability to complete code review and fix workflows.

### Configuration

Create `projects.toml` (or set `PROJECTS_CONFIG` env var to its path):

```toml
[projects.private-drop]
path = "/root/git/private-drop"
allow_patch = true
allowed_checks = ["fmt", "test", "build", "e2e", "full"]

[projects.private-drop.checks]
fmt = "cargo fmt --check"
test = "cargo test"
build = "cargo build --release"
e2e = "bash scripts/e2e_test.sh"
full = "cargo fmt --check && cargo test && cargo build --release && bash scripts/e2e_test.sh"
```

If `projects.toml` is missing, the Codex API returns clear errors but the original message/file APIs remain fully functional.

### SSH Executor (Remote Projects)

When private-drop is deployed on a gateway machine (e.g., SG4 as a public HTTPS entry point) but the actual project lives on a different machine (reachable via Tailscale), use the SSH executor:

```toml
[projects.private-drop]
executor = "ssh"
host = "msi"
user = "root"
path = "/root/git/private-drop"
allow_patch = true
allowed_checks = ["fmt", "test", "build", "e2e"]

[projects.private-drop.checks]
fmt = "cargo fmt --check"
test = "cargo test"
build = "cargo build --release"
e2e = "bash scripts/e2e_test.sh"
```

**Architecture:**
- SG4 runs private-drop with HTTPS (public entry point)
- SSH commands go to the remote machine (e.g., `msi` via Tailscale)
- All Codex API operations (context, apply_patch, check) execute on the remote machine
- Reports are written locally on SG4

**How it works:**
- `executor = "ssh"`: Commands run via `ssh <host> -- <remote command>`
- `host`: SSH hostname (from `projects.toml`, not user input)
- `user`: SSH user (optional, defaults to current user)
- `path`: Project root on the remote machine
- All path safety checks (no `..`, no absolute paths, no sensitive files) are enforced before SSH commands are constructed
- Patches are piped via SSH stdin to a temp file on the remote machine
- **Edit operations** (`applyProjectEdit`) are executed via an embedded Python3 script on the remote host. Edit JSON is piped via stdin; the remote python3 script validates paths and performs edits. Requires `python3` on the remote host.

**Before using SSH executor, verify connectivity:**
```bash
ssh msi 'cd /root/git/private-drop && cargo test'
```

### API Examples

```bash
# Get project overview
curl -X POST http://localhost:8080/api/codex/context \
  -H "Authorization: Bearer your-secret-token" \
  -H "Content-Type: application/json" \
  -d '{"project":"private-drop","mode":"overview"}'

# Search for code
curl -X POST http://localhost:8080/api/codex/context \
  -H "Authorization: Bearer your-secret-token" \
  -H "Content-Type: application/json" \
  -d '{"project":"private-drop","mode":"search","query":"fn main"}'

# Apply a patch
curl -X POST http://localhost:8080/api/codex/apply_patch \
  -H "Authorization: Bearer your-secret-token" \
  -H "Content-Type: application/json" \
  -d '{"project":"private-drop","patch":"diff --git a/...","reason":"fix bug"}'

# Run checks
curl -X POST http://localhost:8080/api/codex/check \
  -H "Authorization: Bearer your-secret-token" \
  -H "Content-Type: application/json" \
  -d '{"project":"private-drop","suite":"test"}'

# Write a report
curl -X POST http://localhost:8080/api/codex/report \
  -H "Authorization: Bearer your-secret-token" \
  -H "Content-Type: application/json" \
  -d '{"project":"private-drop","status":"completed","title":"Fix X","summary":"Changed Y, tests pass","channel":"omo"}'
```

### GPT Actions Setup

1. Import `https://your-server/openapi.json` into your GPT Actions
2. Set authentication to API Key / Bearer with your `DROP_TOKEN`
3. GPT can use the `codex` operationIds: `getProjectContext`, `applyProjectEdit`, `applyProjectPatch`, `runProjectCheck`, `writeProjectReport`

### Codex Edit API (applyProjectEdit)

`applyProjectEdit` is the recommended way for GPT to make small, targeted file changes. Unlike `applyProjectPatch` (which requires a full unified diff), `applyProjectEdit` accepts structured JSON edit operations that are easier for GPT to generate correctly.

**Why use applyProjectEdit over applyProjectPatch?**

| Feature | applyProjectEdit | applyProjectPatch |
|---------|------------------|-------------------|
| Input format | Structured JSON operations | Unified diff text |
| Error-prone? | Low — field names are explicit | High — diff format is fragile |
| Supports dry_run | Yes | No |
| Best for | Small targeted changes (1-10 files) | Large refactors, bulk changes |
| GPT Actions friendly | Yes — explicit schema with oneOf | Requires diff generation skill |

**Edit operation types:**

#### Quick examples

Preview a change without writing files by setting `dry_run=true`:

```json
{
  "project": "private-drop-v4",
  "dry_run": true,
  "edits": [{
    "type": "replace_text",
    "path": "README.md",
    "old_text": "old text",
    "new_text": "new text"
  }]
}
```

Replace a unique text snippet in an existing file:

```json
{
  "project": "private-drop-v4",
  "edits": [{
    "type": "replace_text",
    "path": "src/main.rs",
    "old_text": "Router::new()",
    "new_text": "Router::new().hoop(logging)"
  }]
}
```

Append text to an existing file:

```json
{
  "project": "private-drop-v4",
  "edits": [{
    "type": "append_file",
    "path": "README.md",
    "text": "\n## Notes\n\nNew note added by GPT.\n"
  }]
}
```

#### replace_text — Find and replace text in an existing file

```bash
curl -X POST http://localhost:8080/api/codex/edit \
  -H "Authorization: Bearer your-secret-token" \
  -H "Content-Type: application/json" \
  -d '{
    "project": "my-project",
    "reason": "fix typo",
    "edits": [{
      "type": "replace_text",
      "path": "src/main.rs",
      "old_text": "println!(\"hello\");",
      "new_text": "println!(\"Hello, world!\");"
    }]
  }'
```

If `old_text` appears multiple times, specify `occurrence` (1-based):

```json
{
  "type": "replace_text",
  "path": "config.toml",
  "old_text": "localhost",
  "new_text": "0.0.0.0",
  "occurrence": 2
}
```

#### replace_range — Replace a range of lines

Replace lines 5-10 (inclusive, 1-based) with new content:

```bash
curl -X POST http://localhost:8080/api/codex/edit \
  -H "Authorization: Bearer your-secret-token" \
  -H "Content-Type: application/json" \
  -d '{
    "project": "my-project",
    "edits": [{
      "type": "replace_range",
      "path": "src/lib.rs",
      "start_line": 5,
      "end_line": 10,
      "new_text": "fn new_function() -> i32 {\n    42\n}\n"
    }]
  }'
```

#### append_file — Append text to an existing file

```bash
curl -X POST http://localhost:8080/api/codex/edit \
  -H "Authorization: Bearer your-secret-token" \
  -H "Content-Type: application/json" \
  -d '{
    "project": "my-project",
    "edits": [{
      "type": "append_file",
      "path": "TODO.md",
      "text": "\n- [ ] New task added by GPT\n"
    }]
  }'
```

#### create_file — Create a new file (fails if it already exists)

```bash
curl -X POST http://localhost:8080/api/codex/edit \
  -H "Authorization: Bearer your-secret-token" \
  -H "Content-Type: application/json" \
  -d '{
    "project": "my-project",
    "edits": [{
      "type": "create_file",
      "path": "src/utils.rs",
      "content": "pub fn add(a: i32, b: i32) -> i32 { a + b }\n"
    }]
  }'
```

#### write_file — Write full file content (use sparingly)

```bash
curl -X POST http://localhost:8080/api/codex/edit \
  -H "Authorization: Bearer your-secret-token" \
  -H "Content-Type: application/json" \
  -d '{
    "project": "my-project",
    "edits": [{
      "type": "write_file",
      "path": "config.json",
      "content": "{\"key\": \"value\"}\n",
      "allow_overwrite": true
    }]
  }'
```

> **Note:** `write_file` with `allow_overwrite=true` replaces the entire file. Prefer `replace_text` or `replace_range` for partial edits.

#### dry_run — Preview changes without writing

```bash
curl -X POST http://localhost:8080/api/codex/edit \
  -H "Authorization: Bearer your-secret-token" \
  -H "Content-Type: application/json" \
  -d '{
    "project": "my-project",
    "dry_run": true,
    "edits": [{
      "type": "replace_text",
      "path": "src/main.rs",
      "old_text": "old_code()",
      "new_text": "new_code()"
    }]
  }'
```

Returns the same response with a `diff` field showing what would change, but does not modify any files.

#### Security limits

- **Path validation**: No absolute paths, no `..` traversal, no `.git/`, `.env`, `*.pem`, `*.key`, `id_rsa`, `target/`, `node_modules/`
- **File size**: Max 2MB per file (read), max 200KB per edit text
- **Project whitelist**: Only projects in `projects.toml` with `allow_patch = true`
- **UTF-8 only**: Binary files are rejected

#### GPT Actions recommendation

**Prefer `applyProjectEdit` for most code changes.** Use `applyProjectPatch` only when:
- You need to change many files in a coordinated way
- The change is a large refactor that's easier expressed as a diff
- You're applying an externally generated patch

For simple edits (fix a bug, add a function, update a config value), `applyProjectEdit` with `replace_text` or `replace_range` is more reliable and less error-prone.

### Security Boundaries

- **No arbitrary shell**: Only pre-configured check commands from `projects.toml`
- **No git push**: Only `git apply` for patches, no commit or push
- **No arbitrary path access**: All paths are canonicalized and verified to be within project root
- **Sensitive files blocked**: `.git/`, `.env`, `*.pem`, `*.key`, `id_rsa`, `target/`, `node_modules/` cannot be modified
- **Output truncated**: All outputs capped at 50K characters to avoid overwhelming GPT Actions
- **Project whitelist only**: Only projects listed in `projects.toml` are accessible

## Security Notes

- Always set `DROP_TOKEN` in production
- Use HTTPS in production (via nginx reverse proxy)
- Token is required for all API and file download operations
- File paths are randomized (UUID) to prevent guessing
- Path traversal is prevented (canonicalized paths checked against uploads dir)
- Filename in Content-Disposition is sanitized (no path separators)
- Request body size limited to 2MB for text, 100MB for files

## License

MIT


## SSH executor performance tuning

Projects using `executor = "ssh"` can optionally enable SSH connection reuse in `projects.toml`.
This reduces the repeated SSH handshake cost for Codex API calls such as `overview`, `read_file`,
`search`, `check`, and `edit`.

Example global SSH configuration:

```toml
[ssh]
batch_mode = true
connect_timeout_secs = 10
control_master = true
control_persist = "10m"
control_path = "/tmp/private-drop-ssh-%C"
server_alive_interval = 30
server_alive_count_max = 3
```

When `control_master = true`, private-drop adds SSH options equivalent to:

```text
-o BatchMode=yes
-o ConnectTimeout=10
-o ControlMaster=auto
-o ControlPersist=10m
-o ControlPath=/tmp/private-drop-ssh-%C
-o ServerAliveInterval=30
-o ServerAliveCountMax=3
```

`BatchMode=yes` is also added when `batch_mode = true`, even if ControlMaster is disabled.
All SSH options are passed as separate `std::process::Command` arguments; user request text is
not interpolated into SSH options.

If reused SSH connections behave unexpectedly, remove stale sockets with:

```sh
rm -f /tmp/private-drop-ssh-*
```

Alternatively set `control_master = false` or remove the `[ssh]` section to return to the
previous one-SSH-process-per-call behavior.

For SSH projects, `getProjectContext(mode="overview")` is batched into one SSH call that gathers
the branch, `git status --short`, allowed checks, and important-file presence in a single remote
command.


## Codex API performance tracing

Codex operations emit lightweight structured tracing logs under the `codex.metrics` target. These logs are intended to help compare SSH executor latency before and after connection reuse or batching changes without changing the public JSON response schema.

Examples of logged fields include:

- `operation`: `getProjectContext`, `runProjectCheck`, or `applyProjectEdit`
- `project`
- `mode` or `suite` where applicable
- `executor`: `local` or `ssh`
- `success`
- `duration_ms`
- `ssh_calls`
- `control_master`

For SSH projects, `getProjectContext(mode="overview")` should report `ssh_calls=1` because overview is batched into a single remote command. `read_file`, `search`, `git_status`, `git_diff`, `check`, and `edit` also log their per-request duration so you can identify remaining slow operations.


## Codex context batch API

`POST /api/codex/context_batch` runs multiple read-only context observations for one project in a single authenticated Codex API call.

Example:

```json
{
  "project": "private-drop-v4",
  "requests": [
    {"mode": "overview"},
    {"mode": "git_status"},
    {"mode": "read_file", "path": "README.md", "start_line": 1, "limit": 40}
  ]
}
```

The response contains `results`, one normal context response per request, plus `duration_ms` and `ssh_calls`. Batches are limited to 20 items. This reduces GPT Action round trips; SSH projects still execute one remote command per item, but reuse ControlMaster connections when configured.


## Codex controlled Git API

`POST /api/codex/git` exposes a small fixed set of Git operations without opening an arbitrary shell. It is intended for safe Codex maintenance tasks such as checking status or amending the current commit after tests pass.

Supported operations:

- `status`: runs `git status --short`
- `diff`: runs `git diff`, optionally restricted to `paths`
- `log`: runs `git log --oneline -n 20`
- `add`: runs `git add -- <paths>`; requires project patch permission
- `commit_amend_no_edit`: runs `git add -- <paths> && git commit --amend --no-edit --no-verify`; requires project patch permission

Example:

```json
{
  "project": "private-drop-v4",
  "operation": "commit_amend_no_edit",
  "paths": ["src/codex.rs"]
}
```

Paths are relative to the project root and use the same sensitive path checks as the edit API. `.env`, `.git`, `target`, `node_modules`, private key paths, absolute paths, and `..` traversal are rejected. The API does not accept arbitrary Git subcommands or shell snippets.


## Codex whitelisted command API

`POST /api/codex/command` runs a project-level command configured in `projects.toml` under `[projects.<name>.commands]`.
The request only supplies a command id; it cannot submit arbitrary shell text.

Example configuration:

```toml
[projects.private-drop-v4.commands]
clippy = "cargo clippy --all-targets -- -D warnings"
doc = "cargo doc --no-deps"
```

Example request:

```json
{
  "project": "private-drop-v4",
  "command": "clippy"
}
```

The response includes `success`, `exit_code`, `duration_ms`, `stdout_tail`, `stderr_tail`, and `truncated`. SSH projects use the existing SSH executor path and therefore benefit from configured ControlMaster reuse.

Command ids may only contain ASCII letters, digits, `_`, `-`, and `.` and must be configured in `projects.toml`. This endpoint is intended for project-specific checks such as `clippy`, `doc`, `pytest`, `lint`, or smoke tests while avoiding arbitrary command execution from API requests.


## Codex chat-approved command requests

`POST /api/codex/command_request` creates an audited pending command request for a command id configured in `[projects.<name>.commands]`. It does not execute the command.

Project opt-in is required:

```toml
[projects.private-drop-v4]
allow_command_requests = true

[projects.private-drop-v4.commands]
smoke = "cargo test smoke"
```

Create a request:

```json
{
  "project": "private-drop-v4",
  "command": "smoke",
  "reason": "Need to verify smoke tests before deploy"
}
```

The response contains a `request_id` and an audit `record` with `status = "pending"`. After approval in chat, execute the exact configured command id with:

```json
{
  "request_id": "<id from command_request>"
}
```

sent to `POST /api/codex/command_approve`.

The server records status, timestamps, exit code, stdout/stderr tails, and error in SQLite. A request can only be approved while pending, so repeated approval attempts do not re-run the command. Approval atomically claims a pending request by moving it to `running` before execution, preventing concurrent double execution. Approval executes the stored `command_text` snapshot captured when the request was created, not a later value from a changed `projects.toml`. `reason` is limited to 2000 characters. This is a chat-friendly approval flow with server-side audit records, not an arbitrary shell endpoint.
