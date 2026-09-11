#!/usr/bin/env bash
set -uo pipefail

# Minimal WebCodex coding-loop eval harness.
#
# This harness measures deterministic runtime/tool-loop mechanics only. It
# starts a local WebCodex server and agent, exposes a disposable local project,
# runs three scripted cases through /api/tools/call, and emits a final JSON
# summary as the last stdout line.

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$PROJECT_DIR"

CARGO_BIN="${CARGO_BIN:-cargo}"
SERVER_BIN="${EVAL_SERVER_BIN:-}"
RUNNER_BIN="${EVAL_RUNNER_BIN:-}"
TOKEN="${EVAL_TOKEN:-eval-coding-loop-token}"
CLIENT_ID="${EVAL_CLIENT_ID:-eval-agent}"
PROJECT_ID="${EVAL_PROJECT_ID:-coding-loop-eval}"
TRANSPORT="${EVAL_TRANSPORT:-websocket}"
TIMEOUT_SECS="${EVAL_TIMEOUT_SECS:-240}"
RUNTIME_PROJECT_ID="agent:${CLIENT_ID}:${PROJECT_ID}"
EVAL_MODE="${EVAL_MODE:-compare}"

case "$EVAL_MODE" in
    baseline|guided|compare)
        ;;
    *)
        echo "[eval] EVAL_MODE must be one of: baseline, guided, compare" >&2
        exit 2
        ;;
esac

PASS=0
FAIL=0
SERVER_PID=""
RUNNER_PID=""
TMP_ROOT=""
DATA_DIR=""
PROJECTS_DIR=""
AGENT_TOML=""
TEST_REPO=""
SERVER_LOG=""
RUNNER_LOG=""
CASE_SUMMARIES_FILE=""
CASE_WARNINGS_FILE=""
LAST_BODY=""
LAST_SESSION_ID=""
START_EPOCH="$(date +%s)"

CASE_FLOW_KIND=""
CASE_NAME=""
CASE_LABEL=""
CASE_TOOL_CALLS=0
CASE_RAW_SHELL_CALLS=0
CASE_STRUCTURED_EDIT_CALLS=0
CASE_FAILED_TOOL_CALLS=0
CASE_RECOVERED_FAILED_TOOL_CALLS=0
CASE_HANDOFF_AVAILABLE=0
CASE_VALIDATION_AVAILABLE=0
CASE_VALIDATION_EVENTS_TOTAL=0
CASE_VALIDATION_SUCCESSES=0
CASE_VALIDATION_FAILURES=0
CASE_VALIDATION_PARSER_AVAILABLE=0
CASE_VALIDATION_DIAGNOSTICS_AVAILABLE=0
CASE_VALIDATION_DIAGNOSTICS_TOTAL=0
CASE_FINISH_CALLED=0
CASE_FINISH_SUCCEEDED=0
CASE_ASSERT_FAILURES=0
CASE_WORKSPACE_CLEAN=false

log() { printf '[eval] %s\n' "$*"; }

elapsed() {
    echo $(( $(date +%s) - START_EPOCH ))
}

remaining_time() {
    local used
    used="$(elapsed)"
    echo $(( TIMEOUT_SECS - used ))
}

check_deadline() {
    if [ "$(remaining_time)" -le 0 ]; then
        log "overall timeout (${TIMEOUT_SECS}s) exceeded"
        exit 1
    fi
}

print_logs_hint() {
    cat >&2 <<EOF

[eval] ---- log locations ----
[eval] server log: ${SERVER_LOG:-<none>}
[eval] runner log:  ${RUNNER_LOG:-<none>}
[eval] temp root:  ${TMP_ROOT:-<none>}
EOF
}

cleanup_background_processes() {
    if [ -n "${RUNNER_PID:-}" ] && kill -0 "$RUNNER_PID" 2>/dev/null; then
        kill "$RUNNER_PID" 2>/dev/null || true
        sleep 1
        kill -9 "$RUNNER_PID" 2>/dev/null || true
        wait "$RUNNER_PID" 2>/dev/null || true
    fi
    if [ -n "${SERVER_PID:-}" ] && kill -0 "$SERVER_PID" 2>/dev/null; then
        kill "$SERVER_PID" 2>/dev/null || true
        sleep 1
        kill -9 "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
    fi
}

on_abort() {
    local status=$?
    cleanup_background_processes
    if [ "$status" -ne 0 ]; then
        print_logs_hint
    fi
}

trap on_abort INT TERM EXIT

if [ "${EVAL_SKIP_RUN:-0}" = "1" ]; then
    log "EVAL_SKIP_RUN=1: skipping service startup and eval execution"
    python3 - "$EVAL_MODE" <<'PY'
import json
import sys

cases = [
    {
        "case": "inspect_only",
        "planned": True,
        "validation_available": None,
        "validation_events_total": 0,
        "validation_successes": 0,
        "validation_failures": 0,
        "validation_parser_available": None,
        "validation_diagnostics_available": None,
        "validation_diagnostics_total": 0,
    },
    {
        "case": "small_structured_line_edit",
        "planned": True,
        "validation_available": None,
        "validation_events_total": 0,
        "validation_successes": 0,
        "validation_failures": 0,
        "validation_parser_available": None,
        "validation_diagnostics_available": None,
        "validation_diagnostics_total": 0,
    },
    {
        "case": "failed_call_recovery",
        "planned": True,
        "validation_available": None,
        "validation_events_total": 0,
        "validation_successes": 0,
        "validation_failures": 0,
        "validation_parser_available": None,
        "validation_diagnostics_available": None,
        "validation_diagnostics_total": 0,
    },
]
mode = sys.argv[1]

def flow(name):
    return {
        "mode": name,
        "skipped": True,
        "cases_total": 3,
        "cases_passed": 0,
        "cases_failed": 0,
        "tool_calls_total": 0,
        "raw_shell_calls": 0,
        "raw_shell_ratio": 0.0,
        "structured_edit_calls": 0,
        "failed_tool_calls": 0,
        "recovered_failed_tool_calls": 0,
        "workspace_clean_after_each_case": None,
        "handoff_available_rate": None,
        "validation_available_rate": None,
        "validation_events_total": 0,
        "validation_successes": 0,
        "validation_failures": 0,
        "validation_parser_available_rate": None,
        "validation_diagnostics_available_rate": None,
        "validation_diagnostics_total": 0,
        "finish_coding_task_success_rate": None,
        "cases": cases,
    }

comparison = {
    "guided_minus_baseline_tool_calls": None,
    "guided_minus_baseline_raw_shell_ratio": None,
    "guided_minus_baseline_structured_edit_calls": None,
    "guided_handoff_available_delta": None,
    "guided_minus_baseline_validation_available_rate": None,
    "guided_minus_baseline_validation_parser_available_rate": None,
    "guided_minus_baseline_validation_diagnostics_available_rate": None,
    "guided_minus_baseline_validation_events_total": None,
    "guided_cleanup_delta": None,
}

baseline = flow("baseline") if mode in ("baseline", "compare") else None
guided = flow("guided") if mode in ("guided", "compare") else None
selected = [summary for summary in (baseline, guided) if summary is not None]

summary = {
    "mode": mode,
    "skipped": True,
    "cases_total": sum(item["cases_total"] for item in selected),
    "cases_passed": sum(item["cases_passed"] for item in selected),
    "cases_failed": sum(item["cases_failed"] for item in selected),
    "baseline": baseline,
    "guided": guided,
    "comparison": comparison if mode == "compare" else None,
}
print(json.dumps(summary, separators=(",", ":"), sort_keys=True))
PY
    trap - INT TERM EXIT
    exit 0
fi

if ! command -v curl >/dev/null 2>&1; then
    echo "[eval] curl is required" >&2
    exit 2
fi
if ! command -v python3 >/dev/null 2>&1; then
    echo "[eval] python3 is required for script-layer JSON parsing" >&2
    exit 2
fi
if ! command -v git >/dev/null 2>&1; then
    echo "[eval] git is required" >&2
    exit 2
fi
if ! command -v "$CARGO_BIN" >/dev/null 2>&1; then
    echo "[eval] ${CARGO_BIN} is required" >&2
    exit 2
fi

find_free_port() {
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
    local budget="${2:-60}"
    local tries=0
    while [ "$tries" -lt "$budget" ]; do
        check_deadline
        if (echo >/dev/tcp/127.0.0.1/"$port") 2>/dev/null; then
            return 0
        fi
        tries=$((tries + 1))
        sleep 1
    done
    return 1
}

api_post() {
    local path="$1"
    local body="${2:-}"
    if [ -z "$body" ]; then
        body="{}"
    fi
    curl -sS --max-time 20 \
        -H "Authorization: Bearer ${TOKEN}" \
        -H "Content-Type: application/json" \
        -X POST "http://127.0.0.1:${PORT}${path}" \
        -d "$body" 2>/dev/null
}

json_get() {
    local json="$1"
    local path="$2"
    python3 - "$json" "$path" <<'PY'
import json
import sys

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
            print("")
            sys.exit(0)
    elif isinstance(cur, dict):
        cur = cur.get(part)
        if cur is None:
            print("")
            sys.exit(0)
    else:
        print("")
        sys.exit(0)

if isinstance(cur, (dict, list)):
    print(json.dumps(cur))
elif cur is None:
    print("")
else:
    print(cur)
PY
}

json_body() {
    local tool="$1"
    local params="$2"
    python3 - "$tool" "$params" <<'PY'
import json
import sys

tool = sys.argv[1]
params = json.loads(sys.argv[2])
print(json.dumps({"tool": tool, "params": params}, separators=(",", ":")))
PY
}

params_work_on_project() {
    local instruction="$1"
    python3 - "$RUNTIME_PROJECT_ID" "$instruction" <<'PY'
import json
import sys

print(json.dumps({
    "project": sys.argv[1],
    "instruction": sys.argv[2],
}, separators=(",", ":")))
PY
}

params_start_session() {
    local title="$1"
    python3 - "$RUNTIME_PROJECT_ID" "$title" <<'PY'
import json
import sys

print(json.dumps({
    "project": sys.argv[1],
    "title": sys.argv[2],
    "mode": "normal",
}, separators=(",", ":")))
PY
}

params_finish_task() {
    local session_id="$1"
    python3 - "$RUNTIME_PROJECT_ID" "$session_id" <<'PY'
import json
import sys

print(json.dumps({
    "project": sys.argv[1],
    "session_id": sys.argv[2],
    "include_diff": True,
    "include_hygiene": True,
    "include_handoff": True,
    "include_validation_summary": True,
}, separators=(",", ":")))
PY
}

params_session_handoff() {
    local session_id="$1"
    python3 - "$RUNTIME_PROJECT_ID" "$session_id" <<'PY'
import json
import sys

print(json.dumps({
    "session_id": sys.argv[2],
    "project": sys.argv[1],
    "include_workspace": True,
    "include_checkpoints": False,
    "include_validation": True,
    "limit": 50,
}, separators=(",", ":")))
PY
}

params_workspace_hygiene() {
    local session_id="$1"
    python3 - "$RUNTIME_PROJECT_ID" "$session_id" <<'PY'
import json
import sys

print(json.dumps({
    "project": sys.argv[1],
    "session_id": sys.argv[2],
    "max_findings": 50,
    "include_tracked": False,
}, separators=(",", ":")))
PY
}

call_tool() {
    local tool="$1"
    local params
    local body

    if [ "$#" -ge 2 ] && [ -n "$2" ]; then
        params="$2"
    else
        params="{}"
    fi

    body="$(json_body "$tool" "$params")"

    CASE_TOOL_CALLS=$((CASE_TOOL_CALLS + 1))
    case "$tool" in
        run_shell|run_job)
            CASE_RAW_SHELL_CALLS=$((CASE_RAW_SHELL_CALLS + 1))
            ;;
    esac
    case "$tool" in
        apply_text_edits)
            CASE_STRUCTURED_EDIT_CALLS=$((CASE_STRUCTURED_EDIT_CALLS + 1))
            ;;
    esac
    if [ "$tool" = "finish_coding_task" ]; then
        CASE_FINISH_CALLED=$((CASE_FINISH_CALLED + 1))
    fi

    LAST_BODY="$(api_post /api/tools/call "$body" || true)"

    if [ "$(json_get "$LAST_BODY" success)" != "True" ]; then
        CASE_FAILED_TOOL_CALLS=$((CASE_FAILED_TOOL_CALLS + 1))
    elif [ "$tool" = "finish_coding_task" ]; then
        CASE_FINISH_SUCCEEDED=$((CASE_FINISH_SUCCEEDED + 1))
    fi
}

case_warn() {
    local message="$1"
    python3 - "$message" <<'PY' >>"$CASE_WARNINGS_FILE"
import json
import sys

print(json.dumps(sys.argv[1]))
PY
}

case_ok() {
    PASS=$((PASS + 1))
    log "[ok]   ${CASE_LABEL}: $*"
}

case_fail() {
    FAIL=$((FAIL + 1))
    CASE_ASSERT_FAILURES=$((CASE_ASSERT_FAILURES + 1))
    log "[FAIL] ${CASE_LABEL}: $*"
    case_warn "$*"
}

assert_success() {
    local label="$1"
    local body="$2"
    if [ "$(json_get "$body" success)" = "True" ]; then
        case_ok "$label"
    else
        case_fail "$label (body: ${body:0:240})"
    fi
}

assert_failure() {
    local label="$1"
    local body="$2"
    if [ "$(json_get "$body" success)" != "True" ] && [ -n "$(json_get "$body" error)" ]; then
        case_ok "$label"
    else
        case_fail "$label (expected controlled failure; body: ${body:0:240})"
    fi
}

assert_failure_error_kind() {
    local label="$1"
    local body="$2"
    local expected="$3"
    if python3 - "$body" "$expected" <<'PY'
import json
import sys

try:
    data = json.loads(sys.argv[1])
except Exception:
    sys.exit(1)

output = data.get("output") or {}
error = data.get("error")
ok = (
    data.get("success") is False
    and output.get("error_kind") == sys.argv[2]
    and isinstance(error, str)
    and bool(error)
)
sys.exit(0 if ok else 1)
PY
    then
        case_ok "$label"
    else
        case_fail "$label (expected error_kind=$expected; body: ${body:0:240})"
    fi
}

workspace_clean_local() {
    local status
    status="$(git -C "$TEST_REPO" status --porcelain --untracked-files=normal 2>/dev/null || true)"
    [ -z "$status" ]
}

cleanup_temp_repo_worktree() {
    git -C "$TEST_REPO" restore --staged --worktree . >/dev/null 2>&1 || true
    git -C "$TEST_REPO" clean -fd >/dev/null 2>&1 || true
    rm -rf "$TEST_REPO/target" >/dev/null 2>&1 || true
}

begin_case() {
    CASE_FLOW_KIND="$1"
    CASE_NAME="$2"
    CASE_LABEL="${CASE_FLOW_KIND}.${CASE_NAME}"
    CASE_TOOL_CALLS=0
    CASE_RAW_SHELL_CALLS=0
    CASE_STRUCTURED_EDIT_CALLS=0
    CASE_FAILED_TOOL_CALLS=0
    CASE_RECOVERED_FAILED_TOOL_CALLS=0
    CASE_HANDOFF_AVAILABLE=0
    CASE_VALIDATION_AVAILABLE=0
    CASE_VALIDATION_EVENTS_TOTAL=0
    CASE_VALIDATION_SUCCESSES=0
    CASE_VALIDATION_FAILURES=0
    CASE_VALIDATION_PARSER_AVAILABLE=0
    CASE_VALIDATION_DIAGNOSTICS_AVAILABLE=0
    CASE_VALIDATION_DIAGNOSTICS_TOTAL=0
    CASE_FINISH_CALLED=0
    CASE_FINISH_SUCCEEDED=0
    CASE_ASSERT_FAILURES=0
    CASE_WORKSPACE_CLEAN=false
    CASE_WARNINGS_FILE="$TMP_ROOT/${CASE_LABEL}.warnings.jsonl"
    : >"$CASE_WARNINGS_FILE"
    log "---- case: ${CASE_LABEL} ----"
    cleanup_temp_repo_worktree
    if workspace_clean_local; then
        case_ok "starts from a clean temporary worktree"
    else
        case_fail "temporary worktree is dirty before case"
    fi
}

record_case_summary() {
    local passed="$1"
    local workspace_clean="$2"
    python3 - \
        "$CASE_FLOW_KIND" \
        "$CASE_NAME" \
        "$passed" \
        "$CASE_TOOL_CALLS" \
        "$CASE_RAW_SHELL_CALLS" \
        "$CASE_STRUCTURED_EDIT_CALLS" \
        "$CASE_FAILED_TOOL_CALLS" \
        "$CASE_RECOVERED_FAILED_TOOL_CALLS" \
        "$CASE_HANDOFF_AVAILABLE" \
        "$workspace_clean" \
        "$CASE_FINISH_CALLED" \
        "$CASE_FINISH_SUCCEEDED" \
        "$CASE_VALIDATION_AVAILABLE" \
        "$CASE_VALIDATION_EVENTS_TOTAL" \
        "$CASE_VALIDATION_SUCCESSES" \
        "$CASE_VALIDATION_FAILURES" \
        "$CASE_VALIDATION_PARSER_AVAILABLE" \
        "$CASE_VALIDATION_DIAGNOSTICS_AVAILABLE" \
        "$CASE_VALIDATION_DIAGNOSTICS_TOTAL" \
        "$CASE_WARNINGS_FILE" <<'PY' >>"$CASE_SUMMARIES_FILE"
import json
import sys

warnings = []
try:
    with open(sys.argv[20], "r", encoding="utf-8") as handle:
        warnings = [json.loads(line) for line in handle if line.strip()]
except FileNotFoundError:
    warnings = []

summary = {
    "mode": sys.argv[1],
    "case": sys.argv[2],
    "passed": sys.argv[3] == "true",
    "tool_calls": int(sys.argv[4]),
    "raw_shell_calls": int(sys.argv[5]),
    "structured_edit_calls": int(sys.argv[6]),
    "failed_tool_calls": int(sys.argv[7]),
    "recovered_failed_tool_calls": int(sys.argv[8]),
    "handoff_available": int(sys.argv[9]) > 0,
    "workspace_clean": sys.argv[10] == "true",
    "finish_coding_task_calls": int(sys.argv[11]),
    "finish_coding_task_successes": int(sys.argv[12]),
    "validation_available": int(sys.argv[13]) > 0,
    "validation_events_total": int(sys.argv[14]),
    "validation_successes": int(sys.argv[15]),
    "validation_failures": int(sys.argv[16]),
    "validation_parser_available": int(sys.argv[17]) > 0,
    "validation_diagnostics_available": int(sys.argv[18]) > 0,
    "validation_diagnostics_total": int(sys.argv[19]),
    "warnings": warnings,
}
print(json.dumps(summary, separators=(",", ":"), sort_keys=True))
PY
}

end_case() {
    cleanup_temp_repo_worktree
    if workspace_clean_local; then
        CASE_WORKSPACE_CLEAN=true
        case_ok "temporary worktree clean after cleanup"
    else
        CASE_WORKSPACE_CLEAN=false
        case_fail "temporary worktree is dirty after cleanup"
    fi

    if [ "$CASE_ASSERT_FAILURES" -eq 0 ] && [ "$CASE_WORKSPACE_CLEAN" = "true" ]; then
        record_case_summary true "$CASE_WORKSPACE_CLEAN"
    else
        record_case_summary false "$CASE_WORKSPACE_CLEAN"
    fi
}

assert_session_created() {
    local tool_name="$1"
    local body="$2"
    local session_path="$3"
    local session_id
    LAST_SESSION_ID=""
    assert_success "${tool_name} succeeds" "$body"
    session_id="$(json_get "$body" "$session_path")"
    if [[ "$session_id" == wc_sess_* ]]; then
        case_ok "${tool_name} returned session_id"
        LAST_SESSION_ID="$session_id"
    else
        case_fail "${tool_name} did not return a wc_sess_* session id"
    fi
}

start_case_session() {
    local flow_kind="$1"
    local title="$2"
    local params

    if [ "$flow_kind" = "guided" ]; then
        params="$(params_work_on_project "$title")"
        call_tool "work_on_project" "$params"
        assert_session_created "work_on_project" "$LAST_BODY" "output.session_id"
    else
        params="$(params_start_session "$title")"
        call_tool "start_session" "$params"
        assert_session_created "start_session" "$LAST_BODY" "output.session_id"
    fi
}

assert_handoff_available() {
    local label="$1"
    local body="$2"
    local handoff_path="$3"

    if python3 - "$body" "$handoff_path" <<'PY'
import json
import sys

try:
    data = json.loads(sys.argv[1])
except Exception:
    sys.exit(1)

cur = data
for part in sys.argv[2].split("."):
    if not part:
        continue
    if isinstance(cur, dict):
        cur = cur.get(part)
    else:
        cur = None
    if cur is None:
        break

ok = (
    data.get("success") is True
    and isinstance(cur, dict)
    and isinstance(cur.get("session_id"), str)
    and cur.get("session_id").startswith("wc_sess_")
)
sys.exit(0 if ok else 1)
PY
    then
        CASE_HANDOFF_AVAILABLE=1
        case_ok "$label"
    else
        case_fail "$label"
    fi
}

assert_handoff_failed_tool_metadata() {
    local label="$1"
    local body="$2"
    local handoff_path="$3"

    if python3 - "$body" "$handoff_path" <<'PY'
import json
import sys

try:
    data = json.loads(sys.argv[1])
except Exception:
    sys.exit(1)

cur = data
for part in sys.argv[2].split("."):
    if not part:
        continue
    if isinstance(cur, dict):
        cur = cur.get(part)
    else:
        cur = None
    if cur is None:
        break

handoff = cur if isinstance(cur, dict) else {}
counts = handoff.get("counts") or {}
recent = handoff.get("recent_failed_tools") or []
ok = (
    data.get("success") is True
    and counts.get("failed_tool_calls", 0) >= 1
    and any(item.get("tool_name") == "apply_text_edits" for item in recent if isinstance(item, dict))
)
sys.exit(0 if ok else 1)
PY
    then
        case_ok "$label"
    else
        case_fail "$label"
    fi
}

assert_finish_reports_changed_file() {
    local body="$1"

    if python3 - "$body" <<'PY'
import json
import sys

data = json.loads(sys.argv[1])
out = data.get("output") or {}
changes = ((out.get("changes") or {}).get("show_changes") or {})
files = changes.get("files") or []
ok = (
    data.get("success") is True
    and (out.get("workspace") or {}).get("clean") is False
    and any(item.get("path") == "src/lib.rs" for item in files if isinstance(item, dict))
)
sys.exit(0 if ok else 1)
PY
    then
        case_ok "finish_coding_task reports changed file"
    else
        case_fail "finish_coding_task did not report changed file"
    fi
}

capture_validation_metrics() {
    local body="$1"
    local validation_path="$2"
    local metrics

    metrics="$(python3 - "$body" "$validation_path" <<'PY'
import json
import sys

try:
    data = json.loads(sys.argv[1])
except Exception:
    print("0 0 0 0 0 0 0")
    sys.exit(0)

cur = data
for part in sys.argv[2].split("."):
    if not part:
        continue
    if isinstance(cur, dict):
        cur = cur.get(part)
    else:
        cur = None
    if cur is None:
        break

validation = cur if isinstance(cur, dict) else {}

def as_int(value):
    if isinstance(value, bool):
        return int(value)
    if isinstance(value, (int, float)):
        return int(value)
    return 0

available = 1 if validation.get("available") is True else 0
events_total = as_int(validation.get("events_total"))
successes = as_int(validation.get("successes"))
failures = as_int(validation.get("failures"))
parser = validation.get("parser") or {}
parser_available = 1 if parser.get("available") is True else 0
diagnostics_available = 0
diagnostics_total = 0
events = validation.get("events")
if not isinstance(events, list):
    events = []
for event in events:
    if not isinstance(event, dict):
        continue
    diagnostics = event.get("diagnostics")
    if not isinstance(diagnostics, dict):
        continue
    if diagnostics.get("available") is True:
        diagnostics_available = 1
        count = diagnostics.get("diagnostic_count")
        diagnostics_total += as_int(count) if count is not None else 1
print(
    f"{available} {events_total} {successes} {failures} "
    f"{parser_available} {diagnostics_available} {diagnostics_total}"
)
PY
)"
    set -- $metrics
    CASE_VALIDATION_AVAILABLE="${1:-0}"
    CASE_VALIDATION_EVENTS_TOTAL="${2:-0}"
    CASE_VALIDATION_SUCCESSES="${3:-0}"
    CASE_VALIDATION_FAILURES="${4:-0}"
    CASE_VALIDATION_PARSER_AVAILABLE="${5:-0}"
    CASE_VALIDATION_DIAGNOSTICS_AVAILABLE="${6:-0}"
    CASE_VALIDATION_DIAGNOSTICS_TOTAL="${7:-0}"
}

assert_validation_available() {
    local label="$1"
    local body="$2"
    local validation_path="$3"

    if python3 - "$body" "$validation_path" <<'PY'
import json
import sys

data = json.loads(sys.argv[1])
cur = data
for part in sys.argv[2].split("."):
    if not part:
        continue
    if isinstance(cur, dict):
        cur = cur.get(part)
    else:
        cur = None
    if cur is None:
        break

validation = cur if isinstance(cur, dict) else {}
parser = validation.get("parser") or {}
events = validation.get("events") or []
latest_success = validation.get("latest_success") or {}
parser_available = parser.get("available")
parser_ok = (
    parser.get("kind") == "structured_validation_parser"
    and isinstance(parser_available, bool)
)
ok = (
    data.get("success") is True
    and validation.get("available") is True
    and validation.get("source") == "session_ledger"
    and validation.get("events_total", 0) >= 1
    and latest_success.get("tool_name") == "cargo_check"
    and isinstance(events, list)
    and len(events) >= 1
    and parser_ok
)
sys.exit(0 if ok else 1)
PY
    then
        case_ok "$label"
    else
        case_fail "$label"
    fi
}

assert_validation_unavailable() {
    local label="$1"
    local body="$2"
    local validation_path="$3"

    if python3 - "$body" "$validation_path" <<'PY'
import json
import sys

data = json.loads(sys.argv[1])
cur = data
for part in sys.argv[2].split("."):
    if not part:
        continue
    if isinstance(cur, dict):
        cur = cur.get(part)
    else:
        cur = None
    if cur is None:
        break

validation = cur if isinstance(cur, dict) else {}
parser = validation.get("parser") or {}
ok = (
    data.get("success") is True
    and validation.get("available") is False
    and validation.get("source") == "session_ledger"
    and validation.get("events_total", 0) == 0
    and parser.get("available") is False
)
sys.exit(0 if ok else 1)
PY
    then
        case_ok "$label"
    else
        case_fail "$label"
    fi
}

complete_case_session() {
    local flow_kind="$1"
    local session_id="$2"
    local case_name="$3"
    local params

    if [ "$flow_kind" = "guided" ]; then
        params="$(params_finish_task "$session_id")"
        call_tool "finish_coding_task" "$params"
        assert_success "finish_coding_task succeeds" "$LAST_BODY"
        assert_handoff_available "finish_coding_task includes handoff" "$LAST_BODY" "output.handoff"
        capture_validation_metrics "$LAST_BODY" "output.validation"

        if [ "$case_name" = "small_structured_line_edit" ]; then
            assert_finish_reports_changed_file "$LAST_BODY"
            assert_validation_available \
                "finish_coding_task validation summary is available" \
                "$LAST_BODY" \
                "output.validation"
        elif [ "$case_name" = "failed_call_recovery" ]; then
            assert_validation_unavailable \
                "finish_coding_task validation summary is unavailable without validation events" \
                "$LAST_BODY" \
                "output.validation"
            assert_handoff_failed_tool_metadata \
                "finish_coding_task handoff includes failed tool metadata" \
                "$LAST_BODY" \
                "output.handoff"
        else
            assert_validation_unavailable \
                "finish_coding_task validation summary is unavailable without validation events" \
                "$LAST_BODY" \
                "output.validation"
        fi
    else
        params="$(params_workspace_hygiene "$session_id")"
        call_tool "workspace_hygiene_check" "$params"
        assert_success "workspace_hygiene_check succeeds" "$LAST_BODY"

        params="$(params_session_handoff "$session_id")"
        call_tool "session_handoff_summary" "$params"
        assert_success "session_handoff_summary succeeds" "$LAST_BODY"
        assert_handoff_available "session_handoff_summary returns handoff" "$LAST_BODY" "output"
        capture_validation_metrics "$LAST_BODY" "output.validation"

        if [ "$case_name" = "small_structured_line_edit" ]; then
            assert_validation_available \
                "session_handoff_summary validation summary is available" \
                "$LAST_BODY" \
                "output.validation"
        elif [ "$case_name" = "failed_call_recovery" ]; then
            assert_validation_unavailable \
                "session_handoff_summary validation summary is unavailable without validation events" \
                "$LAST_BODY" \
                "output.validation"
            assert_handoff_failed_tool_metadata \
                "session_handoff_summary includes failed tool metadata" \
                "$LAST_BODY" \
                "output"
        else
            assert_validation_unavailable \
                "session_handoff_summary validation summary is unavailable without validation events" \
                "$LAST_BODY" \
                "output.validation"
        fi
    fi
}

start_eval_services() {
    PORT="${EVAL_PORT:-$(find_free_port)}"
    TMP_ROOT="$(mktemp -d -t webcodex-eval-coding-loop-XXXXXX)"
    DATA_DIR="$TMP_ROOT/data"
    PROJECTS_DIR="$TMP_ROOT/project-registry"
    AGENT_TOML="$TMP_ROOT/runner.toml"
    TEST_REPO="$TMP_ROOT/coding-loop-project"
    SERVER_LOG="$TMP_ROOT/server.log"
    RUNNER_LOG="$TMP_ROOT/agent.log"
    CASE_SUMMARIES_FILE="$TMP_ROOT/case-summaries.jsonl"

    mkdir -p "$DATA_DIR" "$PROJECTS_DIR" "$TEST_REPO/src"
    : >"$CASE_SUMMARIES_FILE"

    log "temp root: $TMP_ROOT"
    log "using port: $PORT"
    log "runtime project id: $RUNTIME_PROJECT_ID"

    (
        cd "$TEST_REPO" || exit 1
        git init -b main >/dev/null 2>&1
        git config user.email "eval@test.local"
        git config user.name "WebCodex Eval"
        cat >Cargo.toml <<'EOF'
[package]
name = "webcodex-eval-temp"
version = "0.1.0"
edition = "2021"

[lib]
path = "src/lib.rs"
EOF
        cat >src/lib.rs <<'EOF'
pub fn greeting() -> &'static str {
    "hello"
}

pub fn topic() -> &'static str {
    "coding-loop eval"
}
EOF
        cat >README.md <<'EOF'
# Coding Loop Eval

This disposable project is used by the WebCodex coding-loop eval harness.
It contains the phrase coding-loop eval so search_project_texts has a stable match.
EOF
        cat >.gitignore <<'EOF'
/target/
EOF
        "$CARGO_BIN" generate-lockfile >/dev/null 2>&1
        git add . >/dev/null 2>&1
        git commit -m "init eval project" >/dev/null 2>&1
    ) || {
        echo "[eval] failed to initialize temporary Rust project" >&2
        exit 2
    }

    cat >"$PROJECTS_DIR/${PROJECT_ID}.toml" <<EOF
id = "${PROJECT_ID}"
path = "${TEST_REPO}"
name = "Coding Loop Eval"
allow_patch = true
kind = "rust"
description = "Disposable project for the minimal coding-loop eval harness"
EOF

    cat >"$AGENT_TOML" <<EOF
server_url = "http://127.0.0.1:${PORT}"
token = "${TOKEN}"
client_id = "${CLIENT_ID}"
display_name = "Coding Loop Eval Agent"
owner = "eval"
project_registry_dir = "${PROJECTS_DIR}"
poll_interval_ms = 500
transport = "${TRANSPORT}"

[policy]
allow_raw_shell = true
allow_cwd_anywhere = true
max_timeout_secs = 60
max_output_bytes = 262144
EOF

    log "starting server"
    if [ -n "$SERVER_BIN" ]; then
        server_command=("$SERVER_BIN")
    else
        server_command=("$CARGO_BIN" run --quiet -p webcodex --bin webcodex-server)
    fi
    WEBCODEX_ADDR="127.0.0.1:${PORT}" \
    WEBCODEX_DATA="$DATA_DIR" \
    WEBCODEX_TOKEN="$TOKEN" \
    RUST_LOG="info" \
    "${server_command[@]}" >"$SERVER_LOG" 2>&1 &
    SERVER_PID=$!

    if ! wait_for_port "$PORT" 90; then
        echo "[eval] server did not start listening on $PORT" >&2
        print_logs_hint
        exit 1
    fi
    log "server listening on $PORT"

    log "starting agent (transport=${TRANSPORT})"
    if [ -n "$RUNNER_BIN" ]; then
        runner_command=("$RUNNER_BIN" --config "$AGENT_TOML")
    else
        runner_command=("$CARGO_BIN" run --quiet -p webcodex-runner --bin webcodex-runner -- --config "$AGENT_TOML")
    fi
    "${runner_command[@]}" >"$RUNNER_LOG" 2>&1 &
    RUNNER_PID=$!

    log "waiting for agent registration"
    local registered=0
    local body
    for _ in $(seq 1 90); do
        check_deadline
        body="$(api_post /api/runtime/status '{}' || true)"
        if [ "$(json_get "$body" output.agents.count)" = "1" ]; then
            registered=1
            break
        fi
        sleep 1
    done

    if [ "$registered" -ne 1 ]; then
        echo "[eval] agent did not register within budget" >&2
        print_logs_hint
        exit 1
    fi
    log "agent registered"
}

run_case_inspect_only() {
    local flow_kind="$1"
    local session_id
    local params

    begin_case "$flow_kind" "inspect_only"

    start_case_session "$flow_kind" "eval inspect-only"
    session_id="$LAST_SESSION_ID"
    if [ -z "$session_id" ]; then
        end_case
        return
    fi

    params="$(python3 - "$RUNTIME_PROJECT_ID" "$session_id" <<'PY'
import json
import sys

print(json.dumps({
    "project": sys.argv[1],
    "session_id": sys.argv[2],
    "queries": [{
        "pattern": "coding-loop eval",
        "path": ".",
        "limit": 10,
    }],
}, separators=(",", ":")))
PY
)"
    call_tool "search_project_texts" "$params"
    assert_success "search_project_texts succeeds" "$LAST_BODY"
    if python3 - "$LAST_BODY" <<'PY'
import json
import sys

data = json.loads(sys.argv[1])
out = data.get("output") or {}
items = out.get("items") or []
item = items[0] if items and isinstance(items[0], dict) else {}
matches = (item.get("output") or {}).get("matches")
ok = (
    data.get("success") is True
    and isinstance(matches, list)
    and any(
        isinstance(match, dict)
        and isinstance(match.get("path"), str)
        and bool(match.get("path"))
        and isinstance(match.get("line"), int)
        and match.get("line") >= 1
        and isinstance(match.get("preview"), str)
        for match in matches
    )
)
sys.exit(0 if ok else 1)
PY
    then
        case_ok "search_project_texts returns structured match records"
    else
        case_fail "search_project_texts structured match records missing"
    fi

    params="$(python3 - "$RUNTIME_PROJECT_ID" "$session_id" <<'PY'
import json
import sys

print(json.dumps({
    "project": sys.argv[1],
    "session_id": sys.argv[2],
    "items": [{
        "path": "README.md",
        "start_line": 1,
        "limit": 6,
    }],
    "with_line_numbers": True,
}, separators=(",", ":")))
PY
)"
    call_tool "read_files" "$params"
    assert_success "read_files succeeds" "$LAST_BODY"

    params="$(python3 - "$RUNTIME_PROJECT_ID" "$session_id" <<'PY'
import json
import sys

print(json.dumps({
    "project": sys.argv[1],
    "session_id": sys.argv[2],
    "include_diff": False,
}, separators=(",", ":")))
PY
)"
    call_tool "show_changes" "$params"
    assert_success "show_changes succeeds" "$LAST_BODY"

    complete_case_session "$flow_kind" "$session_id" "inspect_only"

    if [ "$CASE_RAW_SHELL_CALLS" -eq 0 ]; then
        case_ok "raw shell runtime calls are zero"
    else
        case_fail "raw shell runtime calls should be zero"
    fi

    end_case
}

run_case_small_structured_line_edit() {
    local flow_kind="$1"
    local session_id
    local params

    begin_case "$flow_kind" "small_structured_line_edit"

    start_case_session "$flow_kind" "eval small structured line edit"
    session_id="$LAST_SESSION_ID"
    if [ -z "$session_id" ]; then
        end_case
        return
    fi

    params="$(python3 - "$RUNTIME_PROJECT_ID" "$session_id" <<'PY'
import json
import sys

print(json.dumps({
    "project": sys.argv[1],
    "session_id": sys.argv[2],
    "items": [{
        "path": "src/lib.rs",
        "start_line": 1,
        "limit": 8,
    }],
    "with_line_numbers": True,
}, separators=(",", ":")))
PY
)"
    call_tool "read_files" "$params"
    assert_success "read_files with line numbers succeeds" "$LAST_BODY"
    if python3 - "$LAST_BODY" <<'PY'
import json
import sys

data = json.loads(sys.argv[1])
out = data.get("output") or {}
items = out.get("items") or []
item = items[0] if items and isinstance(items[0], dict) else {}
read_out = item.get("output") or {}
ok = (
    data.get("success") is True
    and read_out.get("format") == "numbered"
    and isinstance(read_out.get("text"), str)
    and "1 | pub fn greeting" in read_out.get("text", "")
)
sys.exit(0 if ok else 1)
PY
    then
        case_ok "read_files returned stable line-number metadata"
    else
        case_fail "read_files line-number metadata missing"
    fi

    params="$(python3 - "$RUNTIME_PROJECT_ID" "$session_id" "$TEST_REPO/src/lib.rs" <<'PY'
import hashlib
import json
import sys

with open(sys.argv[3], "rb") as handle:
    current_sha = hashlib.sha256(handle.read()).hexdigest()

print(json.dumps({
    "project": sys.argv[1],
    "session_id": sys.argv[2],
    "changes": [{
        "kind": "edit",
        "path": "src/lib.rs",
        "expected_sha256": current_sha,
        "edits": [{
            "kind": "replace_exact",
            "old_text": "    \"hello\"",
            "new_text": "    \"hello eval\""
        }]
    }]
}, separators=(",", ":")))
PY
)"
    call_tool "apply_text_edits" "$params"
    assert_success "apply_text_edits structured edit succeeds" "$LAST_BODY"

    params="$(python3 - "$RUNTIME_PROJECT_ID" "$session_id" <<'PY'
import json
import sys

print(json.dumps({
    "project": sys.argv[1],
    "session_id": sys.argv[2],
    "include_diff": True,
    "max_hunks": 5,
    "max_hunk_lines": 40,
}, separators=(",", ":")))
PY
)"
    call_tool "show_changes" "$params"
    assert_success "show_changes after edit succeeds" "$LAST_BODY"
    if python3 - "$LAST_BODY" <<'PY'
import json
import sys

data = json.loads(sys.argv[1])
out = data.get("output") or {}
files = out.get("files") or []
ok = (
    data.get("success") is True
    and out.get("clean") is False
    and any(item.get("path") == "src/lib.rs" for item in files if isinstance(item, dict))
)
sys.exit(0 if ok else 1)
PY
    then
        case_ok "show_changes reports src/lib.rs as changed"
    else
        case_fail "show_changes did not report src/lib.rs as changed"
    fi

    params="$(python3 - "$RUNTIME_PROJECT_ID" "$session_id" <<'PY'
import json
import sys

print(json.dumps({
    "project": sys.argv[1],
    "session_id": sys.argv[2],
    "timeout_secs": 60,
}, separators=(",", ":")))
PY
)"
    call_tool "cargo_check" "$params"
    assert_success "cargo_check validation succeeds" "$LAST_BODY"

    complete_case_session "$flow_kind" "$session_id" "small_structured_line_edit"

    if [ "$CASE_STRUCTURED_EDIT_CALLS" -eq 1 ]; then
        case_ok "exactly one structured edit tool call recorded"
    else
        case_fail "expected exactly one structured edit tool call"
    fi
    if [ "$CASE_RAW_SHELL_CALLS" -eq 0 ]; then
        case_ok "raw shell runtime calls are zero"
    else
        case_fail "raw shell runtime calls should be zero"
    fi

    end_case
}

run_case_failed_call_recovery() {
    local flow_kind="$1"
    local session_id
    local params

    begin_case "$flow_kind" "failed_call_recovery"

    start_case_session "$flow_kind" "eval failed call recovery"
    session_id="$LAST_SESSION_ID"
    if [ -z "$session_id" ]; then
        end_case
        return
    fi

    params="$(python3 - "$RUNTIME_PROJECT_ID" "$session_id" <<'PY'
import json
import sys

print(json.dumps({
    "project": sys.argv[1],
    "session_id": sys.argv[2],
    "changes": [{
        "kind": "edit",
        "path": "src/lib.rs",
        "expected_sha256": "0000000000000000000000000000000000000000000000000000000000000000",
        "edits": [{
            "kind": "replace_exact",
            "old_text": "    \"hello\"",
            "new_text": "    \"should not apply\""
        }]
    }]
}, separators=(",", ":")))
PY
)"
    call_tool "apply_text_edits" "$params"
    assert_failure_error_kind \
        "apply_text_edits wrong sha guard reports sha256_conflict" "$LAST_BODY" "sha256_conflict"

    params="$(python3 - "$RUNTIME_PROJECT_ID" "$session_id" <<'PY'
import json
import sys

print(json.dumps({
    "project": sys.argv[1],
    "session_id": sys.argv[2],
    "items": [{
        "path": "src/lib.rs",
        "start_line": 1,
        "limit": 4,
    }],
    "with_line_numbers": True,
}, separators=(",", ":")))
PY
)"
    call_tool "read_files" "$params"
    assert_success "read_files after failed edit succeeds" "$LAST_BODY"
    if python3 - "$LAST_BODY" <<'PY'
import json
import sys

data = json.loads(sys.argv[1])
items = (data.get("output") or {}).get("items") or []
item = items[0] if items and isinstance(items[0], dict) else {}
text = (item.get("output") or {}).get("text", "")
ok = data.get("success") is True and '"hello"' in text and "should not apply" not in text
sys.exit(0 if ok else 1)
PY
    then
        case_ok "failed edit did not corrupt src/lib.rs"
    else
        case_fail "failed edit changed src/lib.rs unexpectedly"
    fi

    params="$(python3 - "$RUNTIME_PROJECT_ID" "$session_id" "$TEST_REPO/src/lib.rs" <<'PY'
import hashlib
import json
import sys

with open(sys.argv[3], "rb") as handle:
    current_sha = hashlib.sha256(handle.read()).hexdigest()

print(json.dumps({
    "project": sys.argv[1],
    "session_id": sys.argv[2],
    "changes": [{
        "kind": "edit",
        "path": "src/lib.rs",
        "expected_sha256": current_sha,
        "edits": [{
            "kind": "replace_exact",
            "old_text": "    \"hello\"",
            "new_text": "    \"recovered\""
        }]
    }]
}, separators=(",", ":")))
PY
)"
    call_tool "apply_text_edits" "$params"
    assert_success "apply_text_edits recovery edit succeeds" "$LAST_BODY"

    if [ "$CASE_FAILED_TOOL_CALLS" -ge 1 ]; then
        CASE_RECOVERED_FAILED_TOOL_CALLS=1
        case_ok "failed tool call was followed by successful recovery"
    else
        case_fail "expected at least one failed tool call before recovery"
    fi

    params="$(python3 - "$RUNTIME_PROJECT_ID" "$session_id" <<'PY'
import json
import sys

print(json.dumps({
    "project": sys.argv[1],
    "session_id": sys.argv[2],
    "include_diff": True,
    "max_hunks": 5,
    "max_hunk_lines": 40,
}, separators=(",", ":")))
PY
)"
    call_tool "show_changes" "$params"
    assert_success "show_changes after recovery succeeds" "$LAST_BODY"

    complete_case_session "$flow_kind" "$session_id" "failed_call_recovery"

    if [ "$CASE_RAW_SHELL_CALLS" -eq 0 ]; then
        case_ok "raw shell runtime calls are zero"
    else
        case_fail "raw shell runtime calls should be zero"
    fi

    end_case
}

build_final_summary() {
    python3 - "$CASE_SUMMARIES_FILE" "$EVAL_MODE" <<'PY'
import json
import sys

cases = []
with open(sys.argv[1], "r", encoding="utf-8") as handle:
    for line in handle:
        line = line.strip()
        if line:
            cases.append(json.loads(line))

mode = sys.argv[2]

def summarize(flow_mode):
    flow_cases = [case for case in cases if case.get("mode") == flow_mode]
    tool_calls_total = sum(c["tool_calls"] for c in flow_cases)
    raw_shell_calls = sum(c["raw_shell_calls"] for c in flow_cases)
    finish_calls = sum(c["finish_coding_task_calls"] for c in flow_cases)
    finish_successes = sum(c["finish_coding_task_successes"] for c in flow_cases)
    validation_available = sum(1 for c in flow_cases if c["validation_available"])
    validation_parser_available = sum(1 for c in flow_cases if c["validation_parser_available"])
    validation_diagnostics_available = sum(
        1 for c in flow_cases if c["validation_diagnostics_available"]
    )
    validation_events_total = sum(c["validation_events_total"] for c in flow_cases)
    validation_diagnostics_total = sum(c["validation_diagnostics_total"] for c in flow_cases)
    finish_rate = None
    if flow_mode == "guided":
        finish_rate = (finish_successes / finish_calls) if finish_calls else 0.0

    return {
        "mode": flow_mode,
        "skipped": False,
        "cases_total": len(flow_cases),
        "cases_passed": sum(1 for c in flow_cases if c["passed"]),
        "cases_failed": sum(1 for c in flow_cases if not c["passed"]),
        "tool_calls_total": tool_calls_total,
        "raw_shell_calls": raw_shell_calls,
        "raw_shell_ratio": (raw_shell_calls / tool_calls_total) if tool_calls_total else 0.0,
        "structured_edit_calls": sum(c["structured_edit_calls"] for c in flow_cases),
        "failed_tool_calls": sum(c["failed_tool_calls"] for c in flow_cases),
        "recovered_failed_tool_calls": sum(c["recovered_failed_tool_calls"] for c in flow_cases),
        "workspace_clean_after_each_case": all(c["workspace_clean"] for c in flow_cases) if flow_cases else False,
        "handoff_available_rate": (
            sum(1 for c in flow_cases if c["handoff_available"]) / len(flow_cases)
        ) if flow_cases else 0.0,
        "validation_available_rate": (
            validation_available / len(flow_cases)
        ) if flow_cases else 0.0,
        "validation_parser_available_rate": (
            validation_parser_available / len(flow_cases)
        ) if flow_cases else 0.0,
        "validation_diagnostics_available_rate": (
            validation_diagnostics_available / len(flow_cases)
        ) if flow_cases else 0.0,
        "validation_events_total": validation_events_total,
        "validation_successes": sum(c["validation_successes"] for c in flow_cases),
        "validation_failures": sum(c["validation_failures"] for c in flow_cases),
        "validation_diagnostics_total": validation_diagnostics_total,
        "finish_coding_task_success_rate": finish_rate,
        "cases": flow_cases,
    }

def bool_score(value):
    return 1.0 if value is True else 0.0

baseline = summarize("baseline") if mode in ("baseline", "compare") else None
guided = summarize("guided") if mode in ("guided", "compare") else None

comparison = None
if mode == "compare":
    comparison = {
        "guided_minus_baseline_tool_calls": guided["tool_calls_total"] - baseline["tool_calls_total"],
        "guided_minus_baseline_raw_shell_ratio": guided["raw_shell_ratio"] - baseline["raw_shell_ratio"],
        "guided_minus_baseline_structured_edit_calls": guided["structured_edit_calls"] - baseline["structured_edit_calls"],
        "guided_handoff_available_delta": guided["handoff_available_rate"] - baseline["handoff_available_rate"],
        "guided_minus_baseline_validation_available_rate": guided["validation_available_rate"] - baseline["validation_available_rate"],
        "guided_minus_baseline_validation_parser_available_rate": guided["validation_parser_available_rate"] - baseline["validation_parser_available_rate"],
        "guided_minus_baseline_validation_diagnostics_available_rate": guided["validation_diagnostics_available_rate"] - baseline["validation_diagnostics_available_rate"],
        "guided_minus_baseline_validation_events_total": guided["validation_events_total"] - baseline["validation_events_total"],
        "guided_cleanup_delta": (
            bool_score(guided["workspace_clean_after_each_case"])
            - bool_score(baseline["workspace_clean_after_each_case"])
        ),
    }

selected = [summary for summary in (baseline, guided) if summary is not None]
summary = {
    "mode": mode,
    "skipped": False,
    "cases_total": sum(item["cases_total"] for item in selected),
    "cases_passed": sum(item["cases_passed"] for item in selected),
    "cases_failed": sum(item["cases_failed"] for item in selected),
    "baseline": baseline,
    "guided": guided,
    "comparison": comparison,
}
print(json.dumps(summary, separators=(",", ":"), sort_keys=True))
PY
}

start_eval_services

run_flow_cases() {
    local flow_kind="$1"
    run_case_inspect_only "$flow_kind"
    run_case_small_structured_line_edit "$flow_kind"
    run_case_failed_call_recovery "$flow_kind"
}

run_compare_cases() {
    run_case_inspect_only "baseline"
    run_case_inspect_only "guided"
    run_case_small_structured_line_edit "baseline"
    run_case_small_structured_line_edit "guided"
    run_case_failed_call_recovery "baseline"
    run_case_failed_call_recovery "guided"
}

case "$EVAL_MODE" in
    baseline)
        run_flow_cases "baseline"
        ;;
    guided)
        run_flow_cases "guided"
        ;;
    compare)
        run_compare_cases
        ;;
esac

FINAL_SUMMARY="$(build_final_summary)"
FINAL_FAILED="$(json_get "$FINAL_SUMMARY" cases_failed)"

cleanup_background_processes
trap - INT TERM EXIT

if [ "${FINAL_FAILED:-0}" = "0" ]; then
    if [ "${EVAL_KEEP_TMP:-0}" = "1" ]; then
        log "EVAL_KEEP_TMP=1: keeping temp root $TMP_ROOT"
    else
        rm -rf "$TMP_ROOT"
    fi
else
    print_logs_hint
fi

log "final JSON summary follows"
printf '%s\n' "$FINAL_SUMMARY"

if [ "${FINAL_FAILED:-0}" = "0" ]; then
    exit 0
fi
exit 1
