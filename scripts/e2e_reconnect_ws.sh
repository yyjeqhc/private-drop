#!/usr/bin/env bash
set -euo pipefail

# ============================================================================
# WebCodex — Runner/Server Restart & Reconnect Continuity E2E
#
# Real-process integration harness for Iteration 9 Phase 2:
#   1. Boots a real `webcodex-server` and `webcodex-runner` (WebSocket).
#   2. Verifies the layered connection observations (runner_process /
#      server_transport / server_registration / project_registry /
#      connector_endpoint / last_successful_tool_call) carry the full
#      observation contract, plus version_compatibility and the runner-reported
#      shell profile dialect.
#   3. Creates a durable coding-task session.
#   4. Kills the runner: layers must degrade independently (stale
#      registration is never reported ready) and a reconciliation-capable
#      running job must enter queryable `recovering`, not fake success/loss.
#   5. Restarts the runner: a NEW connection instance must replace the old
#      one, fence the old job to `lost` with `runner_instance_replaced`, the
#      project must re-register, and calls must recover WITHOUT a server restart.
#   6. Restarts the server: the runner must auto-reconnect and the durable
#      session must remain resumable only through its explicit session_id.
#   7. Post-deploy smoke facts: server version/commit, authority mode,
#      version_compatibility status.
#
# Environment overrides: E2E_PORT, E2E_TOKEN, E2E_TIMEOUT_SECS,
# E2E_SKIP_RUN=1 (syntax check only), CARGO_BIN.
# Exit codes: 0 pass, 1 failures, 2 environment error.
# ============================================================================

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$PROJECT_DIR"

CARGO_BIN="${CARGO_BIN:-cargo}"
TOKEN="${E2E_TOKEN:-e2e-reconnect-token}"
CLIENT_ID="e2e-reconnect-agent"
PROJECT_ID="reconnect-proj"
TIMEOUT_SECS="${E2E_TIMEOUT_SECS:-420}"
RUNTIME_PROJECT_ID="agent:${CLIENT_ID}:${PROJECT_ID}"

PASS=0
FAIL=0
SERVER_PID=""
RUNNER_PID=""
TMP_ROOT=""
COOKIE_JAR=""
START_EPOCH=$(date +%s)

log() { printf '[reconnect-e2e] %s\n' "$*"; }
fail() { FAIL=$((FAIL + 1)); printf '[reconnect-e2e][FAIL] %s\n' "$*" >&2; }
pass() { PASS=$((PASS + 1)); printf '[reconnect-e2e][ok]   %s\n' "$*"; }

check_deadline() {
    if [ $(( $(date +%s) - START_EPOCH )) -ge "$TIMEOUT_SECS" ]; then
        fail "overall timeout (${TIMEOUT_SECS}s) exceeded"
        exit 1
    fi
}

find_free_port() {
    python3 -c "
import socket
s = socket.socket()
s.bind(('127.0.0.1', 0))
print(s.getsockname()[1])
s.close()
"
}

api_post() {
    local path="$1"; local body="${2:-}"
    if [ -z "$body" ]; then body="{}"; fi
    curl -sS --max-time 15 \
        -c "$COOKIE_JAR" -b "$COOKIE_JAR" \
        -H "Authorization: Bearer ${TOKEN}" \
        -H "Content-Type: application/json" \
        -X POST "http://127.0.0.1:${PORT}${path}" \
        -d "$body" 2>/dev/null
}

observe_job() {
    local job_id="$1"
    api_post /api/tools/call "{\"tool\":\"observe_jobs\",\"params\":{\"items\":[{\"job_id\":\"${job_id}\"}],\"tail_lines\":1}}"
}

json_get() {
    local json="$1"; local path="$2"
    python3 - "$json" "$path" <<'PY'
import json, sys
try:
    obj = json.loads(sys.argv[1])
except Exception:
    print("")
    sys.exit(0)
cur = obj
for part in sys.argv[2].split("."):
    if part == "":
        break
    if isinstance(cur, list):
        try:
            cur = cur[int(part)]
        except Exception:
            print(""); sys.exit(0)
    elif isinstance(cur, dict):
        cur = cur.get(part)
        if cur is None:
            print(""); sys.exit(0)
    else:
        print(""); sys.exit(0)
if isinstance(cur, (dict, list)):
    print(json.dumps(cur))
else:
    print(cur if cur is not None else "")
PY
}

assert_eq() {
    local label="$1"; local actual="$2"; local expected="$3"
    if [ "$actual" = "$expected" ]; then
        pass "$label"
    else
        fail "$label (expected '$expected', got '$actual')"
    fi
}

assert_nonempty() {
    local label="$1"; local actual="$2"
    if [ -n "$actual" ]; then
        pass "$label"
    else
        fail "$label (empty value)"
    fi
}

cleanup() {
    trap - INT TERM EXIT
    for pid in "${RUNNER_PID:-}" "${SERVER_PID:-}"; do
        if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
            kill "$pid" 2>/dev/null || true
        fi
    done
    sleep 1
    for pid in "${RUNNER_PID:-}" "${SERVER_PID:-}"; do
        if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
            kill -9 "$pid" 2>/dev/null || true
        fi
    done
    if [ -n "${TMP_ROOT:-}" ] && [ -d "$TMP_ROOT" ]; then
        rm -rf -- "$TMP_ROOT"
    fi
}
trap cleanup INT TERM EXIT

command -v curl >/dev/null 2>&1 || { echo "curl required" >&2; exit 2; }
command -v python3 >/dev/null 2>&1 || { echo "python3 required" >&2; exit 2; }
command -v git >/dev/null 2>&1 || { echo "git required" >&2; exit 2; }

if [ "${E2E_SKIP_RUN:-0}" = "1" ]; then
    log "E2E_SKIP_RUN=1: syntax check only"
    exit 0
fi

# Build once so restarts are fast and both restarts run the same binaries.
log "building webcodex + webcodex-runner (release of the current tree, debug profile)"
"$CARGO_BIN" build --quiet -p webcodex -p webcodex-runner --bins
SERVER_BIN="$PROJECT_DIR/target/debug/webcodex-server"
RUNNER_BIN="$PROJECT_DIR/target/debug/webcodex-runner"

PORT="${E2E_PORT:-$(find_free_port)}"
TMP_ROOT="$(mktemp -d -t webcodex-reconnect-e2e-XXXXXX)"
COOKIE_JAR="$TMP_ROOT/cookies.txt"
: >"$COOKIE_JAR"
DATA_DIR="$TMP_ROOT/data"
PROJECTS_DIR="$TMP_ROOT/project-registry"
AGENT_TOML="$TMP_ROOT/runner.toml"
TEST_REPO="$TMP_ROOT/reconnect-repo"
SERVER_LOG="$TMP_ROOT/server.log"
RUNNER_LOG="$TMP_ROOT/agent.log"
mkdir -p "$DATA_DIR" "$PROJECTS_DIR" "$TEST_REPO"
log "temp root: $TMP_ROOT (port $PORT)"

(
    cd "$TEST_REPO"
    git init -b main >/dev/null 2>&1
    git config user.email "e2e@test.local"
    git config user.name "Reconnect E2E"
    printf '# Reconnect fixture\n' > README.md
    git add . >/dev/null 2>&1
    git commit -m "seed" >/dev/null 2>&1
)

cat > "$PROJECTS_DIR/${PROJECT_ID}.toml" <<EOF
id = "${PROJECT_ID}"
path = "${TEST_REPO}"
name = "Reconnect Project"
allow_patch = true
EOF

cat > "$AGENT_TOML" <<EOF
server_url = "http://127.0.0.1:${PORT}"
token = "${TOKEN}"
client_id = "${CLIENT_ID}"
display_name = "Reconnect E2E Agent"
owner = "e2e"
project_registry_dir = "${PROJECTS_DIR}"
poll_interval_ms = 500
transport = "websocket"

[policy]
allow_raw_shell = true
allow_cwd_anywhere = false
allowed_roots = ["${TEST_REPO}"]
max_timeout_secs = 60
max_output_bytes = 262144
EOF

start_server() {
    WEBCODEX_ADDR="127.0.0.1:${PORT}" \
    WEBCODEX_DATA="$DATA_DIR" \
    WEBCODEX_TOKEN="$TOKEN" \
    RUST_LOG="info" \
    "$SERVER_BIN" >>"$SERVER_LOG" 2>&1 &
    SERVER_PID=$!
}

start_runner() {
    "$RUNNER_BIN" --config "$AGENT_TOML" >>"$RUNNER_LOG" 2>&1 &
    RUNNER_PID=$!
}

wait_for_server() {
    for _ in $(seq 1 40); do
        check_deadline
        if (echo >/dev/tcp/127.0.0.1/"$PORT") 2>/dev/null; then
            return 0
        fi
        sleep 1
    done
    return 1
}

runtime_status() { api_post /api/runtime/status '{}'; }

wait_for_agent_online() {
    for _ in $(seq 1 60); do
        check_deadline
        local body; body="$(runtime_status || true)"
        if [ "$(json_get "$body" output.agents.online_count)" = "1" ]; then
            echo "$body"
            return 0
        fi
        sleep 1
    done
    return 1
}

wait_for_agent_offline() {
    for _ in $(seq 1 90); do
        check_deadline
        local body; body="$(runtime_status || true)"
        if [ "$(json_get "$body" output.agents.online_count)" = "0" ]; then
            echo "$body"
            return 0
        fi
        sleep 1
    done
    return 1
}

# ----------------------------------------------------------------------------
# Phase 1: boot + baseline layered observations
# ----------------------------------------------------------------------------
log "starting server + runner"
start_server
wait_for_server || { fail "server did not listen"; exit 1; }
start_runner
BODY="$(wait_for_agent_online)" || { fail "runner did not register"; exit 1; }
pass "server + runner online"

LAYERS_PREFIX="output.connection_layers"
assert_eq "runner_process ready" "$(json_get "$BODY" ${LAYERS_PREFIX}.runner_process.status)" "ready"
assert_eq "runner_process source is runner report" \
    "$(json_get "$BODY" ${LAYERS_PREFIX}.runner_process.source)" "runner_process_report"
assert_nonempty "runner_process.process_started_at reported" \
    "$(json_get "$BODY" ${LAYERS_PREFIX}.runner_process.process_started_at)"
assert_eq "server_transport connected" "$(json_get "$BODY" ${LAYERS_PREFIX}.server_transport.status)" "connected"
INSTANCE_A="$(json_get "$BODY" ${LAYERS_PREFIX}.server_transport.connection_instance)"
assert_nonempty "connection instance identity" "$INSTANCE_A"
assert_eq "server_registration registered" \
    "$(json_get "$BODY" ${LAYERS_PREFIX}.server_registration.status)" "registered"
assert_eq "project_registry registered" \
    "$(json_get "$BODY" ${LAYERS_PREFIX}.project_registry.status)" "registered"
assert_eq "connector_endpoint honest not_configured" \
    "$(json_get "$BODY" ${LAYERS_PREFIX}.connector_endpoint.status)" "not_configured"
assert_eq "version_compatibility compatible" \
    "$(json_get "$BODY" output.version_compatibility.status)" "compatible"
assert_nonempty "server build version reported" \
    "$(json_get "$BODY" output.version_compatibility.server.version)"
assert_eq "authority mode default trusted_agent" \
    "$(json_get "$BODY" output.authority.mode)" "trusted_agent"
assert_eq "authority source default" "$(json_get "$BODY" output.authority.source)" "default"

SHELL_DIALECT="$(json_get "$BODY" output.agents.clients.0.shell_profiles.default_dialect)"
assert_nonempty "runner-reported shell default_dialect" "$SHELL_DIALECT"

# ----------------------------------------------------------------------------
# Phase 2: durable session + a running job, then runner crash
# ----------------------------------------------------------------------------
START_BODY="$(api_post /api/tools/call "{\"tool\":\"work_on_project\",\"params\":{\"project\":\"${RUNTIME_PROJECT_ID}\",\"instruction\":\"reconnect continuity\"}}")"
SESSION_ID="$(json_get "$START_BODY" output.session_id)"
assert_nonempty "durable session created" "$SESSION_ID"
assert_eq "work_on_project created explicit task session" \
    "$(json_get "$START_BODY" output.continuation)" "created"

JOB_BODY="$(api_post /api/tools/call "{\"tool\":\"run_job\",\"params\":{\"project\":\"${RUNTIME_PROJECT_ID}\",\"command\":\"sleep 300\",\"timeout_secs\":600}}")"
JOB_ID="$(json_get "$JOB_BODY" output.job_id)"
if [ -z "$JOB_ID" ]; then
    JOB_ID="$(json_get "$JOB_BODY" output.job.job_id)"
fi
assert_nonempty "long-running agent job started" "$JOB_ID"

log "killing runner (simulated crash)"
kill -9 "$RUNNER_PID" 2>/dev/null || true
RUNNER_PID=""
BODY="$(wait_for_agent_offline)" || { fail "server never observed runner offline"; exit 1; }
pass "server observed runner disconnect"

for layer_status in \
    "runner_process:stale" \
    "server_transport:disconnected" \
    "server_registration:stale" \
    "project_registry:stale"; do
    layer="${layer_status%%:*}"; expected="${layer_status##*:}"
    assert_eq "after crash: ${layer} is ${expected} (never fake-ready)" \
        "$(json_get "$BODY" ${LAYERS_PREFIX}.${layer}.status)" "$expected"
done

JOBS_BODY="$(observe_job "$JOB_ID")"
JOB_STATE="$(json_get "$JOBS_BODY" output.items.0.output.status)"
assert_eq "in-flight reconciliation-capable job is recovering after crash" "$JOB_STATE" "recovering"

# ----------------------------------------------------------------------------
# Phase 3: runner restart — new instance, no server restart
# ----------------------------------------------------------------------------
log "restarting runner"
start_runner
BODY="$(wait_for_agent_online)" || { fail "runner did not re-register"; exit 1; }
INSTANCE_B="$(json_get "$BODY" ${LAYERS_PREFIX}.server_transport.connection_instance)"
assert_nonempty "new connection instance" "$INSTANCE_B"
if [ "$INSTANCE_A" != "$INSTANCE_B" ]; then
    pass "new runner instance replaced the stale one"
else
    fail "runner reconnect reused the crashed instance identity"
fi
assert_eq "project re-registered after runner restart" \
    "$(json_get "$BODY" ${LAYERS_PREFIX}.project_registry.status)" "registered"

JOBS_BODY="$(observe_job "$JOB_ID")"
JOB_STATE="$(json_get "$JOBS_BODY" output.items.0.output.status)"
assert_eq "replacement instance fences old recovering job to lost" "$JOB_STATE" "lost"
assert_eq "replacement loss reason is runner_instance_replaced" \
    "$(json_get "$JOBS_BODY" output.items.0.output.recovery_reason_code)" "runner_instance_replaced"

READ_BODY="$(api_post /api/tools/call "{\"tool\":\"read_files\",\"params\":{\"project\":\"${RUNTIME_PROJECT_ID}\",\"items\":[{\"path\":\"README.md\"}]}}")"
assert_eq "calls recover after runner restart (no server restart)" \
    "$(json_get "$READ_BODY" success)" "True"

# ----------------------------------------------------------------------------
# Phase 4: server restart — durable session and explicit continuation
# ----------------------------------------------------------------------------
log "restarting server"
kill "$SERVER_PID" 2>/dev/null || true
sleep 2
kill -9 "$SERVER_PID" 2>/dev/null || true
SERVER_PID=""
start_server
wait_for_server || { fail "server did not restart"; exit 1; }
BODY="$(wait_for_agent_online)" || { fail "runner did not auto-reconnect after server restart"; exit 1; }
pass "runner auto-reconnected after server restart"

SUMMARY_BODY="$(api_post /api/tools/call "{\"tool\":\"session_summary\",\"params\":{\"session_id\":\"${SESSION_ID}\"}}")"
assert_eq "durable session resumable via explicit session_id" \
    "$(json_get "$SUMMARY_BODY" success)" "True"

RESTART_START="$(api_post /api/tools/call "{\"tool\":\"work_on_project\",\"params\":{\"project\":\"${RUNTIME_PROJECT_ID}\",\"instruction\":\"post restart\",\"session_id\":\"${SESSION_ID}\"}}")"
assert_eq "explicit continuation succeeds after server restart" \
    "$(json_get "$RESTART_START" success)" "True"
assert_eq "explicit continuation keeps the original session" \
    "$(json_get "$RESTART_START" output.session_id)" "$SESSION_ID"
assert_eq "explicit continuation is reported as resumed" \
    "$(json_get "$RESTART_START" output.continuation)" "resumed_explicitly"

# ----------------------------------------------------------------------------
# Phase 5: post-deploy smoke facts
# ----------------------------------------------------------------------------
log "post-deploy smoke facts:"
log "  server.version        = $(json_get "$BODY" output.version)"
log "  server.git_commit     = $(json_get "$BODY" output.build.git_commit)"
log "  authority.mode        = $(json_get "$BODY" output.authority.mode)"
log "  version_compatibility = $(json_get "$BODY" output.version_compatibility.status)"
log "  runner.shell_dialect  = ${SHELL_DIALECT}"

log "==============================================="
log "pass=$PASS fail=$FAIL"
if [ "$FAIL" -gt 0 ]; then
    log "server log: $SERVER_LOG"
    log "runner log:  $RUNNER_LOG"
    exit 1
fi
exit 0
