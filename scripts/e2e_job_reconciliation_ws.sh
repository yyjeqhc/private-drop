#!/usr/bin/env bash
set -euo pipefail

# ============================================================================
# WebCodex — active Job reconciliation across a Server restart E2E
#
# Real-process harness for the async Job recovery phase 1 acceptance:
#   Scenario A — a running Job survives a Server restart.
#     1. Boots a real `webcodex-server` and a real WebSocket `webcodex-runner`.
#     2. Waits for the runner online and the `job_state_reconciliation`
#        capability.
#     3. Starts a long-running job with deterministic marker output.
#     4. Records job_id, last_update_seq, and the stdout/stderr cursor.
#     5. Stops the SERVER only (runner process + job process stay alive).
#     6. Restarts the server; the SAME runner instance re-registers and
#        reconciles the in-flight job via its active inventory.
#     7. Re-queries the original job_id; asserts ownership/project/session
#        survive, last_update_seq does not regress, existing stdout does not
#        repeat, the log cursor is monotonic, and recovered_after_server_restart
#        / reconciliation metadata is correct.
#     8. stop_job(confirm=true) drives the ORIGINAL process group; asserts the
#        job reaches `stopped` and is never `lost`, with exactly one set of
#        side-effects.
#
#   Scenario B — a Job completes while the Server is offline.
#     1. Starts a short job; confirms the runner has taken it over.
#     2. Stops the server.
#     3. Waits for the command to finish on the runner.
#     4. Restarts the server; the runner submits the terminal inventory.
#     5. Re-queries the original job_id: `completed`, exit_code=0, ended_at /
#        duration / logs present, markers appear exactly the expected number
#        of times, no duplicate log replay across re-registration, no second
#        job, no re-execution.
##
#   Scenario C — a handed-off structured process survives a Server restart.
#     1. Starts `run_process` and waits for the same execution to hand off as a Job.
#     2. Records the original job_id and observation token.
#     3. Stops the Server only while the Runner process and child stay alive.
#     4. Restarts the Server; the same Runner inventory reconstructs the same Job.
#     5. Reuses the old observation token and requires an immediate fresh-epoch
#        snapshot, then stops the original process by the original job_id.
#     6. A marker proves the structured command executed exactly once.
##
#   Scenario D — a real `cargo_check` validation handoff survives restart.
#     The fixture build script runs longer than the 90s validation sync window,
#     forcing the first-class validation path to hand off the same Job. The
#     original job_id and old observation token must remain usable after a
#     Control-only restart, with no validation redispatch.
#
# Everything uses temp dirs, temp ports, temp tokens, and a temp project.
# Never reads or controls production services. No QUIC certs or prod domains.
# Environment overrides: E2E_PORT, E2E_TOKEN, E2E_TIMEOUT_SECS,
# E2E_SKIP_RUN=1 (syntax check only), CARGO_BIN.
# Exit codes: 0 pass, 1 failures, 2 environment error.
# ============================================================================

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$PROJECT_DIR"

CARGO_BIN="${CARGO_BIN:-cargo}"
TOKEN="${E2E_TOKEN:-e2e-jobrecon-token}"
CLIENT_ID="e2e-jobrecon-agent"
PROJECT_ID="jobrecon-proj"
TIMEOUT_SECS="${E2E_TIMEOUT_SECS:-480}"
RUNTIME_PROJECT_ID="agent:${CLIENT_ID}:${PROJECT_ID}"

PASS=0
FAIL=0
SERVER_PID=""
RUNNER_PID=""
TMP_ROOT=""
COOKIE_JAR=""
START_EPOCH=$(date +%s)

log() { printf '[jobrecon-e2e] %s\n' "$*"; }
fail() { FAIL=$((FAIL + 1)); printf '[jobrecon-e2e][FAIL] %s\n' "$*" >&2; }
pass() { PASS=$((PASS + 1)); printf '[jobrecon-e2e][ok]   %s\n' "$*"; }

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
    local path="$1"; local body="${2:-}"; local max_time="${3:-15}"
    if [ -z "$body" ]; then body="{}"; fi
    curl -sS --max-time "$max_time" \
        -c "$COOKIE_JAR" -b "$COOKIE_JAR" \
        -H "Authorization: Bearer ${TOKEN}" \
        -H "Content-Type: application/json" \
        -X POST "http://127.0.0.1:${PORT}${path}" \
        -d "$body" 2>/dev/null
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

assert_ne() {
    local label="$1"; local actual="$2"; local expected="$3"
    if [ "$actual" != "$expected" ]; then
        pass "$label"
    else
        fail "$label (got unexpected '$expected')"
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

# Print a bounded tail of server/runner logs on failure. Token is never in
# the process logs (it is only ever sent as a header), but scrub defensively.
dump_logs() {
    log "---- server log (last 60 lines) ----"
    if [ -f "$SERVER_LOG" ]; then
        sed -E 's/(Bearer )[^ ]*/\1<redacted>/g' "$SERVER_LOG" | tail -n 60 >&2
    fi
    log "---- agent log (last 60 lines) ----"
    if [ -f "$RUNNER_LOG" ]; then
        sed -E 's/(Bearer )[^ ]*/\1<redacted>/g' "$RUNNER_LOG" | tail -n 60 >&2
    fi
}

if [ "${E2E_SKIP_RUN:-0}" = "1" ]; then
    log "E2E_SKIP_RUN=1: syntax check only"
    exit 0
fi

# Build once so restarts reuse the same binaries.
log "building webcodex + webcodex-runner (debug profile)"
"$CARGO_BIN" build --quiet -p webcodex -p webcodex-runner --bins
SERVER_BIN="$PROJECT_DIR/target/debug/webcodex-server"
RUNNER_BIN="$PROJECT_DIR/target/debug/webcodex-runner"

PORT="${E2E_PORT:-$(find_free_port)}"
TMP_ROOT="$(mktemp -d -t webcodex-jobrecon-e2e-XXXXXX)"
COOKIE_JAR="$TMP_ROOT/cookies.txt"
: >"$COOKIE_JAR"
DATA_DIR="$TMP_ROOT/data"
PROJECTS_DIR="$TMP_ROOT/project-registry"
AGENT_TOML="$TMP_ROOT/runner.toml"
TEST_REPO="$TMP_ROOT/jobrecon-repo"
SERVER_LOG="$TMP_ROOT/server.log"
RUNNER_LOG="$TMP_ROOT/agent.log"
mkdir -p "$DATA_DIR" "$PROJECTS_DIR" "$TEST_REPO"
log "temp root: $TMP_ROOT (port $PORT)"

(
    cd "$TEST_REPO"
    git init -b main >/dev/null 2>&1
    git config user.email "e2e@test.local"
    git config user.name "JobRecon E2E"
    printf '# JobRecon fixture\n' > README.md
    mkdir -p src
    cat > Cargo.toml <<'EOF'
[package]
name = "jobrecon-fixture"
version = "0.1.0"
edition = "2021"
build = "build.rs"
EOF
    cat > src/lib.rs <<'EOF'
pub fn fixture() -> u32 { 1 }
EOF
    cat > build.rs <<'EOF'
use std::fs::OpenOptions;
use std::io::Write;
use std::thread;
use std::time::Duration;

fn main() {
    let mut marker = OpenOptions::new()
        .create(true)
        .append(true)
        .open("scenario-d-count.txt")
        .expect("open scenario D marker");
    writeln!(marker, "D-START").expect("write scenario D marker");
    marker.flush().expect("flush scenario D marker");
    thread::sleep(Duration::from_secs(150));
}
EOF
    git add . >/dev/null 2>&1
    git commit -m "seed" >/dev/null 2>&1
)

cat > "$PROJECTS_DIR/${PROJECT_ID}.toml" <<EOF
id = "${PROJECT_ID}"
path = "${TEST_REPO}"
name = "Job Reconciliation Project"
allow_patch = true
EOF

cat > "$AGENT_TOML" <<EOF
server_url = "http://127.0.0.1:${PORT}"
token = "${TOKEN}"
client_id = "${CLIENT_ID}"
display_name = "JobRecon E2E Agent"
owner = "e2e"
project_registry_dir = "${PROJECTS_DIR}"
poll_interval_ms = 500
transport = "websocket"

[policy]
allow_raw_shell = true
allow_cwd_anywhere = false
allowed_roots = ["${TEST_REPO}"]
max_timeout_secs = 180
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

# Wait for the runner to advertise job_state_reconciliation, surfaced in the
# runtime status connection layers / agent client capability summary.
wait_for_reconciliation_capability() {
    for _ in $(seq 1 60); do
        check_deadline
        local body; body="$(runtime_status || true)"
        local cap; cap="$(json_get "$body" output.agents.clients.0.capabilities.job_state_reconciliation)"
        if [ "$cap" = "True" ]; then
            echo "$body"; return 0
        fi
        sleep 1
    done
    return 1
}

tool_call() {
    local tool="$1"; local params="$2"; local max_time="${3:-15}"
    api_post /api/tools/call "{\"tool\":\"${tool}\",\"params\":${params}}" "$max_time"
}

run_job_call() {
    local command="$1"; local timeout="$2"
    tool_call "run_job" "{\"project\":\"${RUNTIME_PROJECT_ID}\",\"command\":$(python3 -c 'import json,sys; print(json.dumps(sys.argv[1]))' "$command"),\"timeout_secs\":${timeout}}"
}

run_process_call() {
    local script="$1"; local timeout="$2"
    local encoded
    encoded="$(python3 -c 'import json,sys; print(json.dumps(sys.argv[1]))' "$script")"
    tool_call "run_process" "{\"project\":\"${RUNTIME_PROJECT_ID}\",\"executable\":\"python3\",\"args\":[${encoded}],\"timeout_secs\":${timeout}}"
}

observe_job_call() {
    local job_id="$1"; local token="$2"; local wait_secs="$3"
    local encoded_token
    encoded_token="$(python3 -c 'import json,sys; print(json.dumps(sys.argv[1]))' "$token")"
    tool_call "observe_jobs" "{\"items\":[{\"job_id\":\"${job_id}\",\"after_observation_token\":${encoded_token}}],\"tail_lines\":40,\"wait_secs\":${wait_secs}}"
}

observe_one_job_compat() {
    local job_id="$1"; local tail_lines="$2"
    tool_call "observe_jobs" "{\"items\":[{\"job_id\":\"${job_id}\"}],\"tail_lines\":${tail_lines}}" | python3 -c \
        'import json,sys; d=json.load(sys.stdin); item=d["output"]["items"][0]; print(json.dumps({"success":item["success"],"output":item.get("output",{}),"error":item.get("error")}))'
}

job_status_call() {
    observe_one_job_compat "$1" 1
}

job_log_call() {
    observe_one_job_compat "$1" 40
}

stop_job_call() {
    local job_id="$1"
    tool_call "stop_job" "{\"project\":\"${RUNTIME_PROJECT_ID}\",\"job_id\":\"${job_id}\",\"confirm\":true}"
}

# Wait for a job status to match one of a set of expected statuses, bounded.
# Args: job_id expected1 [expected2 ...]
wait_for_job_status() {
    local job_id="$1"; shift
    local expected="$*"
    for _ in $(seq 1 90); do
        check_deadline
        local body; body="$(job_status_call "$job_id" || true)"
        local status; status="$(json_get "$body" output.status)"
        for want in $expected; do
            if [ "$status" = "$want" ]; then
                echo "$body"; return 0
            fi
        done
        sleep 1
    done
    return 1
}

# ----------------------------------------------------------------------------
# Boot server + runner
# ----------------------------------------------------------------------------
log "starting server + runner"
start_server
wait_for_server || { fail "server did not listen"; dump_logs; exit 1; }
start_runner
BODY="$(wait_for_agent_online)" || { fail "runner did not register"; dump_logs; exit 1; }
pass "server + runner online"

BODY="$(wait_for_reconciliation_capability)" || { fail "runner did not advertise job_state_reconciliation"; dump_logs; exit 1; }
pass "runner advertises job_state_reconciliation"

INSTANCE_ID="$(json_get "$BODY" output.connection_layers.server_transport.connection_instance)"
assert_nonempty "connection instance identity recorded" "$INSTANCE_ID"

# ----------------------------------------------------------------------------
# Scenario A: a running job survives a server restart
# ----------------------------------------------------------------------------
log "scenario A: running job across server restart"

# A long, controllable task with deterministic marker output. It emits a
# START marker immediately, then sleeps so the job stays running through the
# server restart, and prints a final marker on stop. Side effects land in a
# fixture file inside the temp repo.
A_MARKER_FILE="$TEST_REPO/scenario-a-count.txt"
: >"$A_MARKER_FILE"
A_COMMAND="printf 'A-START\\n' >> '$A_MARKER_FILE'; printf 'A-START\\n'; printf 'A-START\\n' >&2; i=0; while [ ! -f '$TEST_REPO/scenario-a-stop.flag' ] && [ \$i -lt 600 ]; do sleep 1; i=\$((i+1)); done; printf 'A-END %d\\n' \$i >> '$A_MARKER_FILE'"

JOB_BODY="$(run_job_call "$A_COMMAND" 600)"
JOB_ID_A="$(json_get "$JOB_BODY" output.job_id)"
assert_nonempty "scenario A: long-running job started" "$JOB_ID_A"
assert_eq "scenario A: job executor is agent" "$(json_get "$JOB_BODY" output.executor)" "agent"

# Wait for running + first stdout segment.
BODY_A="$(wait_for_job_status "$JOB_ID_A" running)" || { fail "scenario A: job did not reach running"; dump_logs; exit 1; }
pass "scenario A: job is running"

# Read the first stdout/stderr segment and capture the cursor + update_seq.
LOG_BODY_A="$(job_log_call "$JOB_ID_A")"
sleep 1
LOG_BODY_A="$(job_log_call "$JOB_ID_A")"
A_STDOUT_1="$(json_get "$LOG_BODY_A" output.stdout_tail)"
A_STDERR_1="$(json_get "$LOG_BODY_A" output.stderr_tail)"
A_SEQ_1="$(json_get "$LOG_BODY_A" output.last_update_seq)"
A_CURSOR_OUT_1="$(json_get "$LOG_BODY_A" output.cursor.stdout)"
A_CURSOR_ERR_1="$(json_get "$LOG_BODY_A" output.cursor.stderr)"
assert_eq "scenario A: first stdout has A-START marker" "$A_STDOUT_1" "A-START"
assert_eq "scenario A: first stderr has A-START marker" "$A_STDERR_1" "A-START"
assert_nonempty "scenario A: last_update_seq recorded" "$A_SEQ_1"
assert_nonempty "scenario A: stdout cursor recorded" "$A_CURSOR_OUT_1"

# Snapshot ownership/project before restart.
A_PROJECT_BEFORE="$(json_get "$BODY_A" output.project)"
A_CLIENT_BEFORE="$(json_get "$BODY_A" output.client_id)"

log "scenario A: stopping SERVER only (runner + job process stay alive)"
kill "$SERVER_PID" 2>/dev/null || true
sleep 2
kill -9 "$SERVER_PID" 2>/dev/null || true
SERVER_PID=""
# Confirm the runner process is still alive (the job must keep running).
if ! kill -0 "$RUNNER_PID" 2>/dev/null; then
    fail "scenario A: runner process died with the server"; dump_logs; exit 1
fi
pass "scenario A: runner process stayed alive across server stop"

# Restart the server; the SAME runner instance re-registers and reconciles.
start_server
wait_for_server || { fail "scenario A: server did not restart"; dump_logs; exit 1; }
log "scenario A: waiting for runner to re-register and reconcile"
BODY="$(wait_for_agent_online)" || { fail "scenario A: runner did not re-register after server restart"; dump_logs; exit 1; }
pass "scenario A: runner re-registered after server restart"

INSTANCE_ID_2="$(json_get "$BODY" output.connection_layers.server_transport.connection_instance)"
assert_eq "scenario A: same runner instance reconciled (no new instance)" "$INSTANCE_ID_2" "$INSTANCE_ID"

# Re-query the original job_id; wait for reconciliation to settle to running.
log "scenario A: waiting for job reconciliation to settle"
RECON_BODY="$(wait_for_job_status "$JOB_ID_A" running stop_requested)" || { fail "scenario A: job did not reconcile to a non-lost active state"; dump_logs; exit 1; }
RECON_STATUS="$(json_get "$RECON_BODY" output.status)"
assert_ne "scenario A: job is not lost after reconciliation" "$RECON_STATUS" "lost"

A_PROJECT_AFTER="$(json_get "$RECON_BODY" output.project)"
A_CLIENT_AFTER="$(json_get "$RECON_BODY" output.client_id)"
assert_eq "scenario A: project ownership preserved" "$A_PROJECT_AFTER" "$A_PROJECT_BEFORE"
assert_eq "scenario A: client ownership preserved" "$A_CLIENT_AFTER" "$A_CLIENT_BEFORE"
assert_eq "scenario A: runner instance preserved" "$(json_get "$RECON_BODY" output.client_id)" "$A_CLIENT_BEFORE"

A_RECOVERED="$(json_get "$RECON_BODY" output.recovered_after_server_restart)"
A_RECON_STATE="$(json_get "$RECON_BODY" output.recovery_state)"
assert_eq "scenario A: recovered_after_server_restart flag set" "$A_RECOVERED" "True"
# After reconciliation the job is active again; recovery_state reflects the
# reconciliation that rebuilt it from the runner inventory.
assert_nonempty "scenario A: reconciliation state recorded" "$A_RECON_STATE"

# last_update_seq must not regress across the restart.
RECON_LOG="$(job_log_call "$JOB_ID_A")"
A_SEQ_2="$(json_get "$RECON_LOG" output.last_update_seq)"
if [ -n "$A_SEQ_2" ] && [ -n "$A_SEQ_1" ] && [ "$A_SEQ_2" -lt "$A_SEQ_1" ] 2>/dev/null; then
    fail "scenario A: last_update_seq regressed ($A_SEQ_1 -> $A_SEQ_2)"
else
    pass "scenario A: last_update_seq did not regress"
fi

# Existing stdout must not repeat; the log cursor is monotonic.
A_STDOUT_2="$(json_get "$RECON_LOG" output.stdout_tail)"
A_CURSOR_OUT_2="$(json_get "$RECON_LOG" output.cursor.stdout)"
# A-START must appear exactly once across the retained tail (no duplication).
A_START_COUNT="$(printf '%s\n' "$A_STDOUT_2" | grep -c 'A-START' || true)"
if [ "$A_START_COUNT" -eq 1 ]; then
    pass "scenario A: existing stdout not duplicated after reconciliation"
else
    fail "scenario A: stdout marker duplicated (count=$A_START_COUNT)"
fi
if [ -n "$A_CURSOR_OUT_2" ] && [ -n "$A_CURSOR_OUT_1" ] && [ "$A_CURSOR_OUT_2" -ge "$A_CURSOR_OUT_1" ] 2>/dev/null; then
    pass "scenario A: stdout cursor is monotonic"
else
    fail "scenario A: stdout cursor regressed ($A_CURSOR_OUT_1 -> $A_CURSOR_OUT_2)"
fi

# Stop the original process group via the original job_id.
log "scenario A: stop_job the original process group"
: >"$TEST_REPO/scenario-a-stop.flag"
STOP_BODY="$(stop_job_call "$JOB_ID_A")"
assert_eq "scenario A: stop_job accepted (confirm=true)" "$(json_get "$STOP_BODY" success)" "True"
BODY_A_STOP="$(wait_for_job_status "$JOB_ID_A" stopped)" || { fail "scenario A: job did not reach stopped"; dump_logs; exit 1; }
assert_eq "scenario A: job reached stopped" "$(json_get "$BODY_A_STOP" output.status)" "stopped"
assert_ne "scenario A: job is not lost at the end" "$(json_get "$BODY_A_STOP" output.status)" "lost"

# Exactly one set of side effects: the marker file has exactly one A-START.
A_FILE_START_COUNT="$(grep -c 'A-START' "$A_MARKER_FILE" || true)"
if [ "$A_FILE_START_COUNT" -eq 1 ]; then
    pass "scenario A: single side-effect set (one A-START in marker file)"
else
    fail "scenario A: marker file has $A_FILE_START_COUNT A-START lines (expected 1)"
fi
# The job must not have been re-executed: the A-END line count is <= 1.
A_FILE_END_COUNT="$(grep -c 'A-END' "$A_MARKER_FILE" || true)"
if [ "$A_FILE_END_COUNT" -le 1 ]; then
    pass "scenario A: job not re-executed (A-END count $A_FILE_END_COUNT)"
else
    fail "scenario A: job appears re-executed (A-END count $A_FILE_END_COUNT)"
fi

# Only one job for this client across the whole scenario.
LIST_BODY="$(tool_call "list_jobs" '{"limit":100}')"
A_JOB_COUNT="$(printf '%s' "$(json_get "$LIST_BODY" output.jobs)" | python3 -c 'import json,sys; obj=json.loads(sys.stdin.read() or "[]"); print(len([j for j in obj if j.get("job_id")=="'"$JOB_ID_A"'"]))' 2>/dev/null || echo "?")"
if [ "$A_JOB_COUNT" = "1" ]; then
    pass "scenario A: exactly one job record for the original job_id"
else
    fail "scenario A: expected 1 job record, got $A_JOB_COUNT"
fi
log "scenario A complete"

# ----------------------------------------------------------------------------
# Scenario B: a job completes while the server is offline
# ----------------------------------------------------------------------------
log "scenario B: job completes while server offline"

B_MARKER_FILE="$TEST_REPO/scenario-b-count.txt"
: >"$B_MARKER_FILE"
# A short job: emits a START marker on stdout and stderr, sleeps briefly,
# emits an END marker, and exits 0. It should finish while the server is down.
B_COMMAND="printf 'B-START\\n' >> '$B_MARKER_FILE'; printf 'B-START\\n'; printf 'B-START\\n' >&2; sleep 3; printf 'B-END\\n' >> '$B_MARKER_FILE'; printf 'B-END\\n'; printf 'B-END\\n' >&2; exit 0"

JOB_BODY_B="$(run_job_call "$B_COMMAND" 60)"
JOB_ID_B="$(json_get "$JOB_BODY_B" output.job_id)"
assert_nonempty "scenario B: short job started" "$JOB_ID_B"
BODY_B_RUNNING="$(wait_for_job_status "$JOB_ID_B" running)" || { fail "scenario B: job did not reach running"; dump_logs; exit 1; }
pass "scenario B: runner took over the short job"

# Stop the server while the job is still running.
log "scenario B: stopping server while job runs"
kill "$SERVER_PID" 2>/dev/null || true
sleep 2
kill -9 "$SERVER_PID" 2>/dev/null || true
SERVER_PID=""
if ! kill -0 "$RUNNER_PID" 2>/dev/null; then
    fail "scenario B: runner process died"; dump_logs; exit 1
fi
pass "scenario B: runner process stayed alive"

# Wait for the command to finish on the runner (bounded). The marker file
# records completion independent of the server.
B_DONE_DEADLINE=$(( $(date +%s) + 20 ))
while [ ! -f "$B_MARKER_FILE" ] || ! grep -q 'B-END' "$B_MARKER_FILE"; do
    check_deadline
    if [ "$(date +%s)" -ge "$B_DONE_DEADLINE" ]; then
        fail "scenario B: command did not finish on the runner in time"; dump_logs; exit 1
    fi
    sleep 1
done
pass "scenario B: command completed on the runner while server offline"

# Restart the server; the runner submits the terminal inventory.
start_server
wait_for_server || { fail "scenario B: server did not restart"; dump_logs; exit 1; }
BODY="$(wait_for_agent_online)" || { fail "scenario B: runner did not re-register"; dump_logs; exit 1; }
pass "scenario B: runner re-registered after server restart"
INSTANCE_ID_3="$(json_get "$BODY" output.connection_layers.server_transport.connection_instance)"
assert_eq "scenario B: same runner instance reconciled" "$INSTANCE_ID_3" "$INSTANCE_ID"

# Re-query the original job_id: must be completed, exit_code 0.
log "scenario B: waiting for terminal inventory reconciliation"
BODY_B_FINAL="$(wait_for_job_status "$JOB_ID_B" completed)" || { fail "scenario B: job did not reconcile to completed"; dump_logs; exit 1; }
assert_eq "scenario B: job recovered to completed" "$(json_get "$BODY_B_FINAL" output.status)" "completed"
assert_eq "scenario B: exit_code is 0" "$(json_get "$BODY_B_FINAL" output.exit_code)" "0"
assert_nonempty "scenario B: ended_at recorded" "$(json_get "$BODY_B_FINAL" output.ended_at)"
assert_nonempty "scenario B: duration recorded" "$(json_get "$BODY_B_FINAL" output.duration_ms)"
assert_ne "scenario B: job is not recovering" "$(json_get "$BODY_B_FINAL" output.recovery_state)" "recovering"
assert_eq "scenario B: recovered_after_server_restart flag set" "$(json_get "$BODY_B_FINAL" output.recovered_after_server_restart)" "True"

LOG_BODY_B="$(job_log_call "$JOB_ID_B")"
B_STDOUT="$(json_get "$LOG_BODY_B" output.stdout_tail)"
B_STDERR="$(json_get "$LOG_BODY_B" output.stderr_tail)"
B_START_OUT_COUNT="$(printf '%s\n' "$B_STDOUT" | grep -c 'B-START' || true)"
B_END_OUT_COUNT="$(printf '%s\n' "$B_STDOUT" | grep -c 'B-END' || true)"
B_START_ERR_COUNT="$(printf '%s\n' "$B_STDERR" | grep -c 'B-START' || true)"
B_END_ERR_COUNT="$(printf '%s\n' "$B_STDERR" | grep -c 'B-END' || true)"
assert_eq "scenario B: stdout B-START appears once" "$B_START_OUT_COUNT" "1"
assert_eq "scenario B: stdout B-END appears once" "$B_END_OUT_COUNT" "1"
assert_eq "scenario B: stderr B-START appears once" "$B_START_ERR_COUNT" "1"
assert_eq "scenario B: stderr B-END appears once" "$B_END_ERR_COUNT" "1"

# Marker file side effects: exactly one B-START and one B-END (no re-execution).
B_FILE_START_COUNT="$(grep -c 'B-START' "$B_MARKER_FILE" || true)"
B_FILE_END_COUNT="$(grep -c 'B-END' "$B_MARKER_FILE" || true)"
assert_eq "scenario B: single side-effect B-START" "$B_FILE_START_COUNT" "1"
assert_eq "scenario B: single side-effect B-END" "$B_FILE_END_COUNT" "1"

# Re-registration does not duplicate logs: query again and the markers are
# still exactly once.
LOG_BODY_B2="$(job_log_call "$JOB_ID_B")"
B_STDOUT2="$(json_get "$LOG_BODY_B2" output.stdout_tail)"
B_START_OUT_COUNT2="$(printf '%s\n' "$B_STDOUT2" | grep -c 'B-START' || true)"
assert_eq "scenario B: no duplicate log after re-query" "$B_START_OUT_COUNT2" "1"

# No second job created; the original job_id is the only B job.
LIST_BODY_B="$(tool_call "list_jobs" '{"limit":100}')"
B_JOB_COUNT="$(printf '%s' "$(json_get "$LIST_BODY_B" output.jobs)" | python3 -c 'import json,sys; obj=json.loads(sys.stdin.read() or "[]"); print(len([j for j in obj if j.get("job_id")=="'"$JOB_ID_B"'"]))' 2>/dev/null || echo "?")"
if [ "$B_JOB_COUNT" = "1" ]; then
    pass "scenario B: exactly one job record for the original job_id"
else
    fail "scenario B: expected 1 job record, got $B_JOB_COUNT"
fi
log "scenario B complete"

# ----------------------------------------------------------------------------
# Scenario C: a handed-off structured process keeps job identity and refreshes
# an old Server-epoch observation token after restart.
# ----------------------------------------------------------------------------
log "scenario C: structured process handoff across server restart"

C_MARKER_FILE="$TEST_REPO/scenario-c-count.txt"
C_SCRIPT="$TEST_REPO/scenario-c.py"
cat >"$C_SCRIPT" <<'PY'
from pathlib import Path
import time

marker = Path("scenario-c-count.txt")
with marker.open("a", encoding="utf-8") as handle:
    handle.write("C-START\n")
    handle.flush()
while True:
    time.sleep(0.25)
PY
: >"$C_MARKER_FILE"

# run_process uses the structured execution path. The child intentionally runs
# longer than the 10s synchronous grace window so the SAME execution is handed
# off rather than restarted.
JOB_BODY_C="$(run_process_call "scenario-c.py" 120)"
JOB_ID_C="$(json_get "$JOB_BODY_C" output.job_id)"
TOKEN_C_OLD="$(json_get "$JOB_BODY_C" output.observation_token)"
assert_nonempty "scenario C: structured process handed off with job_id" "$JOB_ID_C"
assert_nonempty "scenario C: structured process returned observation token" "$TOKEN_C_OLD"
assert_eq "scenario C: handoff reports promoted_to_job" "$(json_get "$JOB_BODY_C" output.promoted_to_job)" "True"
BODY_C_RUNNING="$(wait_for_job_status "$JOB_ID_C" running)" || { fail "scenario C: structured process did not reach running"; dump_logs; exit 1; }
C_START_BEFORE="$(grep -c 'C-START' "$C_MARKER_FILE" || true)"
assert_eq "scenario C: command executed once before restart" "$C_START_BEFORE" "1"

log "scenario C: stopping SERVER only after structured handoff"
kill "$SERVER_PID" 2>/dev/null || true
sleep 2
kill -9 "$SERVER_PID" 2>/dev/null || true
SERVER_PID=""
if ! kill -0 "$RUNNER_PID" 2>/dev/null; then
    fail "scenario C: runner process died"; dump_logs; exit 1
fi
pass "scenario C: runner process stayed alive"

start_server
wait_for_server || { fail "scenario C: server did not restart"; dump_logs; exit 1; }
BODY="$(wait_for_agent_online)" || { fail "scenario C: runner did not re-register"; dump_logs; exit 1; }
INSTANCE_ID_4="$(json_get "$BODY" output.connection_layers.server_transport.connection_instance)"
assert_eq "scenario C: same runner instance reconciled" "$INSTANCE_ID_4" "$INSTANCE_ID"

# The old token belongs to the previous Server epoch. It must be actionable
# immediately: same job_id, fresh token, no wait for command-side progress.
C_OBSERVE_STARTED="$(date +%s)"
OBS_BODY_C="$(observe_job_call "$JOB_ID_C" "$TOKEN_C_OLD" 30)"
C_OBSERVE_ELAPSED=$(( $(date +%s) - C_OBSERVE_STARTED ))
assert_eq "scenario C: old-token observation succeeds" "$(json_get "$OBS_BODY_C" output.items.0.success)" "True"
TOKEN_C_NEW="$(json_get "$OBS_BODY_C" output.items.0.output.observation_token)"
assert_nonempty "scenario C: refreshed observation token returned" "$TOKEN_C_NEW"
assert_ne "scenario C: Server restart refreshes observation epoch" "$TOKEN_C_NEW" "$TOKEN_C_OLD"
assert_eq "scenario C: original job_id survives token refresh" "$(json_get "$OBS_BODY_C" output.items.0.job_id)" "$JOB_ID_C"
if [ "$C_OBSERVE_ELAPSED" -le 5 ]; then
    pass "scenario C: stale Server-epoch token refreshed without waiting"
else
    fail "scenario C: stale token refresh took ${C_OBSERVE_ELAPSED}s (expected immediate)"
fi

STOP_BODY_C="$(stop_job_call "$JOB_ID_C")"
assert_eq "scenario C: stop_job accepted for original job_id" "$(json_get "$STOP_BODY_C" success)" "True"
BODY_C_STOP="$(wait_for_job_status "$JOB_ID_C" stopped)" || { fail "scenario C: original structured process did not stop"; dump_logs; exit 1; }
assert_eq "scenario C: original structured Job reached stopped" "$(json_get "$BODY_C_STOP" output.status)" "stopped"
C_START_AFTER="$(grep -c 'C-START' "$C_MARKER_FILE" || true)"
assert_eq "scenario C: structured command was never re-executed" "$C_START_AFTER" "1"

LIST_BODY_C="$(tool_call "list_jobs" '{"limit":100}')"
C_JOB_COUNT="$(printf '%s' "$(json_get "$LIST_BODY_C" output.jobs)" | python3 -c 'import json,sys; obj=json.loads(sys.stdin.read() or "[]"); print(len([j for j in obj if j.get("job_id")=="'"$JOB_ID_C"'"]))' 2>/dev/null || echo "?")"
assert_eq "scenario C: exactly one record for original job_id" "$C_JOB_COUNT" "1"
log "scenario C complete"

# ----------------------------------------------------------------------------
# Scenario D: exact first-class cargo_check handoff across a Server restart.
# ----------------------------------------------------------------------------
log "scenario D: cargo_check validation handoff across server restart"

D_MARKER_FILE="$TEST_REPO/scenario-d-count.txt"
rm -f -- "$D_MARKER_FILE"
JOB_BODY_D="$(tool_call "cargo_check" "{\"project\":\"${RUNTIME_PROJECT_ID}\",\"timeout_secs\":180}" 120)"
JOB_ID_D="$(json_get "$JOB_BODY_D" output.job_id)"
TOKEN_D_OLD="$(json_get "$JOB_BODY_D" output.observation_token)"
assert_nonempty "scenario D: cargo_check handed off with job_id" "$JOB_ID_D"
assert_nonempty "scenario D: cargo_check returned observation token" "$TOKEN_D_OLD"
assert_eq "scenario D: cargo_check reports promoted_to_job" "$(json_get "$JOB_BODY_D" output.promoted_to_job)" "True"
BODY_D_RUNNING="$(wait_for_job_status "$JOB_ID_D" running)" || { fail "scenario D: cargo_check did not remain running after handoff"; dump_logs; exit 1; }
D_START_BEFORE="$(grep -c 'D-START' "$D_MARKER_FILE" 2>/dev/null || true)"
assert_eq "scenario D: validation command started exactly once" "$D_START_BEFORE" "1"

log "scenario D: stopping SERVER only after cargo_check handoff"
kill "$SERVER_PID" 2>/dev/null || true
sleep 2
kill -9 "$SERVER_PID" 2>/dev/null || true
SERVER_PID=""
if ! kill -0 "$RUNNER_PID" 2>/dev/null; then
    fail "scenario D: runner process died"; dump_logs; exit 1
fi
pass "scenario D: runner process stayed alive"

start_server
wait_for_server || { fail "scenario D: server did not restart"; dump_logs; exit 1; }
BODY="$(wait_for_agent_online)" || { fail "scenario D: runner did not re-register"; dump_logs; exit 1; }
INSTANCE_ID_5="$(json_get "$BODY" output.connection_layers.server_transport.connection_instance)"
assert_eq "scenario D: same runner instance reconciled" "$INSTANCE_ID_5" "$INSTANCE_ID"

D_OBSERVE_STARTED="$(date +%s)"
OBS_BODY_D="$(observe_job_call "$JOB_ID_D" "$TOKEN_D_OLD" 30)"
D_OBSERVE_ELAPSED=$(( $(date +%s) - D_OBSERVE_STARTED ))
assert_eq "scenario D: old-token observation succeeds" "$(json_get "$OBS_BODY_D" output.items.0.success)" "True"
TOKEN_D_NEW="$(json_get "$OBS_BODY_D" output.items.0.output.observation_token)"
assert_nonempty "scenario D: refreshed observation token returned" "$TOKEN_D_NEW"
assert_ne "scenario D: Server restart refreshes validation observation epoch" "$TOKEN_D_NEW" "$TOKEN_D_OLD"
assert_eq "scenario D: original cargo_check job_id survives" "$(json_get "$OBS_BODY_D" output.items.0.job_id)" "$JOB_ID_D"
if [ "$D_OBSERVE_ELAPSED" -le 5 ]; then
    pass "scenario D: stale validation token refreshed without waiting"
else
    fail "scenario D: stale validation token refresh took ${D_OBSERVE_ELAPSED}s"
fi

STOP_BODY_D="$(stop_job_call "$JOB_ID_D")"
assert_eq "scenario D: stop_job accepted for original cargo_check" "$(json_get "$STOP_BODY_D" success)" "True"
BODY_D_STOP="$(wait_for_job_status "$JOB_ID_D" stopped)" || { fail "scenario D: original cargo_check did not stop"; dump_logs; exit 1; }
assert_eq "scenario D: original cargo_check reached stopped" "$(json_get "$BODY_D_STOP" output.status)" "stopped"
D_START_AFTER="$(grep -c 'D-START' "$D_MARKER_FILE" 2>/dev/null || true)"
assert_eq "scenario D: cargo_check was never redispatched" "$D_START_AFTER" "1"
assert_eq "scenario D: recovered_after_server_restart set" "$(json_get "$BODY_D_STOP" output.recovered_after_server_restart)" "True"

LIST_BODY_D="$(tool_call "list_jobs" '{"limit":100}')"
D_JOB_COUNT="$(printf '%s' "$(json_get "$LIST_BODY_D" output.jobs)" | python3 -c 'import json,sys; obj=json.loads(sys.stdin.read() or "[]"); print(len([j for j in obj if j.get("job_id")=="'"$JOB_ID_D"'"]))' 2>/dev/null || echo "?")"
assert_eq "scenario D: exactly one record for original cargo_check job_id" "$D_JOB_COUNT" "1"
log "scenario D complete"

if grep -q 'runner job inventory reconciled' "$SERVER_LOG" \
    && grep -q 'inventory_active' "$SERVER_LOG" \
    && grep -q 'reconstructed' "$SERVER_LOG" \
    && grep -q 'missing' "$SERVER_LOG"; then
    pass "reconciliation emits bounded count diagnostics"
else
    fail "reconciliation count diagnostics missing from server log"
fi

log "==============================================="
log "pass=$PASS fail=$FAIL"
if [ "$FAIL" -gt 0 ]; then
    dump_logs
    exit 1
fi
exit 0
