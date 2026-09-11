#!/usr/bin/env bash
set -euo pipefail

if [ "${WEBCODEX_E2E_CLAUDE_PROVIDER:-0}" != "1" ]; then
    printf '[claude-provider-e2e] skipped (set WEBCODEX_E2E_CLAUDE_PROVIDER=1)\n'
    exit 0
fi

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CARGO_BIN="${CARGO_BIN:-cargo}"
CLAUDE_BIN="${WEBCODEX_E2E_CLAUDE_BIN:-claude}"
CLIENT_ID="claude-provider-e2e"
PROJECT_ID="fixture"
RUNTIME_PROJECT="agent:${CLIENT_ID}:${PROJECT_ID}"
TOKEN="webcodex-claude-provider-e2e-only"
TMP_ROOT=""
SERVER_PID=""
RUNNER_PID=""
PASS_COUNT=0

ok() {
    PASS_COUNT=$((PASS_COUNT + 1))
    printf '[claude-provider-e2e][ok] %s\n' "$1"
}

fail() {
    printf '[claude-provider-e2e][FAIL] %s\n' "$1" >&2
    exit 1
}

require_command() {
    command -v "$1" >/dev/null 2>&1 || fail "required command unavailable: $1"
}

stop_process() {
    local pid="$1"
    [ -n "$pid" ] || return 0
    if kill -0 "$pid" 2>/dev/null; then
        kill "$pid" 2>/dev/null || true
        for _ in $(seq 1 50); do
            kill -0 "$pid" 2>/dev/null || break
            sleep 0.1
        done
        if kill -0 "$pid" 2>/dev/null; then
            kill -9 "$pid" 2>/dev/null || true
        fi
    fi
    wait "$pid" 2>/dev/null || true
}

cleanup() {
    stop_process "${RUNNER_PID:-}"
    stop_process "${SERVER_PID:-}"
    if [ -n "${TMP_ROOT:-}" ]; then
        rm -rf "$TMP_ROOT"
    fi
}
trap cleanup EXIT INT TERM

find_port() {
    python3 - <<'PY'
import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()
PY
}

wait_for_port() {
    local port="$1"
    for _ in $(seq 1 300); do
        if (echo >/dev/tcp/127.0.0.1/"$port") 2>/dev/null; then
            return 0
        fi
        sleep 0.1
    done
    return 1
}

api_post() {
    local path="$1"
    local body="$2"
    curl -fsS --max-time 15 \
        -H "Authorization: Bearer ${TOKEN}" \
        -H 'Content-Type: application/json' \
        -X POST "http://127.0.0.1:${PORT}${path}" \
        -d "$body"
}

api_get() {
    local path="$1"
    curl -fsS --max-time 15 \
        -H "Authorization: Bearer ${TOKEN}" \
        "http://127.0.0.1:${PORT}${path}"
}

tool_call() {
    local tool="$1"
    local arguments="$2"
    local body
    body="$(python3 - "$tool" "$arguments" <<'PY'
import json, sys
print(json.dumps({"tool": sys.argv[1], **json.loads(sys.argv[2])}, separators=(",", ":")))
PY
)"
    api_post /api/tools/call "$body"
}

agent_registered() {
    local body
    body="$(api_post /api/runtime/status '{}' 2>/dev/null || true)"
    printf '%s' "$body" | python3 -c '
import json, sys
d = json.load(sys.stdin)
clients = d["output"]["agents"]["clients"]
assert any(c["client_id"] == sys.argv[1] and c["connected"] for c in clients)
' "$CLIENT_ID" >/dev/null 2>&1
}

wait_for_agent() {
    for _ in $(seq 1 300); do
        if agent_registered; then
            return 0
        fi
        sleep 0.1
    done
    return 1
}

provider_status_matches() {
    local provider="$1"
    local fallback="$2"
    local result="$3"
    local error_code="$4"
    local body
    body="$(api_post /api/runtime/status '{}' 2>/dev/null || true)"
    printf '%s' "$body" | python3 -c '
import json, sys
d = json.load(sys.stdin)
client = next(c for c in d["output"]["agents"]["clients"] if c["client_id"] == sys.argv[1])
claude = client["tool_providers"]["claude_code"]
call = claude["last_call"]
assert call["capability"] == "search_project_text"
assert call["selected_provider"] == sys.argv[2]
assert call["fallback_used"] is (sys.argv[3] == "true")
assert call["result"] == sys.argv[4]
expected_code = None if sys.argv[5] == "null" else sys.argv[5]
assert call.get("error_code") == expected_code
assert call.get("write_state") is None
assert claude["process_state"] == "running"
' "$CLIENT_ID" "$provider" "$fallback" "$result" "$error_code" >/dev/null 2>&1
}

wait_for_provider_call() {
    local provider="$1"
    local fallback="$2"
    local result="$3"
    local error_code="$4"
    for _ in $(seq 1 150); do
        if provider_status_matches "$provider" "$fallback" "$result" "$error_code"; then
            return 0
        fi
        sleep 0.1
    done
    return 1
}

write_runner_config() {
    local strategy="$1"
    cat >"$RUNNER_CONFIG" <<EOF
server_url = "http://127.0.0.1:${PORT}"
token = "${TOKEN}"
client_id = "${CLIENT_ID}"
display_name = "Claude Provider E2E"
project_registry_dir = "${PROJECTS_DIR}"
poll_interval_ms = 100
transport = "websocket"

[policy]
allow_raw_shell = true
allow_cwd_anywhere = false
allowed_roots = ["${FIXTURE}"]
max_timeout_secs = 60
max_output_bytes = 262144

[tool_providers]
strategy = "${strategy}"

[tool_providers.claude_code]
enabled = true
command = "${CLAUDE_BIN}"
args = ["mcp", "serve"]
timeout_secs = 30
EOF
}

start_runner() {
    HOME="$ISOLATED_HOME" \
    XDG_CONFIG_HOME="$ISOLATED_HOME/.config" \
    XDG_DATA_HOME="$ISOLATED_HOME/.local/share" \
    XDG_CACHE_HOME="$ISOLATED_HOME/.cache" \
    CLAUDE_CONFIG_DIR="$ISOLATED_HOME/.claude-e2e" \
    RUST_LOG=warn \
        "$ROOT/target/debug/webcodex-runner" --config "$RUNNER_CONFIG" \
        >"$RUNNER_LOG" 2>&1 &
    RUNNER_PID=$!
    wait_for_agent || fail "agent did not register"
}

claude_process_groups() {
    ps -eo ppid=,pgid= | awk -v parent="$RUNNER_PID" '$1 == parent {print $2}' | sort -u
}

assert_groups_gone() {
    local groups="$1"
    local group
    for group in $groups; do
        for _ in $(seq 1 50); do
            if ! kill -0 -- "-${group}" 2>/dev/null; then
                break
            fi
            sleep 0.1
        done
        if kill -0 -- "-${group}" 2>/dev/null; then
            fail "Claude process group remained after agent shutdown"
        fi
    done
}

require_command curl
require_command git
require_command python3
require_command ps
require_command awk
require_command "$CARGO_BIN"
require_command "$CLAUDE_BIN"

cd "$ROOT"
if [ "${WEBCODEX_E2E_SKIP_BUILD:-0}" != "1" ]; then
    "$CARGO_BIN" build --quiet -p webcodex -p webcodex-runner --bins
fi

TMP_ROOT="$(mktemp -d -t webcodex-claude-provider-e2e-XXXXXX)"
PORT="$(find_port)"
DATA_DIR="$TMP_ROOT/data"
PROJECTS_DIR="$TMP_ROOT/project-registry"
FIXTURE="$TMP_ROOT/fixture"
ISOLATED_HOME="$TMP_ROOT/home"
RUNNER_CONFIG="$TMP_ROOT/runner.toml"
SERVER_LOG="$TMP_ROOT/server.log"
RUNNER_LOG="$TMP_ROOT/agent.log"
mkdir -p "$DATA_DIR" "$PROJECTS_DIR" "$FIXTURE" \
    "$ISOLATED_HOME/.config" "$ISOLATED_HOME/.local/share" \
    "$ISOLATED_HOME/.cache" "$ISOLATED_HOME/.claude-e2e"

git -C "$FIXTURE" init -b main >/dev/null
git -C "$FIXTURE" config user.email e2e@example.invalid
git -C "$FIXTURE" config user.name 'WebCodex E2E'
printf 'before\nneedle\n' >"$FIXTURE/fixture.txt"
git -C "$FIXTURE" add fixture.txt
git -C "$FIXTURE" commit -m fixture >/dev/null

cat >"$PROJECTS_DIR/${PROJECT_ID}.toml" <<EOF
id = "${PROJECT_ID}"
path = "${FIXTURE}"
name = "Claude Provider Fixture"
allow_patch = true
kind = "text"
EOF

HOME="$ISOLATED_HOME" WEBCODEX_ADDR="127.0.0.1:${PORT}" \
WEBCODEX_DATA="$DATA_DIR" WEBCODEX_TOKEN="$TOKEN" RUST_LOG=warn \
    "$ROOT/target/debug/webcodex-server" >"$SERVER_LOG" 2>&1 &
SERVER_PID=$!
wait_for_port "$PORT" || fail "server port did not open"
ok "isolated server started"

write_runner_config claude_code_then_native
start_runner
ok "fallback-strategy agent registered"

TOOLS_BEFORE="$TMP_ROOT/tools-before.json"
api_post /mcp '{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}' >"$TOOLS_BEFORE"
python3 - "$TOOLS_BEFORE" <<'PY' || fail "public MCP tools exposed Claude internals or stale edit tools"
import json, sys
with open(sys.argv[1], encoding="utf-8") as stream:
    names = {t["name"] for t in json.load(stream)["result"]["tools"]}
assert {"read_files", "search_project_texts"} <= names
# replace_in_file was removed entirely and must never re-enter the surface.
assert "replace_in_file" not in names
assert not ({"Edit", "Read", "Bash", "Write", "NotebookEdit", "Agent"} & names)
PY
ok "public MCP tools exclude Claude internals and removed edit tools"

api_get /openapi.json | python3 -c '
import json, sys
d = json.load(sys.stdin)
count = sum(len(v) for v in d["paths"].values())
assert 0 < count < 30, count
' || fail "OpenAPI operation count exceeded GPT Actions bound"
ok "OpenAPI operation count remains below GPT Actions limit"

READ_ARGS="$(python3 - "$RUNTIME_PROJECT" <<'PY'
import json, sys
print(json.dumps({"project": sys.argv[1], "items": [{"path": "fixture.txt"}]}))
PY
)"
tool_call read_files "$READ_ARGS" | python3 -c '
import json, sys
d = json.load(sys.stdin)
item = d["output"]["items"][0]
assert d["success"] and item["success"] and "before" in item["output"]["text"]
' || fail "Native read failed"
api_post /api/runtime/status '{}' | python3 -c '
import json, sys
d = json.load(sys.stdin)
c = next(x for x in d["output"]["agents"]["clients"] if x["client_id"] == sys.argv[1])
claude = c["tool_providers"]["claude_code"]
assert claude["process_state"] == "not_started"
assert claude.get("last_call") is None
' "$CLIENT_ID" || fail "read_files started Claude"
ok "read_files stayed Native without starting Claude"

SEARCH_ARGS="$(python3 - "$RUNTIME_PROJECT" <<'PY'
import json, sys
print(json.dumps({"project": sys.argv[1], "queries": [{"pattern": "needle", "path": "."}]}))
PY
)"
tool_call search_project_texts "$SEARCH_ARGS" | python3 -c '
import json, sys
d = json.load(sys.stdin)
item = d["output"]["items"][0]
assert d["success"] and item["success"]
assert item["output"]["backend"] in ("rg", "grep")
' || fail "Native search fallback failed"
wait_for_provider_call native true success null || fail "search fallback evidence did not propagate"
ok "search fallback recorded selected_provider=native"

TOOLS_AFTER="$TMP_ROOT/tools-after.json"
api_post /mcp '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}' >"$TOOLS_AFTER"
python3 - "$TOOLS_BEFORE" "$TOOLS_AFTER" <<'PY' || fail "provider discovery changed public tools"
import json, sys
def names(path):
    with open(path, encoding="utf-8") as stream:
        return sorted(t["name"] for t in json.load(stream)["result"]["tools"])
assert names(sys.argv[1]) == names(sys.argv[2])
PY
ok "public MCP tool set stayed identical after Claude discovery"

FIRST_GROUPS="$(claude_process_groups)"
[ -n "$FIRST_GROUPS" ] || fail "running Claude process group was not observable"
stop_process "$RUNNER_PID"
RUNNER_PID=""
assert_groups_gone "$FIRST_GROUPS"
ok "fallback-strategy Claude process group reaped"

write_runner_config claude_code
start_runner
ok "strict Claude agent registered"

# Strict `claude_code` cannot map a compatible search capability (Claude Code
# builds do not necessarily expose a Grep), so a one-query search_project_texts
# batch must preserve the deterministic provider capability error on that item
# instead of routing through Claude Edit or Bash. No file writes are routed to
# the provider; batch item failures remain isolated inside a successful batch.
SEARCH_STRICT_BODY="$(python3 - "$RUNTIME_PROJECT" <<'PY'
import json, sys
print(json.dumps({
    "tool": "search_project_texts",
    "params": {
        "project": sys.argv[1],
        "queries": [{"pattern": "needle", "path": "."}],
    },
}))
PY
)"
STRICT_RESPONSE="$(curl -sS --max-time 15 \
    -H "Authorization: Bearer ${TOKEN}" \
    -H 'Content-Type: application/json' \
    -X POST "http://127.0.0.1:${PORT}/api/tools/call" \
    -d "$SEARCH_STRICT_BODY" || true)"
printf '%s' "$STRICT_RESPONSE" | python3 -c '
import json, sys
d = json.load(sys.stdin)
assert d["success"]
items = (d.get("output") or {}).get("items") or []
assert len(items) == 1 and not items[0]["success"]
out = items[0].get("output") or {}
assert out.get("format") == "webcodex.external_provider_error.v1"
assert out.get("code") == "provider_capability_unavailable"
' || fail "strict Claude search did not surface a deterministic capability error"
wait_for_provider_call claude_code false failure provider_capability_unavailable \
    || fail "strict capability-error evidence did not propagate"
grep -q '^before$' "$FIXTURE/fixture.txt" || fail "strict provider call modified the fixture"
[ -z "$(git -C "$FIXTURE" status --porcelain)" ] || fail "strict fixture worktree is dirty"
ok "strict search recorded Claude, no fallback, no write, capability error"

SECOND_GROUPS="$(claude_process_groups)"
[ -n "$SECOND_GROUPS" ] || fail "strict Claude process group was not observable"
stop_process "$RUNNER_PID"
RUNNER_PID=""
assert_groups_gone "$SECOND_GROUPS"
ok "strict Claude process group reaped"

stop_process "$SERVER_PID"
SERVER_PID=""
for _ in $(seq 1 50); do
    if ! (echo >/dev/tcp/127.0.0.1/"$PORT") 2>/dev/null; then
        ok "server port released"
        printf '[claude-provider-e2e] passed checks=%s\n' "$PASS_COUNT"
        exit 0
    fi
    sleep 0.1
done
fail "server port remained open"
