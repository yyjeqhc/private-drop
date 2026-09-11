#!/usr/bin/env bash
set -euo pipefail

# Real-process shared-key transport smoke. It starts one temporary Server, one
# direct shared-key Runner, and one managed Runner. All state is loopback-only
# and removed by the EXIT trap.

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$REPO_DIR"

TIMEOUT_SECS="${E2E_TIMEOUT_SECS:-180}"
CARGO_BIN="${CARGO_BIN:-cargo}"
TMP_ROOT=""
SERVER_PID=""
SHARED_PID=""
MANAGED_PID=""
STARTED_AT="$(date +%s)"

log() {
    printf '[shared-key-e2e] %s\n' "$*"
}

die() {
    printf '[shared-key-e2e][FAIL] %s\n' "$*" >&2
    exit 1
}

cleanup() {
    trap - EXIT INT TERM
    for pid in "$SHARED_PID" "$MANAGED_PID" "$SERVER_PID"; do
        if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
            kill "$pid" 2>/dev/null || true
        fi
    done
    for pid in "$SHARED_PID" "$MANAGED_PID" "$SERVER_PID"; do
        if [ -n "$pid" ]; then
            wait "$pid" 2>/dev/null || true
        fi
    done
    if [ -n "$TMP_ROOT" ]; then
        case "$TMP_ROOT" in
            /tmp/webcodex-shared-key-e2e.*)
                rm -rf -- "$TMP_ROOT"
                ;;
            *)
                printf '[shared-key-e2e][WARN] refusing unexpected cleanup path: %s\n' "$TMP_ROOT" >&2
                ;;
        esac
    fi
}
trap cleanup EXIT INT TERM

deadline_ok() {
    [ "$(( $(date +%s) - STARTED_AT ))" -lt "$TIMEOUT_SECS" ]
}

random_secret() {
    python3 -c 'import secrets; print(secrets.token_urlsafe(32))'
}

free_port() {
    python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()'
}

json_field() {
    python3 -c '
import json, sys
value = json.load(sys.stdin)
for component in sys.argv[1].split("."):
    if component:
        value = value[int(component)] if isinstance(value, list) else value.get(component)
        if value is None:
            break
if isinstance(value, (dict, list)):
    print(json.dumps(value, separators=(",", ":")))
elif value is not None:
    print(value)
' "$1"
}

post() {
    local token="$1"
    local path="$2"
    local body="${3:-}"
    if [ -z "$body" ]; then
        body='{}'
    fi
    curl --silent --show-error --max-time 10 \
        -H "Authorization: Bearer ${token}" \
        -H "Content-Type: application/json" \
        -X POST "http://127.0.0.1:${PORT}${path}" \
        -d "$body"
}

wait_for_server() {
    local attempt
    for attempt in $(seq 1 60); do
        deadline_ok || return 1
        if post "$BOOTSTRAP_KEY" /api/runtime/status '{}' >/dev/null 2>&1; then
            return 0
        fi
        sleep 0.25
    done
    return 1
}

wait_for_project() {
    local token="$1"
    local expected="$2"
    local attempt body
    for attempt in $(seq 1 80); do
        deadline_ok || return 1
        body="$(post "$token" /api/projects/list '{}' 2>/dev/null || true)"
        if printf '%s' "$body" | python3 -c '
import json, sys
expected = sys.argv[1]
try:
    data = json.load(sys.stdin)
except Exception:
    raise SystemExit(1)
ids = [item.get("id") for item in data.get("output", {}).get("projects", [])]
raise SystemExit(0 if ids == [expected] else 1)
' "$expected"; then
            return 0
        fi
        sleep 0.25
    done
    return 1
}

if ! command -v curl >/dev/null || ! command -v python3 >/dev/null || ! command -v git >/dev/null; then
    die "curl, python3, and git are required"
fi

log "building Server and Runner binaries"
"$CARGO_BIN" build --quiet \
    -p webcodex --bin webcodex-server \
    -p webcodex-runner --bin webcodex-runner

PORT="$(free_port)"
BOOTSTRAP_KEY="$(random_secret)"
SHARED_KEY_A="shared-a-$(random_secret)"
SHARED_KEY_B="shared-b-$(random_secret)"
TMP_ROOT="$(mktemp -d /tmp/webcodex-shared-key-e2e.XXXXXX)"
DATA_DIR="$TMP_ROOT/data"
SHARED_PROJECT="$TMP_ROOT/shared-project"
MANAGED_PROJECT="$TMP_ROOT/managed-project"
SHARED_PROJECTS_DIR="$TMP_ROOT/shared-project-registry"
MANAGED_PROJECTS_DIR="$TMP_ROOT/managed-project-registry"
mkdir -p "$DATA_DIR" "$SHARED_PROJECT" "$MANAGED_PROJECT" \
    "$SHARED_PROJECTS_DIR" "$MANAGED_PROJECTS_DIR"

for project in "$SHARED_PROJECT" "$MANAGED_PROJECT"; do
    (
        cd "$project"
        git init -q -b main
        git config user.email e2e@example.invalid
        git config user.name "WebCodex E2E"
        printf '# shared-key transport smoke\n' > README.md
        git add README.md
        git commit -q -m init
    )
done

cat >"$SHARED_PROJECTS_DIR/project-a.toml" <<EOF
id = "project-a"
path = "$SHARED_PROJECT"
name = "Shared Project"
allow_patch = true
EOF
cat >"$MANAGED_PROJECTS_DIR/project-m.toml" <<EOF
id = "project-m"
path = "$MANAGED_PROJECT"
name = "Managed Project"
allow_patch = true
EOF

log "starting temporary shared-key-enabled Server"
WEBCODEX_ADDR="127.0.0.1:${PORT}" \
WEBCODEX_DATA="$DATA_DIR" \
WEBCODEX_TOKEN="$BOOTSTRAP_KEY" \
WEBCODEX_SHARED_KEY_ENABLED=true \
RUST_LOG=warn \
"$REPO_DIR/target/debug/webcodex-server" >"$TMP_ROOT/server.log" 2>&1 &
SERVER_PID=$!
wait_for_server || die "Server did not become ready"

post "$BOOTSTRAP_KEY" /api/users/create \
    '{"username":"managed-e2e","display_name":"Managed E2E"}' >/dev/null
MANAGED_PAT_RESPONSE="$(post "$BOOTSTRAP_KEY" /api/tokens/create \
    '{"username":"managed-e2e","name":"e2e","scopes":["runtime:read","project:read","project:write","job:run"]}')"
MANAGED_PAT="$(printf '%s' "$MANAGED_PAT_RESPONSE" | json_field token)"
[ -n "$MANAGED_PAT" ] || die "failed to create managed PAT"
MANAGED_AGENT_RESPONSE="$(post "$BOOTSTRAP_KEY" /api/agent-tokens/create \
    '{"username":"managed-e2e","client_id":"managed-runner","name":"e2e-runner"}')"
MANAGED_AGENT_TOKEN="$(printf '%s' "$MANAGED_AGENT_RESPONSE" | json_field token)"
[ -n "$MANAGED_AGENT_TOKEN" ] || die "failed to create managed Agent token"
unset MANAGED_PAT_RESPONSE MANAGED_AGENT_RESPONSE

cat >"$TMP_ROOT/shared-runner.toml" <<EOF
server_url = "http://127.0.0.1:${PORT}"
token = "$SHARED_KEY_A"
client_id = "shared-runner"
display_name = "Shared Runner"
owner = "must-be-ignored"
project_registry_dir = "$SHARED_PROJECTS_DIR"
transport = "websocket"

[policy]
allow_cwd_anywhere = false
allowed_roots = ["$SHARED_PROJECT"]
allow_raw_shell = true
EOF
cat >"$TMP_ROOT/managed-runner.toml" <<EOF
server_url = "http://127.0.0.1:${PORT}"
token = "$MANAGED_AGENT_TOKEN"
client_id = "managed-runner"
display_name = "Managed Runner"
owner = "managed-e2e"
project_registry_dir = "$MANAGED_PROJECTS_DIR"
transport = "websocket"

[policy]
allow_cwd_anywhere = false
allowed_roots = ["$MANAGED_PROJECT"]
allow_raw_shell = true
EOF
chmod 600 "$TMP_ROOT/shared-runner.toml" "$TMP_ROOT/managed-runner.toml"

"$REPO_DIR/target/debug/webcodex-runner" --config "$TMP_ROOT/shared-runner.toml" \
    >"$TMP_ROOT/shared-runner.log" 2>&1 &
SHARED_PID=$!
"$REPO_DIR/target/debug/webcodex-runner" --config "$TMP_ROOT/managed-runner.toml" \
    >"$TMP_ROOT/managed-runner.log" 2>&1 &
MANAGED_PID=$!

wait_for_project "$SHARED_KEY_A" "agent:shared-runner:project-a" \
    || die "shared-key project did not become visible"
wait_for_project "$MANAGED_PAT" "agent:managed-runner:project-m" \
    || die "managed project did not become visible"
log "same-key and managed projects registered"

READ_RESPONSE="$(post "$SHARED_KEY_A" /api/tools/call \
    '{"tool":"read_files","params":{"project":"agent:shared-runner:project-a","items":[{"path":"README.md"}]}}')"
[ "$(printf '%s' "$READ_RESPONSE" | json_field success)" = "True" ] \
    || die "same-key read_files failed"
printf '%s' "$READ_RESPONSE" | python3 -c '
import json, sys
data = json.load(sys.stdin)
raise SystemExit(0 if "transport smoke" in data.get("output", {}).get("items", [{}])[0].get("output", {}).get("text", "") else 1)
' || die "same-key read_files returned unexpected content"

KEY_B_PROJECTS="$(post "$SHARED_KEY_B" /api/projects/list '{}')"
[ "$(printf '%s' "$KEY_B_PROJECTS" | json_field output.count)" = "0" ] \
    || die "Key B discovered Key A project"
KEY_B_AGENTS="$(post "$SHARED_KEY_B" /api/runtime/status '{}')"
[ "$(printf '%s' "$KEY_B_AGENTS" | json_field output.agents.count)" = "0" ] \
    || die "Key B discovered Key A Runner"

GUESSED_RESPONSE="$(post "$SHARED_KEY_B" /api/projects/git_status \
    '{"project":"agent:shared-runner:project-a"}')"
[ "$(printf '%s' "$GUESSED_RESPONSE" | json_field success)" = "False" ] \
    || die "Key B operated on guessed Key A project id"
[ "$(printf '%s' "$GUESSED_RESPONSE" | json_field output.error_kind)" = "unknown_project" ] \
    || die "guessed project rejection did not preserve non-disclosure"

SHARED_PROJECTS="$(post "$SHARED_KEY_A" /api/projects/list '{}')"
printf '%s' "$SHARED_PROJECTS" | grep -q 'agent:managed-runner:project-m' \
    && die "shared key discovered managed project"
MANAGED_PROJECTS="$(post "$MANAGED_PAT" /api/projects/list '{}')"
printf '%s' "$MANAGED_PROJECTS" | grep -q 'agent:shared-runner:project-a' \
    && die "managed identity discovered shared-key project"
log "cross-key and managed/shared isolation verified"

kill "$SHARED_PID"
wait "$SHARED_PID" 2>/dev/null || true
SHARED_PID=""
for _ in $(seq 1 40); do
    STATUS="$(post "$SHARED_KEY_A" /api/runtime/status '{}' 2>/dev/null || true)"
    if [ "$(printf '%s' "$STATUS" | json_field output.agents.clients.0.connected)" = "False" ]; then
        log "Runner disconnect state verified"
        break
    fi
    sleep 0.25
done
[ "$(printf '%s' "$STATUS" | json_field output.agents.clients.0.connected)" = "False" ] \
    || die "disconnected Runner stayed online"

log "PASS"
