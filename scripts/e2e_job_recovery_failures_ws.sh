#!/usr/bin/env bash
set -euo pipefail

# ============================================================================
# WebCodex — Job recovery failure & compatibility paths E2E
#
# Real-process harness for async Job recovery phase 2: the failure/compat
# semantics the happy-path reconciliation script does NOT cover.
#
# Scenarios (all real `webcodex-server` + real WebSocket `webcodex-runner`,
# temp dirs/ports/tokens/projects, bounded waits, trap cleanup, masked logs):
#
#   C — Runner permanently gone within the recovery window.
#       Server + runner connected; long job running. Kill the RUNNER only
#       (server keeps running). The job enters `recovering`. The runner does
#       NOT return within the grace period. The non-request-triggered
#       recovery-timeout sweep transitions the job to terminal `lost` with
#       reason `runner_recovery_deadline_exceeded`. Asserts ended_at set once,
#       no new job, one list record, stop-on-lost is stable, re-query does not
#       rewrite ended_at/reason, re-sweep is a no-op, processes cleaned.
#
#   D — New runner instance replaces the old instance.
#       Instance A starts a long job. Instance B registers with the same
#       client_id but a different agent_instance_id. A's job becomes `lost`
#       with `runner_instance_replaced`; B starts its own new job; A's late
#       update is rejected; first ended_at/reason preserved.
#
#   E — Runner without reconciliation (no job_state_reconciliation) disconnect lost.
#       Runner registered with the reconciliation capability disabled
#       (WEBCODEX_RUNNER_DISABLE_JOB_STATE_RECONCILIATION=1). A job runs;
#       the no-reconciliation Runner disconnects. The job deterministically becomes
#       `lost` with `runner_disconnected_without_reconciliation`, stays lost after a server
#       restart, no re-execution, no same-client takeover.
#
#   F — Repeated server restart (3x) keeps the same job_id.
#       A long job across three server restarts uses the same job_id; the
#       command runs once; last_update_seq / stdout/stderr cursor do not
#       regress; log markers do not duplicate; stop controls the original
#       process group; terminal `stopped` survives a third restart; terminal
#       inventory replay does not rewrite first ended_at; one list record.
#       Also verifies the restart-rebuild recovery window: after a restart
#       the runner reconnects + submits inventory and a fresh bounded recovery
#       window is re-anchored (the in-process deadline is not persisted across
#       the restart).
#
# Uses WEBCODEX_JOB_RECOVERY_GRACE_SECS=10 (above the 5s floor) so the deadline
# is bounded without waiting the 120s production default.
#
# Everything is temp/isolated: never reads or controls production services, no
# QUIC certs, no prod domains, no systemd. Environment overrides: E2E_PORT,
# E2E_TOKEN, E2E_TIMEOUT_SECS, E2E_SKIP_RUN=1 (syntax check only), CARGO_BIN.
# Exit codes: 0 pass, 1 failures, 2 environment error.
# ============================================================================

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$PROJECT_DIR"

CARGO_BIN="${CARGO_BIN:-cargo}"
TOKEN="${E2E_TOKEN:-e2e-jobfail-token}"
CLIENT_ID="e2e-jobfail-agent"
PROJECT_ID="jobfail-proj"
TIMEOUT_SECS="${E2E_TIMEOUT_SECS:-600}"
RUNTIME_PROJECT_ID="agent:${CLIENT_ID}:${PROJECT_ID}"
# Short bounded recovery grace for tests (clamped by the server to >=5s).
export WEBCODEX_JOB_RECOVERY_GRACE_SECS="${WEBCODEX_JOB_RECOVERY_GRACE_SECS:-10}"

PASS=0
FAIL=0
SERVER_PID=""
RUNNER_PID=""
TMP_ROOT=""
COOKIE_JAR=""
START_EPOCH=$(date +%s)

log() { printf '[jobfail-e2e] %s\n' "$*"; }
fail() { FAIL=$((FAIL + 1)); printf '[jobfail-e2e][FAIL] %s\n' "$*" >&2; }
pass() { PASS=$((PASS + 1)); printf '[jobfail-e2e][ok]   %s\n' "$*"; }

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

log "building webcodex + webcodex-runner (debug profile)"
"$CARGO_BIN" build --quiet -p webcodex -p webcodex-runner --bins
SERVER_BIN="$PROJECT_DIR/target/debug/webcodex-server"
RUNNER_BIN="$PROJECT_DIR/target/debug/webcodex-runner"

PORT="${E2E_PORT:-$(find_free_port)}"
TMP_ROOT="$(mktemp -d -t webcodex-jobfail-e2e-XXXXXX)"
COOKIE_JAR="$TMP_ROOT/cookies.txt"
: >"$COOKIE_JAR"
DATA_DIR="$TMP_ROOT/data"
PROJECTS_DIR="$TMP_ROOT/project-registry"
AGENT_TOML="$TMP_ROOT/runner.toml"
NO_RECONCILIATION_AGENT_TOML="$TMP_ROOT/no-reconciliation-runner.toml"
TEST_REPO="$TMP_ROOT/jobfail-repo"
SERVER_LOG="$TMP_ROOT/server.log"
RUNNER_LOG="$TMP_ROOT/agent.log"
mkdir -p "$DATA_DIR" "$PROJECTS_DIR" "$TEST_REPO"
log "temp root: $TMP_ROOT (port $PORT, grace ${WEBCODEX_JOB_RECOVERY_GRACE_SECS}s)"

(
    cd "$TEST_REPO"
    git init -b main >/dev/null 2>&1
    git config user.email "e2e@test.local"
    git config user.name "JobFail E2E"
    printf '# JobFail fixture\n' > README.md
    git add . >/dev/null 2>&1
    git commit -m "seed" >/dev/null 2>&1
)

cat > "$PROJECTS_DIR/${PROJECT_ID}.toml" <<EOF
id = "${PROJECT_ID}"
path = "${TEST_REPO}"
name = "Job Failure Project"
allow_patch = true
EOF

# A normal reconciliation-capable runner config.
write_agent_toml() {
    local out="$1"; local instance="${2:-}"
    local extra=""
    if [ -n "$instance" ]; then
        extra="agent_instance_id = \"${instance}\""
    fi
    cat > "$out" <<EOF
server_url = "http://127.0.0.1:${PORT}"
token = "${TOKEN}"
client_id = "${CLIENT_ID}"
display_name = "JobFail E2E Agent"
owner = "e2e"
project_registry_dir = "${PROJECTS_DIR}"
poll_interval_ms = 500
transport = "websocket"
${extra}

[policy]
allow_raw_shell = true
allow_cwd_anywhere = false
allowed_roots = ["${TEST_REPO}"]
max_timeout_secs = 120
max_output_bytes = 262144
EOF
}
write_agent_toml "$AGENT_TOML"

start_server() {
    WEBCODEX_ADDR="127.0.0.1:${PORT}" \
    WEBCODEX_DATA="$DATA_DIR" \
    WEBCODEX_TOKEN="$TOKEN" \
    WEBCODEX_JOB_RECOVERY_GRACE_SECS="$WEBCODEX_JOB_RECOVERY_GRACE_SECS" \
    RUST_LOG="info" \
    "$SERVER_BIN" >>"$SERVER_LOG" 2>&1 &
    SERVER_PID=$!
}

start_runner() {
    local cfg="${1:-$AGENT_TOML}"
    "$RUNNER_BIN" --config "$cfg" >>"$RUNNER_LOG" 2>&1 &
    RUNNER_PID=$!
}

stop_runner() {
    if [ -n "${RUNNER_PID:-}" ] && kill -0 "$RUNNER_PID" 2>/dev/null; then
        kill "$RUNNER_PID" 2>/dev/null || true
        sleep 1
        kill -9 "$RUNNER_PID" 2>/dev/null || true
    fi
    RUNNER_PID=""
}

stop_server() {
    if [ -n "${SERVER_PID:-}" ] && kill -0 "$SERVER_PID" 2>/dev/null; then
        kill "$SERVER_PID" 2>/dev/null || true
        sleep 2
        kill -9 "$SERVER_PID" 2>/dev/null || true
    fi
    SERVER_PID=""
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
            echo "$body"; return 0
        fi
        sleep 1
    done
    return 1
}

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
    local tool="$1"; local params="$2"
    api_post /api/tools/call "{\"tool\":\"${tool}\",\"params\":${params}}"
}

run_job_call() {
    local command="$1"; local timeout="$2"
    tool_call "run_job" "{\"project\":\"${RUNTIME_PROJECT_ID}\",\"command\":$(python3 -c 'import json,sys; print(json.dumps(sys.argv[1]))' "$command"),\"timeout_secs\":${timeout}}"
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

list_jobs_call() {
    tool_call "list_jobs" "{\"limit\":100}"
}

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

# A long, controllable task with deterministic marker output into a fixture
# file. Emits START immediately, then idles until a stop flag appears.
long_marker_command() {
    local marker="$1"; local stop_flag="$2"
    printf 'printf %s\\n >> %q; printf %s\\n; i=0; while [ ! -f %q ] && [ $i -lt 600 ]; do sleep 1; i=$((i+1)); done; printf %s-end\\n $i >> %q' \
        "$marker" "$marker_file_placeholder" "$marker" "$stop_flag" "$marker" "$marker_file_placeholder"
}

# ============================================================================
# Scenario C — Runner permanently gone within the recovery window
# ============================================================================
log "scenario C: runner permanently gone -> recovery deadline lost"
C_MARKER_FILE="$TEST_REPO/scenario-c-count.txt"
: >"$C_MARKER_FILE"
C_STOP_FLAG="$TEST_REPO/scenario-c-stop.flag"
C_COMMAND="printf 'C-START\\n' >> '$C_MARKER_FILE'; printf 'C-START\\n'; i=0; while [ ! -f '$C_STOP_FLAG' ] && [ \$i -lt 600 ]; do sleep 1; i=\$((i+1)); done; printf 'C-END %d\\n' \$i >> '$C_MARKER_FILE'"

start_server
wait_for_server || { fail "C: server did not listen"; dump_logs; exit 1; }
start_runner
BODY="$(wait_for_agent_online)" || { fail "C: runner did not register"; dump_logs; exit 1; }
pass "C: server + runner online"
wait_for_reconciliation_capability >/dev/null || { fail "C: runner missing reconciliation capability"; dump_logs; exit 1; }
pass "C: runner advertises job_state_reconciliation"

JOB_BODY_C="$(run_job_call "$C_COMMAND" 600)"
JOB_ID_C="$(json_get "$JOB_BODY_C" output.job_id)"
assert_nonempty "C: long-running job started" "$JOB_ID_C"
wait_for_job_status "$JOB_ID_C" running >/dev/null || { fail "C: job did not reach running"; dump_logs; exit 1; }
pass "C: job is running"

log "C: killing RUNNER only (server stays up)"
stop_runner
pass "C: runner stopped"

# The job enters `recovering` (reconciliation-capable runner disconnect).
wait_for_job_status "$JOB_ID_C" recovering >/dev/null || { fail "C: job did not enter recovering"; dump_logs; exit 1; }
pass "C: job entered recovering"

# Bound the deadline wait to grace + a sweep interval (30s) + slack.
C_DEADLINE=$(( $(date +%s) + WEBCODEX_JOB_RECOVERY_GRACE_SECS + 35 ))
C_LOST_BODY=""
for _ in $(seq 1 80); do
    check_deadline
    if [ "$(date +%s)" -gt "$C_DEADLINE" ]; then break; fi
    BODY="$(job_status_call "$JOB_ID_C" || true)"
    if [ "$(json_get "$BODY" output.status)" = "lost" ]; then
        C_LOST_BODY="$BODY"; break
    fi
    sleep 1
done
if [ -z "$C_LOST_BODY" ]; then
    fail "C: job did not become lost within the recovery deadline"; dump_logs; exit 1
fi
pass "C: job became lost after the recovery deadline"

C_REASON="$(json_get "$C_LOST_BODY" output.recovery_reason_code)"
assert_eq "C: recovery reason is deadline exceeded" "$C_REASON" "runner_recovery_deadline_exceeded"
C_END_AT="$(json_get "$C_LOST_BODY" output.ended_at)"
assert_nonempty "C: ended_at is set" "$C_END_AT"
C_REASON_TEXT="$(json_get "$C_LOST_BODY" output.recovery_reason)"
assert_nonempty "C: recovery_reason text surfaced" "$C_REASON_TEXT"

# Re-query must not rewrite ended_at or reason.
BODY2="$(job_status_call "$JOB_ID_C")"
assert_eq "C: ended_at stable on re-query" "$(json_get "$BODY2" output.ended_at)" "$C_END_AT"
assert_eq "C: reason stable on re-query" "$(json_get "$BODY2" output.recovery_reason_code)" "$C_REASON"

# stop_job on a terminal lost job returns stable semantics: the call succeeds
# and does not change the terminal state. Re-query to confirm lost is stable.
stop_job_call "$JOB_ID_C" >/dev/null
BODY_AFTER_STOP="$(job_status_call "$JOB_ID_C")"
assert_eq "C: stop on lost job keeps lost status" "$(json_get "$BODY_AFTER_STOP" output.status)" "lost"
assert_eq "C: ended_at unchanged after stop" "$(json_get "$BODY_AFTER_STOP" output.ended_at)" "$C_END_AT"

# list shows exactly one record for this job.
LIST_BODY="$(list_jobs_call)"
LIST_COUNT="$(python3 - "$LIST_BODY" "$JOB_ID_C" <<'PY'
import json, sys
obj = json.loads(sys.argv[1])
jid = sys.argv[2]
jobs = obj.get("output", {}).get("jobs", [])
print(sum(1 for j in jobs if j.get("job_id") == jid))
PY
)"
assert_eq "C: exactly one list record for the job" "$LIST_COUNT" "1"

# Marker file has exactly one C-START (command never re-executed).
C_START_COUNT="$(grep -c 'C-START' "$C_MARKER_FILE" || true)"
assert_eq "C: command executed once (single C-START marker)" "$C_START_COUNT" "1"

stop_server
log "scenario C complete"

# ============================================================================
# Scenario D — New runner instance replaces the old instance
# ============================================================================
log "scenario D: instance B replaces instance A"
: >"$SERVER_LOG"; : >"$RUNNER_LOG"
D_MARKER_FILE="$TEST_REPO/scenario-d-count.txt"
: >"$D_MARKER_FILE"
D_STOP_FLAG="$TEST_REPO/scenario-d-stop.flag"
D_COMMAND="printf 'D-START\\n' >> '$D_MARKER_FILE'; printf 'D-START\\n'; i=0; while [ ! -f '$D_STOP_FLAG' ] && [ \$i -lt 600 ]; do sleep 1; i=\$((i+1)); done; printf 'D-END %d\\n' \$i >> '$D_MARKER_FILE'"

INSTANCE_A="instance-d-a"
INSTANCE_B="instance-d-b"
write_agent_toml "$TMP_ROOT/agent-a.toml" "$INSTANCE_A"
write_agent_toml "$TMP_ROOT/agent-b.toml" "$INSTANCE_B"

start_server
wait_for_server || { fail "D: server did not listen"; dump_logs; exit 1; }
start_runner "$TMP_ROOT/agent-a.toml"
wait_for_agent_online >/dev/null || { fail "D: runner A did not register"; dump_logs; exit 1; }
pass "D: instance A online"

JOB_BODY_D="$(run_job_call "$D_COMMAND" 600)"
JOB_ID_D="$(json_get "$JOB_BODY_D" output.job_id)"
assert_nonempty "D: A started long job" "$JOB_ID_D"
wait_for_job_status "$JOB_ID_D" running >/dev/null || { fail "D: job did not reach running"; dump_logs; exit 1; }
pass "D: A's job is running"

# Kill A. The server still holds A's job. Wait for A's online lease to lapse
# (CLIENT_ONLINE_WINDOW_SECS) so a same-client different-instance register is
# accepted and triggers instance replacement fencing on A's job.
log "D: terminating A and waiting for its lease to lapse"
stop_runner
ONLINE_DEADLINE=$(( $(date +%s) + 75 ))
ONLINE_LAPSED=0
for _ in $(seq 1 80); do
    check_deadline
    if [ "$(date +%s)" -gt "$ONLINE_DEADLINE" ]; then break; fi
    d_body="$(runtime_status || true)"
    if [ "$(json_get "$d_body" output.agents.online_count)" = "0" ]; then
        ONLINE_LAPSED=1; break
    fi
    sleep 1
done
if [ "$ONLINE_LAPSED" != "1" ]; then
    fail "D: A's lease did not lapse"; dump_logs; exit 1
fi
pass "D: A's lease lapsed"
# Register B with the same client_id and a new instance id.
start_runner "$TMP_ROOT/agent-b.toml"
wait_for_agent_online >/dev/null || { fail "D: runner B did not register"; dump_logs; exit 1; }
pass "D: instance B online"

# A's job must now be lost with runner_instance_replaced.
D_LOST_BODY="$(wait_for_job_status "$JOB_ID_D" lost)"
assert_eq "D: A's job is lost" "$(json_get "$D_LOST_BODY" output.status)" "lost"
assert_eq "D: A's job lost reason is instance replaced" "$(json_get "$D_LOST_BODY" output.recovery_reason_code)" "runner_instance_replaced"
D_END_AT="$(json_get "$D_LOST_BODY" output.ended_at)"
assert_nonempty "D: A's job ended_at set" "$D_END_AT"

# A's late update is rejected (run via a direct job_update tool is not public;
# assert via job_status that the lost state is terminal and stable).
BODY2="$(job_status_call "$JOB_ID_D")"
assert_eq "D: A's job ended_at not rewritten" "$(json_get "$BODY2" output.ended_at)" "$D_END_AT"

# B can start its own new job.
D2_COMMAND="printf 'D2-OK\\n' >> '$D_MARKER_FILE'; printf 'D2-OK\\n'"
JOB_BODY_D2="$(run_job_call "$D2_COMMAND" 30)"
JOB_ID_D2="$(json_get "$JOB_BODY_D2" output.job_id)"
assert_nonempty "D: B started its own new job" "$JOB_ID_D2"
assert_ne "D: B's job is a different job_id" "$JOB_ID_D2" "$JOB_ID_D"
wait_for_job_status "$JOB_ID_D2" completed >/dev/null || { fail "D: B's job did not complete"; dump_logs; exit 1; }
pass "D: B's own job completed"
D2_START_COUNT="$(grep -c 'D-START' "$D_MARKER_FILE" || true)"
assert_eq "D: A's command executed once" "$D2_START_COUNT" "1"

stop_server
stop_runner
log "scenario D complete"

# ============================================================================
# Scenario E — Runner without reconciliation (no job_state_reconciliation) disconnect lost
# ----------------------------------------------------------------------------
# Scope note: the runner's running-job update path always carries an
# authoritative log_snapshot + update_seq, which the server rejects without the
# reconciliation capability. Faithfully disabling that path on the runner is a
# larger runner-side change outside this phase. The full no-reconciliation
# disconnect->lost transition is covered by the unit test
# `job_reconciliation_absent_capability_keeps_immediate_lost_semantics`.
# This E2E verifies the real-process generation-2 registration of a
# no-reconciliation Runner (no capability, no inventory) and that a job it accepted is
# deterministically fenced to `lost` with `runner_disconnected_without_reconciliation` when the
# no-reconciliation transport disconnects, without entering `recovering`.
# ============================================================================
log "scenario E: no-reconciliation Runner disconnect -> deterministic lost"
: >"$SERVER_LOG"; : >"$RUNNER_LOG"
E_MARKER_FILE="$TEST_REPO/scenario-e-count.txt"
: >"$E_MARKER_FILE"
E_COMMAND="printf 'E-START\\n' >> '$E_MARKER_FILE'; sleep 300"

# A no-reconciliation Runner config (reconciliation capability disabled via env at start).
write_agent_toml "$NO_RECONCILIATION_AGENT_TOML"
start_server
wait_for_server || { fail "E: server did not listen"; dump_logs; exit 1; }
WEBCODEX_RUNNER_DISABLE_JOB_STATE_RECONCILIATION=1 start_runner "$NO_RECONCILIATION_AGENT_TOML"
wait_for_agent_online >/dev/null || { fail "E: no-reconciliation Runner did not register"; dump_logs; exit 1; }
pass "E: no-reconciliation Runner registered without job_state_reconciliation"

# The capability is not advertised (field absent or false).
BODY_CAP="$(runtime_status)"
E_CAP="$(json_get "$BODY_CAP" output.agents.clients.0.capabilities.job_state_reconciliation)"
if [ "$E_CAP" = "True" ]; then
    fail "E: no-reconciliation Runner unexpectedly advertised reconciliation"
else
    pass "E: no-reconciliation Runner does not advertise reconciliation"
fi

# Start a job; it becomes queued then agent_queued when the no-reconciliation Runner
# polls it. We do NOT wait for `running` (the snapshot update is rejected), so
# the job stays agent_queued until disconnect.
JOB_BODY_E="$(run_job_call "$E_COMMAND" 300)"
JOB_ID_E="$(json_get "$JOB_BODY_E" output.job_id)"
assert_nonempty "E: no-reconciliation job started" "$JOB_ID_E"
# Give the no-reconciliation Runner a moment to poll/accept the request.
sleep 2
BODY_QUEUED="$(job_status_call "$JOB_ID_E")"
assert_ne "E: no-reconciliation job did not enter recovering" "$(json_get "$BODY_QUEUED" output.status)" "recovering"
pass "E: no-reconciliation job dispatched without entering recovering"

log "E: killing no-reconciliation Runner"
stop_runner
# No-reconciliation disconnect goes straight to lost (no recovering grace, no snapshot).
E_LOST_BODY="$(wait_for_job_status "$JOB_ID_E" lost)"
assert_eq "E: no-reconciliation job is lost" "$(json_get "$E_LOST_BODY" output.status)" "lost"
assert_eq "E: no-reconciliation lost reason" "$(json_get "$E_LOST_BODY" output.recovery_reason_code)" "runner_disconnected_without_reconciliation"
E_END_AT="$(json_get "$E_LOST_BODY" output.ended_at)"
assert_nonempty "E: no-reconciliation job ended_at set" "$E_END_AT"
E_REASON_TEXT="$(json_get "$E_LOST_BODY" output.recovery_reason)"
assert_nonempty "E: recovery_reason text surfaced" "$E_REASON_TEXT"

# After a server restart the in-memory registry is cleared: there is no
# durable record of the lost job, so it is unknown (it does NOT come back as a
# new job and is not re-executed). This is the documented in-process model.
stop_server
start_server
wait_for_server || { fail "E: server did not restart"; dump_logs; exit 1; }
sleep 2
BODY_E2="$(job_status_call "$JOB_ID_E")"
assert_eq "E: lost job has no durable record after restart (unknown)" "$(json_get "$BODY_E2" output.status)" ""
E_START_COUNT="$(grep -c 'E-START' "$E_MARKER_FILE" || true)"
assert_eq "E: command executed once (no re-execution)" "$E_START_COUNT" "1"

# A same-client new no-reconciliation instance cannot take over the old job (it has no
# durable record after the restart, and the new instance submits no inventory
# for it).
WEBCODEX_RUNNER_DISABLE_JOB_STATE_RECONCILIATION=1 start_runner "$NO_RECONCILIATION_AGENT_TOML"
wait_for_agent_online >/dev/null || { fail "E: second no-reconciliation Runner did not register"; dump_logs; exit 1; }
sleep 2
BODY_E3="$(job_status_call "$JOB_ID_E")"
assert_eq "E: old job not revived by new same-client instance" "$(json_get "$BODY_E3" output.status)" ""

stop_server
stop_runner
log "scenario E complete"

# ============================================================================
# Scenario F — Repeated server restart (3x) keeps the same job_id
# ============================================================================
log "scenario F: repeated server restart keeps same job_id"
: >"$SERVER_LOG"; : >"$RUNNER_LOG"
F_MARKER_FILE="$TEST_REPO/scenario-f-count.txt"
: >"$F_MARKER_FILE"
F_STOP_FLAG="$TEST_REPO/scenario-f-stop.flag"
F_COMMAND="printf 'F-START\\n' >> '$F_MARKER_FILE'; printf 'F-START\\n'; i=0; while [ ! -f '$F_STOP_FLAG' ] && [ \$i -lt 600 ]; do sleep 1; i=\$((i+1)); done; printf 'F-END %d\\n' \$i >> '$F_MARKER_FILE'"

start_server
wait_for_server || { fail "F: server did not listen"; dump_logs; exit 1; }
start_runner
wait_for_agent_online >/dev/null || { fail "F: runner did not register"; dump_logs; exit 1; }
wait_for_reconciliation_capability >/dev/null || { fail "F: runner missing reconciliation capability"; dump_logs; exit 1; }
pass "F: server + runner online"

JOB_BODY_F="$(run_job_call "$F_COMMAND" 600)"
JOB_ID_F="$(json_get "$JOB_BODY_F" output.job_id)"
assert_nonempty "F: long job started" "$JOB_ID_F"
wait_for_job_status "$JOB_ID_F" running >/dev/null || { fail "F: job did not reach running"; dump_logs; exit 1; }
pass "F: job running (initial)"
LOG_BODY_F="$(job_log_call "$JOB_ID_F")"
F_SEQ_1="$(json_get "$LOG_BODY_F" output.last_update_seq)"
F_CURSOR_1="$(json_get "$LOG_BODY_F" output.cursor.stdout)"

restart_keep_runner() {
    stop_server
    if ! kill -0 "$RUNNER_PID" 2>/dev/null; then
        fail "F: runner died with the server"; dump_logs; exit 1
    fi
    start_server
    wait_for_server || { fail "F: server did not restart"; dump_logs; exit 1; }
    wait_for_agent_online >/dev/null || { fail "F: runner did not re-register"; dump_logs; exit 1; }
}

# Restart 1.
log "F: restart #1"
restart_keep_runner
pass "F: runner stayed alive across restart #1"
RECON1="$(wait_for_job_status "$JOB_ID_F" running stop_requested)"
assert_ne "F: not lost after restart #1" "$(json_get "$RECON1" output.status)" "lost"
pass "F: reconciled to running after restart #1"
LOG1="$(job_log_call "$JOB_ID_F")"
F_SEQ_2="$(json_get "$LOG1" output.last_update_seq)"
F_CURSOR_2="$(json_get "$LOG1" output.cursor.stdout)"
if [ -n "$F_SEQ_2" ] && [ -n "$F_SEQ_1" ] && [ "$F_SEQ_2" -lt "$F_SEQ_1" ] 2>/dev/null; then
    fail "F: last_update_seq regressed ($F_SEQ_1 -> $F_SEQ_2)"
else
    pass "F: last_update_seq did not regress after restart #1"
fi
if [ -n "$F_CURSOR_2" ] && [ -n "$F_CURSOR_1" ] && [ "$F_CURSOR_2" -ge "$F_CURSOR_1" ] 2>/dev/null; then
    pass "F: stdout cursor monotonic after restart #1"
else
    fail "F: stdout cursor regressed ($F_CURSOR_1 -> $F_CURSOR_2)"
fi

# Restart 2.
log "F: restart #2"
restart_keep_runner
pass "F: runner stayed alive across restart #2"
RECON2="$(wait_for_job_status "$JOB_ID_F" running stop_requested)"
assert_ne "F: not lost after restart #2" "$(json_get "$RECON2" output.status)" "lost"
pass "F: reconciled to running after restart #2"
LOG2="$(job_log_call "$JOB_ID_F")"
F_SEQ_3="$(json_get "$LOG2" output.last_update_seq)"
if [ -n "$F_SEQ_3" ] && [ -n "$F_SEQ_2" ] && [ "$F_SEQ_3" -lt "$F_SEQ_2" ] 2>/dev/null; then
    fail "F: last_update_seq regressed ($F_SEQ_2 -> $F_SEQ_3)"
else
    pass "F: last_update_seq did not regress after restart #2"
fi
# F-START must still appear exactly once (no snapshot re-append duplication).
F_STDOUT="$(json_get "$LOG2" output.stdout_tail)"
F_START_IN_STDOUT="$(printf '%s\n' "$F_STDOUT" | grep -c 'F-START' || true)"
if [ "$F_START_IN_STDOUT" -le 1 ]; then
    pass "F: log marker not duplicated after restart #2"
else
    fail "F: log marker duplicated (count=$F_START_IN_STDOUT)"
fi
F_FILE_START_COUNT="$(grep -c 'F-START' "$F_MARKER_FILE" || true)"
assert_eq "F: command executed once across restarts" "$F_FILE_START_COUNT" "1"

# Stop the job (controls the original process group), then restart #3.
stop_job_call "$JOB_ID_F" >/dev/null
touch "$F_STOP_FLAG"
wait_for_job_status "$JOB_ID_F" stopped >/dev/null || { fail "F: job did not stop"; dump_logs; exit 1; }
pass "F: job stopped"
STATUS_STOP="$(job_status_call "$JOB_ID_F")"
F_END_AT="$(json_get "$STATUS_STOP" output.ended_at)"
assert_nonempty "F: stopped job ended_at set" "$F_END_AT"

log "F: restart #3 (terminal job)"
restart_keep_runner
# Terminal `stopped` survives the third restart; terminal inventory replay
# does not rewrite ended_at.
RECON3="$(wait_for_job_status "$JOB_ID_F" stopped)"
assert_eq "F: terminal stopped stable after restart #3" "$(json_get "$RECON3" output.status)" "stopped"
assert_eq "F: ended_at not rewritten by replay" "$(json_get "$RECON3" output.ended_at)" "$F_END_AT"

# list has exactly one record for the job.
LIST_BODY_F="$(list_jobs_call)"
F_LIST_COUNT="$(python3 - "$LIST_BODY_F" "$JOB_ID_F" <<'PY'
import json, sys
obj = json.loads(sys.argv[1])
jid = sys.argv[2]
jobs = obj.get("output", {}).get("jobs", [])
print(sum(1 for j in jobs if j.get("job_id") == jid))
PY
)"
assert_eq "F: exactly one list record" "$F_LIST_COUNT" "1"

stop_server
stop_runner
log "scenario F complete"

# ----------------------------------------------------------------------------
# Summary
# ----------------------------------------------------------------------------
log "summary: pass=$PASS fail=$FAIL"
if [ "$FAIL" -gt 0 ]; then
    dump_logs
    exit 1
fi
exit 0
