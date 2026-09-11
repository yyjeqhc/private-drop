#!/usr/bin/env bash
set -euo pipefail

# ============================================================================
# WebCodex — Zero-Config Agent Transport E2E Smoke
#
# Starts a real `webcodex` server and a `webcodex-runner` connected over
# the selected agent transport, defaulting to WebSocket, then exercises the
# full GPT Actions + MCP surface via curl to prove the runtime is wired
# end-to-end on a single host.
#
# What this proves:
#   - Server boots with WEBCODEX_TOKEN auth and no server-side projects.toml.
#   - Agent registers over the selected transport and announces a project.
#   - listProjects / getRuntimeStatus see the agent-registered project.
#   - read_files / getProjectGitStatus route to the agent.
#   - startProjectShellJob starts an async job on the agent and job status/log
#     round-trip.
#   - MCP initialize / tools/list / tools/call(list_projects) work.
#   - /openapi.json still exposes the expected GPT Actions operation set and
#     omits legacy/admin paths.
#
# What this does NOT do:
#   - It does not touch the real ChatGPT web UI.
#   - It does not invoke removed run_codex paths or the real Codex CLI.
#   - It does not implement QUIC.
#
# Environment overrides:
#   E2E_PORT            bind port (default: auto-pick a free port)
#   E2E_TOKEN           Bearer token (default: e2e-smoke-token)
#   E2E_CLIENT_ID       agent client_id (default: e2e-agent)
#   E2E_PROJECT_ID      agent project id (default: smoke-proj)
#   E2E_TRANSPORT       agent transport (default: websocket; polling fallback)
#   E2E_TIMEOUT_SECS    overall wall-clock cap (default: 180)
#   E2E_KEEPALIVE_WAIT_SECS
#                       seconds to idle before the keepalive-online recheck
#                       (default: 2; raise to ~35 to span a real ping/pong)
#   E2E_SKIP_RUN        if set to "1", skip execution and only syntax-check
#   E2E_SERVER_BIN      existing webcodex-server executable; skips server `cargo run`
#   E2E_RUNNER_BIN      existing webcodex-runner executable; skips runner `cargo run`
#   CARGO_BIN           cargo binary (default: cargo; used when an override is absent)
#
# Exit codes:
#   0  all smoke checks passed
#   1  one or more checks failed
#   2  environment/dependency error
# ============================================================================

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$PROJECT_DIR"

CARGO_BIN="${CARGO_BIN:-cargo}"
SERVER_BIN="${E2E_SERVER_BIN:-}"
RUNNER_BIN="${E2E_RUNNER_BIN:-}"
TOKEN="${E2E_TOKEN:-e2e-smoke-token}"
CLIENT_ID="${E2E_CLIENT_ID:-e2e-agent}"
PROJECT_ID="${E2E_PROJECT_ID:-smoke-proj}"
TRANSPORT="${E2E_TRANSPORT:-websocket}"
TIMEOUT_SECS="${E2E_TIMEOUT_SECS:-180}"
# Runtime project id exposed by the agent: agent:<client_id>:<project_id>
RUNTIME_PROJECT_ID="agent:${CLIENT_ID}:${PROJECT_ID}"

PASS=0
FAIL=0
SERVER_PID=""
RUNNER_PID=""
TMP_ROOT=""
SERVER_LOG=""
RUNNER_LOG=""
START_EPOCH=$(date +%s)

# ----------------------------------------------------------------------------
# Helpers
# ----------------------------------------------------------------------------

log() { printf '[e2e] %s\n' "$*"; }

fail() {
    FAIL=$((FAIL + 1))
    printf '[e2e][FAIL] %s\n' "$*" >&2
}

pass() {
    PASS=$((PASS + 1))
    printf '[e2e][ok]   %s\n' "$*"
}

elapsed() {
    echo $(( $(date +%s) - START_EPOCH ))
}

remaining_time() {
    local used; used=$(elapsed)
    echo $(( TIMEOUT_SECS - used ))
}

# Hard overall deadline: bail out if exceeded.
check_deadline() {
    if [ "$(remaining_time)" -le 0 ]; then
        fail "overall timeout (${TIMEOUT_SECS}s) exceeded"
        cleanup
        print_logs_hint
        exit 1
    fi
}

# Find a free TCP port on 127.0.0.1.
find_free_port() {
    python3 -c "
import socket
s = socket.socket()
s.bind(('127.0.0.1', 0))
print(s.getsockname()[1])
s.close()
" 2>/dev/null || {
        # Fallback when python3 is unavailable.
        local p
        for p in 18080 18081 18082 18083 18084; do
            if ! (echo >/dev/tcp/127.0.0.1/$p) 2>/dev/null; then
                echo "$p"
                return
            fi
        done
        echo 18080
    }
}

# Wait until a TCP port accepts connections, with a per-call budget.
wait_for_port() {
    local port="$1"
    local budget="${2:-30}"
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

# curl wrapper with auth + timeout. Prints body to stdout.
api_post() {
    local path="$1"
    local body="${2:-}"
    # Avoid `${2:-{}}` here: bash parses the `}` ambiguously and appends a
    # stray `}` to non-empty bodies, which breaks strict JSON parsing on the
    # server. Default explicitly instead.
    if [ -z "$body" ]; then
        body="{}"
    fi
    curl -sS --max-time 10 \
        -H "Authorization: Bearer ${TOKEN}" \
        -H "Content-Type: application/json" \
        -X POST "http://127.0.0.1:${PORT}${path}" \
        -d "$body" 2>/dev/null
}

runtime_tool_call() {
    local tool="$1"
    local params="$2"
    api_post /api/tools/call "{\"tool\":\"${tool}\",\"params\":${params}}"
}

show_changes_call() {
    runtime_tool_call "show_changes" "{\"project\":\"$RUNTIME_PROJECT_ID\"}"
}

observe_one_job_call() {
    local job_id="$1"
    local tail_lines="${2:-40}"
    runtime_tool_call "observe_jobs" "{\"items\":[{\"job_id\":\"${job_id}\"}],\"tail_lines\":${tail_lines}}"
}

api_get() {
    local path="$1"
    curl -sS --max-time 10 \
        -H "Authorization: Bearer ${TOKEN}" \
        "http://127.0.0.1:${PORT}${path}" 2>/dev/null
}

# Python here is used only by dev/e2e scripts for JSON parsing/test
# orchestration; runtime production paths do not depend on Python helpers.
# Extract a JSON field with python3 (no jq dependency required).
json_get() {
    # json_get '<json>' '<dot.path>'
    local json="$1"
    local path="$2"
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
else:
    print(cur if cur is not None else "")
PY
}

# Assert a JSON response has success == true.
assert_success() {
    local label="$1"
    local body="$2"
    local ok
    ok="$(json_get "$body" success)"
    if [ "$ok" = "True" ]; then
        pass "$label"
        return 0
    else
        fail "$label (success != true; body: ${body:0:300})"
        return 1
    fi
}

print_logs_hint() {
    cat >&2 <<EOF

[e2e] ---- log locations ----
[e2e] server log: ${SERVER_LOG:-<none>}
[e2e] runner log:  ${RUNNER_LOG:-<none>}
[e2e] temp root:  ${TMP_ROOT:-<none>}
EOF
}

# ----------------------------------------------------------------------------
# Cleanup
# ----------------------------------------------------------------------------

cleanup() {
    trap - INT TERM EXIT
    log "cleaning up background processes"
    if [ -n "${RUNNER_PID:-}" ] && kill -0 "$RUNNER_PID" 2>/dev/null; then
        kill "$RUNNER_PID" 2>/dev/null || true
        sleep 1
        kill -9 "$RUNNER_PID" 2>/dev/null || true
    fi
    if [ -n "${SERVER_PID:-}" ] && kill -0 "$SERVER_PID" 2>/dev/null; then
        kill "$SERVER_PID" 2>/dev/null || true
        sleep 1
        kill -9 "$SERVER_PID" 2>/dev/null || true
    fi
    # Wait briefly for the launched runtime processes to tear down children.
    sleep 1
}

trap cleanup INT TERM EXIT

# ----------------------------------------------------------------------------
# Dependency checks
# ----------------------------------------------------------------------------

if ! command -v curl >/dev/null 2>&1; then
    echo "[e2e] curl is required" >&2
    exit 2
fi
if ! command -v python3 >/dev/null 2>&1; then
    echo "[e2e] python3 is required (for JSON checks and port discovery)" >&2
    exit 2
fi
if ! command -v git >/dev/null 2>&1; then
    echo "[e2e] git is required" >&2
    exit 2
fi

if [ "${E2E_SKIP_RUN:-0}" = "1" ]; then
    log "E2E_SKIP_RUN=1: skipping execution (syntax-only)"
    exit 0
fi

validate_binary_override() {
    local label="$1"
    local value="$2"
    if [ -z "$value" ]; then
        return 0
    fi
    case "$value" in
        *$'\n'*|*$'\r'*)
            echo "[e2e] $label contains a newline" >&2
            exit 2
            ;;
    esac
    if [ ! -f "$value" ] || [ ! -x "$value" ]; then
        echo "[e2e] $label is not an executable regular file: $value" >&2
        exit 2
    fi
}

validate_binary_override "E2E_SERVER_BIN" "$SERVER_BIN"
validate_binary_override "E2E_RUNNER_BIN" "$RUNNER_BIN"
if { [ -z "$SERVER_BIN" ] || [ -z "$RUNNER_BIN" ]; } && ! command -v "$CARGO_BIN" >/dev/null 2>&1; then
    echo "[e2e] cargo is required when an existing runtime binary override is absent" >&2
    exit 2
fi

# ----------------------------------------------------------------------------
# 1. Pick a port, then build the temporary runtime layout
# ----------------------------------------------------------------------------

PORT="${E2E_PORT:-$(find_free_port)}"
BASE="http://127.0.0.1:${PORT}"

TMP_ROOT="$(mktemp -d -t webcodex-e2e-XXXXXX)"
DATA_DIR="$TMP_ROOT/data"
PROJECTS_DIR="$TMP_ROOT/project-registry"
AGENT_TOML="$TMP_ROOT/runner.toml"
TEST_REPO="$TMP_ROOT/smoke-repo"
SERVER_LOG="$TMP_ROOT/server.log"
RUNNER_LOG="$TMP_ROOT/agent.log"

mkdir -p "$DATA_DIR" "$PROJECTS_DIR" "$TEST_REPO"
log "temp root: $TMP_ROOT"

# Initialize a tiny git repo as the agent project so git_status works.
(
    cd "$TEST_REPO"
    git init -b main >/dev/null 2>&1
    git config user.email "e2e@test.local"
    git config user.name "E2E Smoke"
    printf '# Smoke Project\n\nUsed by the webcodex E2E harness.\n' > README.md
    printf 'fn main() { println!("smoke"); }\n' > src.rs 2>/dev/null || {
        mkdir -p src
        printf 'fn main() { println!("smoke"); }\n' > src/main.rs
    }
    git add . >/dev/null 2>&1
    git commit -m "smoke init" >/dev/null 2>&1 || true
)

# Agent-side project file describing the local repo.
cat > "$PROJECTS_DIR/${PROJECT_ID}.toml" <<EOF
id = "${PROJECT_ID}"
path = "${TEST_REPO}"
name = "Smoke Project"
allow_patch = true
kind = "rust"
description = "E2E smoke project"
EOF

# Agent config: WebSocket preferred transport. owner is arbitrary because
# WEBCODEX_TOKEN auth marks the principal as bootstrap (any owner allowed).
cat > "$AGENT_TOML" <<EOF
server_url = "http://127.0.0.1:${PORT}"
token = "${TOKEN}"
client_id = "${CLIENT_ID}"
display_name = "E2E Agent"
owner = "e2e"
project_registry_dir = "${PROJECTS_DIR}"
poll_interval_ms = 500
transport = "${TRANSPORT}"

[policy]
allow_raw_shell = true
allow_cwd_anywhere = true
max_timeout_secs = 60
max_output_bytes = 262144
EOF

log "using port: $PORT"
log "transport: $TRANSPORT"
log "runtime project id: $RUNTIME_PROJECT_ID"

# ----------------------------------------------------------------------------
# 3. Start the server
# ----------------------------------------------------------------------------

if [ -n "$SERVER_BIN" ]; then
    log "starting server (existing binary: $SERVER_BIN)"
    WEBCODEX_ADDR="127.0.0.1:${PORT}" \
    WEBCODEX_DATA="$DATA_DIR" \
    WEBCODEX_TOKEN="$TOKEN" \
    RUST_LOG="info" \
    "$SERVER_BIN" >"$SERVER_LOG" 2>&1 &
else
    log "starting server (cargo run -p webcodex --bin webcodex-server)"
    WEBCODEX_ADDR="127.0.0.1:${PORT}" \
    WEBCODEX_DATA="$DATA_DIR" \
    WEBCODEX_TOKEN="$TOKEN" \
    RUST_LOG="info" \
    "$CARGO_BIN" run --quiet -p webcodex --bin webcodex-server >"$SERVER_LOG" 2>&1 &
fi
SERVER_PID=$!

if ! wait_for_port "$PORT" 40; then
    fail "server did not start listening on $PORT within budget"
    print_logs_hint
    exit 1
fi
pass "server listening on $PORT"

# ----------------------------------------------------------------------------
# 4. Start the agent
# ----------------------------------------------------------------------------

if [ -n "$RUNNER_BIN" ]; then
    log "starting agent (existing binary: $RUNNER_BIN, transport=$TRANSPORT)"
    "$RUNNER_BIN" --config "$AGENT_TOML" >"$RUNNER_LOG" 2>&1 &
else
    log "starting agent (cargo run -p webcodex-runner --bin webcodex-runner, transport=$TRANSPORT)"
    "$CARGO_BIN" run --quiet -p webcodex-runner --bin webcodex-runner -- --config "$AGENT_TOML" >"$RUNNER_LOG" 2>&1 &
fi
RUNNER_PID=$!

# Wait for the agent to register by polling runtime_status for the client.
log "waiting for agent registration..."
REGISTERED=0
for _ in $(seq 1 60); do
    check_deadline
    body="$(api_post /api/runtime/status '{}' || true)"
    agent_count="$(json_get "$body" output.agents.count)"
    if [ "$agent_count" = "1" ]; then
        REGISTERED=1
        break
    fi
    sleep 1
done

if [ "$REGISTERED" -ne 1 ]; then
    fail "agent did not register within budget"
    print_logs_hint
    exit 1
fi
pass "agent registered (transport=$TRANSPORT)"

# ----------------------------------------------------------------------------
# 4b. Keepalive liveness smoke
# ----------------------------------------------------------------------------
# After a brief idle period the agent must still report online. This is a
# light regression guard for the WebSocket ping/pong liveness fix: a
# connected-but-idle agent must not decay to stale merely because no job
# requests are flowing. (The full 60s online window is exercised by unit
# tests via last_seen injection; here we only confirm no immediate drop so
# the default e2e stays fast. Override the wait with
# E2E_KEEPALIVE_WAIT_SECS, e.g. 35 to span one real ping/pong cycle.)
KEEPALIVE_WAIT="${E2E_KEEPALIVE_WAIT_SECS:-2}"
log "keepalive liveness check (idle ${KEEPALIVE_WAIT}s)"
sleep "$KEEPALIVE_WAIT"
check_deadline
body="$(api_post /api/runtime/status '{}' || true)"
agent_connected="$(json_get "$body" output.agents.clients.0.connected)"
agent_status="$(json_get "$body" output.agents.clients.0.status)"
agent_transport="$(json_get "$body" output.agents.clients.0.transport)"
if [ "$agent_connected" = "True" ] && [ "$agent_status" = "online" ]; then
    pass "agent still online after idle wait (transport=$agent_transport)"
else
    fail "agent went stale after idle wait (connected=$agent_connected status=$agent_status transport=$agent_transport)"
fi

# ----------------------------------------------------------------------------
# 5. GPT Actions surface smoke
# ----------------------------------------------------------------------------

log "---- GPT Actions surface ----"

# getRuntimeStatus
body="$(api_post /api/runtime/status '{}')"
assert_success "getRuntimeStatus" "$body" || true

# listProjects — must include the agent-registered project id.
body="$(api_post /api/projects/list '{}')"
assert_success "listProjects" "$body" || true
# Verify the runtime project id appears in the list.
list_json="$(json_get "$body" output)"
if echo "$list_json" | grep -q "\"$RUNTIME_PROJECT_ID\""; then
    pass "listProjects contains $RUNTIME_PROJECT_ID"
else
    fail "listProjects did not contain $RUNTIME_PROJECT_ID (got: ${list_json:0:200})"
fi

# getProjectGitStatus — routes to the agent, runs `git status --porcelain`.
body="$(api_post /api/projects/git_status "{\"project\":\"$RUNTIME_PROJECT_ID\"}")"
assert_success "getProjectGitStatus" "$body" || true

# read_files — reads README.md through the canonical runtime tool.
body="$(api_post /api/tools/call "{\"tool\":\"read_files\",\"params\":{\"project\":\"$RUNTIME_PROJECT_ID\",\"items\":[{\"path\":\"README.md\"}]}}")"
assert_success "read_files(README.md)" "$body" || true
readme_content="$(json_get "$body" output.items.0.output.text)"
if echo "$readme_content" | grep -q "Smoke Project"; then
    pass "read_files returns README content"
else
    fail "read_files content mismatch (got: ${readme_content:0:120})"
fi

# git_diff_hunks — canonical bounded diff inspection through the generic runtime route.
body="$(runtime_tool_call "git_diff_hunks" "{\"project\":\"$RUNTIME_PROJECT_ID\",\"max_hunks\":5}")"
assert_success "git_diff_hunks" "$body" || true

# runProjectShellCommand — runs `echo hi` through the agent.
body="$(api_post /api/projects/run_shell "{\"project\":\"$RUNTIME_PROJECT_ID\",\"command\":\"echo hi\"}")"
assert_success "runProjectShellCommand" "$body" || true
shell_stdout="$(json_get "$body" output.stdout_tail)"
if echo "$shell_stdout" | grep -q "hi"; then
    pass "runProjectShellCommand returns echo output"
else
    fail "runProjectShellCommand output mismatch (got: ${shell_stdout:0:120})"
fi

# startProjectShellJob — starts an async job on the agent.
async_job_body="$(python3 -c '
import json, sys
print(json.dumps({
    "project": sys.argv[1],
    "command": "printf job-log-ok",
    "timeout_secs": 20
}))
' "$RUNTIME_PROJECT_ID")"
body="$(api_post /api/projects/run_job "$async_job_body")"
assert_success "startProjectShellJob" "$body" || true
JOB_ID="$(json_get "$body" output.job_id)"
if [ -z "$JOB_ID" ] || [ "$JOB_ID" = "" ] || [ "$JOB_ID" = "None" ]; then
    fail "startProjectShellJob did not return a job_id (body: ${body:0:300})"
else
    pass "startProjectShellJob returned job_id=$JOB_ID"

    # Poll job status until terminal.
    JOB_TERMINAL=0
    for _ in $(seq 1 40); do
        check_deadline
        body="$(observe_one_job_call "$JOB_ID" 1)"
        status="$(json_get "$body" output.items.0.output.status)"
        case "$status" in
            completed|failed|stopped|lost)
                JOB_TERMINAL=1
                break
                ;;
            *)
                sleep 1
                ;;
        esac
    done

    if [ "$JOB_TERMINAL" -ne 1 ]; then
        fail "job $JOB_ID did not reach a terminal status in time"
    else
        if [ "$status" = "completed" ]; then
            pass "job $JOB_ID reached terminal status: $status"
        else
            fail "job $JOB_ID reached terminal status: $status (expected completed)"
        fi
    fi

    # observe_jobs — read bounded stdout for the job through the canonical observer.
    body="$(observe_one_job_call "$JOB_ID" 50)"
    assert_success "observe_jobs" "$body" || true
    log_stdout="$(json_get "$body" output.items.0.output.stdout_tail)"
    if echo "$log_stdout" | grep -q "job-log-ok"; then
        pass "observe_jobs contains async job output"
    else
        fail "observe_jobs did not contain async job output (got: ${log_stdout:0:160})"
    fi
fi

# ----------------------------------------------------------------------------
# 6. MCP surface smoke
# ----------------------------------------------------------------------------

# The runtime exposure is startup-selected and immutable. Without Connector
# configuration, Runtime(ModelSurface) defaults to adaptive_runtime: a smaller
# typed core plus one long-tail runtime gateway. local-coding-v1 remains the
# explicit fixed compatibility surface, while full-operator-v1 exposes the complete operator tool set.
# No runtime surface re-exposes removed legacy edit tools or ModelHidden tools
# (job_tail) via MCP tools/list.
MODEL_SURFACE_ENV="${WEBCODEX_MCP_MODEL_SURFACE:-}"
case "$MODEL_SURFACE_ENV" in
    "" | "adaptive-runtime-v1")
        EXPECTED_SURFACE="adaptive_runtime"
        ;;
    "local-coding-v1")
        EXPECTED_SURFACE="local_coding"
        ;;
    "full-operator-v1")
        EXPECTED_SURFACE="full_operator_runtime"
        ;;
    *)
        fail "unsupported WEBCODEX_MCP_MODEL_SURFACE=$MODEL_SURFACE_ENV"
        EXPECTED_SURFACE="adaptive_runtime"
        ;;
esac
log "expected runtime exposure: $EXPECTED_SURFACE"

log "---- MCP surface (/mcp) ----"

# initialize
body="$(api_post /mcp '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}')"
proto="$(json_get "$body" result.protocolVersion)"
if [ -n "$proto" ] && [ "$proto" != "" ]; then
    pass "MCP initialize returns protocolVersion=$proto"
else
    fail "MCP initialize did not return a protocolVersion (body: ${body:0:300})"
fi
runtime_exposure="$(json_get "$body" result.serverInfo.runtimeExposure)"
if [ "$runtime_exposure" = "$EXPECTED_SURFACE" ]; then
    pass "MCP initialize runtimeExposure=$runtime_exposure"
else
    fail "MCP initialize runtimeExposure mismatch (expected $EXPECTED_SURFACE got '$runtime_exposure' body: ${body:0:300})"
fi

# tools/list
body="$(api_post /mcp '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}')"
TOOLS_LIST_BODY="$body"
tools_count="$(echo "$body" | python3 -c 'import json,sys; d=json.load(sys.stdin); print(len(d.get("result",{}).get("tools",[])))' 2>/dev/null || echo 0)"
if [ "$EXPECTED_SURFACE" = "adaptive_runtime" ]; then
    min_tools_count=21
else
    min_tools_count=30
fi
if [ "${tools_count:-0}" -ge "$min_tools_count" ]; then
    pass "MCP tools/list returned $tools_count tools"
else
    fail "MCP tools/list returned too few tools (got $tools_count expected >=$min_tools_count; body: ${body:0:300})"
fi
# Extract the exact tool names from MCP tools/list (never grep the raw body:
# descriptions and schemas may legitimately mention other tool names).
mcp_tool_names() {
    echo "$TOOLS_LIST_BODY" | python3 -c '
import json, sys
try:
    data = json.load(sys.stdin)
except Exception:
    sys.exit(0)
for tool in data.get("result", {}).get("tools", []):
    print(tool.get("name", ""))
'
}
mcp_tool_present() {
    mcp_tool_names | grep -qx "$1"
}

if [ "$EXPECTED_SURFACE" = "local_coding" ]; then
    # The local_coding canonical coding loop must expose its key tools.
    mcp_canonical_present=1
    for tname in work_on_project list_projects project_overview read_files \
        search_project_texts apply_text_edits apply_unified_diff run_shell \
        run_job job_status job_log list_jobs stop_job cargo_fmt cargo_check \
        cargo_test validation_summary git_status git_diff show_changes \
        finish_coding_task; do
        if mcp_tool_present "$tname"; then
            :
        else
            mcp_canonical_present=0
            fail "MCP tools/list missing local_coding tool $tname"
        fi
    done
    if [ "$mcp_canonical_present" = "1" ]; then
        pass "MCP tools/list exposes the local_coding canonical coding loop"
    fi
    # Non-local_coding / old-granularity tools must NOT re-enter the model
    # surface. replace_in_file was removed entirely; write_project_file is a
    # retained whole-file write tool that stays ModelVisible only on the
    # full-operator surface, never on local_coding.
    mcp_compat_absent=1
    for tname in write_project_file job_tail list_tools \
        git_diff git_diff_summary job_status job_log start_coding_task; do
        if mcp_tool_present "$tname"; then
            mcp_compat_absent=0
            fail "MCP tools/list must not expose $tname on local_coding"
        fi
    done
    if [ "$mcp_compat_absent" = "1" ]; then
        pass "MCP tools/list excludes non-local_coding tools on local_coding"
    fi
elif [ "$EXPECTED_SURFACE" = "adaptive_runtime" ]; then
    adaptive_present=1
    for tname in work_on_project runtime_status tool_manifest \
        search_project_texts read_files apply_text_edits apply_patch run_process run_shell observe_jobs list_jobs \
        cargo_check cargo_test git_review_summary git_diff_hunks \
        show_changes workspace_hygiene_check finish_coding_task call_runtime_tool; do
        if mcp_tool_present "$tname"; then
            :
        else
            adaptive_present=0
            fail "MCP tools/list missing adaptive_runtime tool $tname"
        fi
    done
    for tname in list_tools list_projects project_overview read_file search_project_text run_script apply_unified_diff \
        go_test validation_summary git_status goto_definition computer_list_windows \
        post_session_message coding_agent_start artifact_upload_begin; do
        if mcp_tool_present "$tname"; then
            adaptive_present=0
            fail "MCP tools/list must keep long-tail tool $tname behind call_runtime_tool"
        fi
    done
    if [ "$adaptive_present" = "1" ]; then
        pass "MCP tools/list exposes only the adaptive typed core plus gateway"
    fi
else
    # full_operator_runtime: the complete operator tool surface.
    mcp_operator_present=1
    for tname in list_tools work_on_project finish_coding_task \
        git_review_summary git_diff_hunks apply_unified_diff read_files \
        search_project_texts run_shell run_job observe_jobs list_jobs show_changes; do
        if mcp_tool_present "$tname"; then
            :
        else
            mcp_operator_present=0
            fail "MCP tools/list missing full-operator tool $tname"
        fi
    done
    if [ "$mcp_operator_present" = "1" ]; then
        pass "MCP tools/list exposes the full-operator tool surface"
    fi
    # ModelHidden and retired tools must never appear in MCP tools/list.
    # write_project_file is ModelVisible and is part of the full-operator
    # surface, so it is not asserted absent here. replace_in_file and the
    # external start_coding_task compatibility entry were retired entirely.
    mcp_hidden_absent=1
    for tname in job_tail start_coding_task; do
        if mcp_tool_present "$tname"; then
            mcp_hidden_absent=0
            fail "MCP tools/list must not expose ModelHidden tool $tname"
        fi
    done
    if [ "$mcp_hidden_absent" = "1" ]; then
        pass "MCP tools/list excludes ModelHidden tools"
    fi
fi

# tools/call list_projects — must return structuredContent with the agent project.
body="$(api_post /mcp '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"list_projects","arguments":{}}}')"
sc="$(json_get "$body" result.structuredContent)"
sc_success="$(json_get "$sc" success)"
if [ "$sc_success" = "True" ]; then
    pass "MCP tools/call(list_projects) returns structuredContent.success=true"
else
    fail "MCP tools/call(list_projects) structuredContent not success (body: ${body:0:300})"
fi
sc_output="$(json_get "$sc" output)"
if echo "$sc_output" | grep -q "$RUNTIME_PROJECT_ID"; then
    pass "MCP list_projects sees agent project $RUNTIME_PROJECT_ID"
else
    fail "MCP list_projects did not see $RUNTIME_PROJECT_ID (got: ${sc_output:0:200})"
fi

# tools/call read_files — exercise the real bounded batch path through MCP and
# verify order, per-item success, and project-relative output only.
body="$(api_post /mcp "{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"tools/call\",\"params\":{\"name\":\"read_files\",\"arguments\":{\"project\":\"$RUNTIME_PROJECT_ID\",\"items\":[{\"path\":\"README.md\",\"limit\":20},{\"path\":\"src.rs\",\"limit\":20}]}}}")"
sc="$(json_get "$body" result.structuredContent)"
if [ "$(json_get "$sc" success)" = "True" ] \
    && [ "$(json_get "$sc" output.requested_count)" = "2" ] \
    && [ "$(json_get "$sc" output.returned_count)" = "2" ] \
    && [ "$(json_get "$sc" output.items.0.path)" = "README.md" ] \
    && [ "$(json_get "$sc" output.items.0.success)" = "True" ] \
    && [ "$(json_get "$sc" output.items.1.path)" = "src.rs" ] \
    && [ "$(json_get "$sc" output.items.1.success)" = "True" ]; then
    pass "MCP tools/call(read_files) returns two ordered successful items"
else
    fail "MCP tools/call(read_files) contract mismatch (body: ${body:0:400})"
fi
if echo "$sc" | grep -Fq "$TEST_REPO"; then
    fail "MCP tools/call(read_files) leaked an absolute project path"
else
    pass "MCP tools/call(read_files) keeps paths project-relative"
fi

# tools/call search_project_texts — exercise two independent result modes
# through the real MCP/Runner path and verify ordered project-relative output.
body="$(api_post /mcp "{\"jsonrpc\":\"2.0\",\"id\":5,\"method\":\"tools/call\",\"params\":{\"name\":\"search_project_texts\",\"arguments\":{\"project\":\"$RUNTIME_PROJECT_ID\",\"queries\":[{\"pattern\":\"Smoke Project\",\"path\":\"README.md\",\"result_mode\":\"matches\",\"limit\":10},{\"pattern\":\"println!\",\"path\":\"src.rs\",\"result_mode\":\"files_with_matches\",\"limit\":10}]}}}")"
sc="$(json_get "$body" result.structuredContent)"
if [ "$(json_get "$sc" success)" = "True" ] \
    && [ "$(json_get "$sc" output.items.0.index)" = "0" ] \
    && [ "$(json_get "$sc" output.items.0.success)" = "True" ] \
    && [ "$(json_get "$sc" output.items.0.output.matches.0.path)" = "README.md" ] \
    && [ "$(json_get "$sc" output.items.1.index)" = "1" ] \
    && [ "$(json_get "$sc" output.items.1.success)" = "True" ] \
    && [ "$(json_get "$sc" output.items.1.output.result_mode)" = "files_with_matches" ] \
    && [ "$(json_get "$sc" output.items.1.output.files.0.path)" = "src.rs" ]; then
    pass "MCP tools/call(search_project_texts) returns ordered sparse/default and explicit-mode results"
else
    fail "MCP tools/call(search_project_texts) contract mismatch (body: ${body:0:500})"
fi
if echo "$sc" | grep -Fq "$TEST_REPO"; then
    fail "MCP tools/call(search_project_texts) leaked an absolute project path"
else
    pass "MCP tools/call(search_project_texts) keeps paths project-relative"
fi

# ----------------------------------------------------------------------------
# 6b. Phase A read-only console tools (REST + MCP) against the agent project
# ----------------------------------------------------------------------------

log "---- Phase A read-only console tools ----"

# list_project_files via REST — must return a bounded entries array that
# includes README.md (the smoke project always has one).
body="$(api_post /api/projects/list_files "{\"project\":\"$RUNTIME_PROJECT_ID\"}")"
if [ "$(json_get "$body" success)" = "True" ]; then
    pass "list_project_files returns success"
else
    fail "list_project_files did not return success (body: ${body:0:300})"
fi
lpf_entries="$(json_get "$body" output.entries)"
if echo "$lpf_entries" | grep -q "README.md"; then
    pass "list_project_files includes README.md"
else
    fail "list_project_files did not include README.md (got: ${lpf_entries:0:200})"
fi

# search_project_texts via the generic runtime route — one query is the canonical
# single-query path. Validate the sparse item payload rather than redundant metadata.
body="$(api_post /api/tools/call "{\"tool\":\"search_project_texts\",\"params\":{\"project\":\"$RUNTIME_PROJECT_ID\",\"queries\":[{\"pattern\":\"Smoke Project\",\"path\":\"README.md\",\"limit\":10}]}}")"
if [ "$(json_get "$body" success)" = "True" ]; then
    pass "search_project_texts returns success"
else
    fail "search_project_texts did not return success (body: ${body:0:300})"
fi
if [ "$(json_get "$body" output.items.0.output.matches.0.path)" = "README.md" ]; then
    pass "search_project_texts found README.md match"
else
    fail "search_project_texts did not return README.md match (got: ${body:0:200})"
fi

# show_changes via generic runtime — canonical read-only worktree summary.
body="$(show_changes_call)"
if [ "$(json_get "$body" success)" = "True" ]; then
    pass "show_changes returns success"
else
    fail "show_changes did not return success (body: ${body:0:300})"
fi
if [ "$(json_get "$body" output.files_total)" != "None" ]; then
    pass "show_changes returns files_total"
else
    fail "show_changes missing files_total (got: ${body:0:200})"
fi

# list_jobs via REST — bounded summaries, never stdout/stderr bodies.
body="$(api_post /api/jobs/list '{}')"
if [ "$(json_get "$body" success)" = "True" ]; then
    pass "list_jobs returns success"
else
    fail "list_jobs did not return success (body: ${body:0:300})"
fi
lj_serialized="$(json_get "$body" output.jobs)"
if ! echo "$lj_serialized" | grep -qi "stdout\|stderr"; then
    pass "list_jobs summaries omit stdout/stderr bodies"
else
    fail "list_jobs summaries leaked stdout/stderr (got: ${lj_serialized:0:200})"
fi

# job_tail via REST for the completed async shell job — bounded tail.
if [ -n "$JOB_ID" ]; then
    body="$(api_post /api/jobs/tail "{\"job_id\":\"$JOB_ID\",\"tail_lines\":50}")"
    if [ "$(json_get "$body" success)" = "True" ]; then
        pass "job_tail returns success"
    else
        fail "job_tail did not return success (body: ${body:0:300})"
    fi
else
    fail "job_tail skipped: no JOB_ID available"
fi

# MCP tools/list must now expose the Phase A tool names only on expanded coding
# surfaces. adaptive_runtime deliberately keeps these lower-frequency tools
# behind call_runtime_tool rather than expanding their schemas.
phase_a_present=1
if [ "$EXPECTED_SURFACE" = "adaptive_runtime" ]; then
    for tname in list_project_files list_jobs job_log git_diff; do
        if mcp_tool_present "$tname"; then
            phase_a_present=0
            fail "MCP tools/list must keep Phase A long-tail tool $tname behind call_runtime_tool"
        fi
    done
    if [ "$phase_a_present" = "1" ]; then
        pass "MCP adaptive_runtime keeps Phase A long-tail schemas behind the gateway"
    fi
else
    for tname in list_project_files list_jobs job_log git_diff; do
        if mcp_tool_present "$tname"; then
            :
        else
            phase_a_present=0
            fail "MCP tools/list missing $tname"
        fi
    done
    if [ "$EXPECTED_SURFACE" = "local_coding" ] && mcp_tool_present "git_diff_summary"; then
        phase_a_present=0
        fail "MCP tools/list must not expose git_diff_summary on local_coding"
    fi
    if [ "$phase_a_present" = "1" ]; then
        pass "MCP tools/list exposes the Phase A console tools on the expanded model surface"
    fi
fi

# ----------------------------------------------------------------------------
# 6c. apply_unified_diff non-applicable/no-change contract
# ----------------------------------------------------------------------------

log "---- apply_unified_diff (non-applicable/no-change) ----"

build_unified_diff_body() {
    local diff="$1"
    python3 -c '
import json, sys
print(json.dumps({"project": sys.argv[1], "diff": sys.argv[2]}))
' "$RUNTIME_PROJECT_ID" "$diff"
}

BAD_UNIFIED_DIFF='diff --git a/README.md b/README.md
--- a/README.md
+++ b/README.md
@@ -1,1 +1,1 @@
-NONEXISTENT_CONTEXT_LINE
+replacement
'
pre_status="$(api_post /api/projects/git_status "{\"project\":\"$RUNTIME_PROJECT_ID\"}")"
pre_porcelain="$(json_get "$pre_status" output.stdout)"
bad_body="$(build_unified_diff_body "$BAD_UNIFIED_DIFF")"
body="$(api_post /api/projects/apply_unified_diff "$bad_body")"
ud_success="$(json_get "$body" success)"
ud_applied="$(json_get "$body" output.applied)"
ud_can_apply="$(json_get "$body" output.can_apply)"
ud_state_changed="$(json_get "$body" output.state_changed)"
if [ "$ud_success" = "True" ] && [ "$ud_applied" = "False" ] && \
   [ "$ud_can_apply" = "False" ] && [ "$ud_state_changed" = "False" ]; then
    pass "apply_unified_diff(non-applicable) returns a no-change domain outcome"
else
    fail "apply_unified_diff(non-applicable) contract mismatch (body=${body:0:300})"
fi
post_status="$(api_post /api/projects/git_status "{\"project\":\"$RUNTIME_PROJECT_ID\"}")"
post_porcelain="$(json_get "$post_status" output.stdout)"
if [ "$pre_porcelain" = "$post_porcelain" ]; then
    pass "apply_unified_diff failed preflight leaves worktree unchanged"
else
    fail "apply_unified_diff failed preflight mutated the worktree"
fi

if [ "$EXPECTED_SURFACE" = "adaptive_runtime" ]; then
    if mcp_tool_present "apply_unified_diff"; then
        fail "MCP adaptive_runtime must keep apply_unified_diff behind call_runtime_tool"
    else
        pass "MCP adaptive_runtime keeps apply_unified_diff behind call_runtime_tool"
    fi
elif mcp_tool_present "apply_unified_diff"; then
    pass "MCP tools/list exposes apply_unified_diff"
else
    fail "MCP tools/list missing apply_unified_diff"
fi
if mcp_tool_present "apply_patch"; then
    pass "MCP tools/list exposes canonical apply_patch"
else
    fail "MCP tools/list missing canonical apply_patch"
fi
for retired_patch_tool in apply_patch_checked validate_patch; do
    if mcp_tool_present "$retired_patch_tool"; then
        fail "MCP tools/list must not expose retired patch tool $retired_patch_tool"
    fi
done

# ----------------------------------------------------------------------------
# 7. GPT Actions schema smoke (/openapi.json)
# ----------------------------------------------------------------------------

log "---- GPT Actions schema (/openapi.json) ----"

SCHEMA="$(api_get /openapi.json)"
python3 - "$SCHEMA" "$RUNTIME_PROJECT_ID" <<'PY'
import json, sys
schema = json.loads(sys.argv[1])
errors = []

# Collect operation ids.
ops = []
for path, methods in schema.get("paths", {}).items():
    for method, op in methods.items():
        ops.append(op.get("operationId"))
ops_set = set(ops)

expected_ops = {
    "listRuntimeTools", "listProjects", "registerProject", "createProject",
    "getRuntimeStatus",
    "getProjectGitStatus", "listProjectFiles",
    "applyUnifiedDiff",
    "runProjectShellCommand", "gitRestorePaths",
    "discardUntrackedFiles", "importConversationFilesToProject", "startProjectShellJob",
    "listRuntimeJobs", "getRuntimeJobTail", "callRuntimeTool",
}
missing = expected_ops - ops_set
extra = ops_set - expected_ops
if missing:
    errors.append(f"missing operationIds: {sorted(missing)}")
if extra:
    errors.append(f"unexpected operationIds: {sorted(extra)}")

# Operation count must stay small (<= 30) and match the current dedicated
# GPT Actions surface in src/openapi.rs.
if len(ops) > 30:
    errors.append(f"too many operations: {len(ops)} (must be <= 30)")
if len(ops) != len(expected_ops):
    errors.append(f"operation count must be {len(expected_ops)}, got {len(ops)}")

tool_call = (
    schema
    .get("components", {})
    .get("schemas", {})
    .get("ToolCallRequest", {})
)
tool_desc = (
    tool_call
    .get("properties", {})
    .get("tool", {})
    .get("description", "")
)
# The generic GPT Action advertises only current model-facing runtime tools.
# work_on_project is the canonical external task bootstrap; start_coding_task is retired.
for runtime_tool in ["work_on_project", "finish_coding_task"]:
    if runtime_tool not in tool_desc:
        errors.append(f"ToolCallRequest.tool description missing {runtime_tool}")

# Keep this cross-language check aligned with MODEL_TOOL_DESCRIPTION_MAX_CHARS.
MODEL_TOOL_DESCRIPTION_MAX_CHARS = 900
# Phase 2: each operation description must fit the repository model budget.
for path, methods in schema.get("paths", {}).items():
    for method, op in methods.items():
        desc = op.get("description", "") or ""
        if len(desc) > MODEL_TOOL_DESCRIPTION_MAX_CHARS:
            errors.append(
                f"{method} {path} operationId {op.get('operationId')} "
                f"description too long: {len(desc)} chars "
                f"(hard budget {MODEL_TOOL_DESCRIPTION_MAX_CHARS})"
            )

# Forbidden legacy/admin/internal paths must not appear in the schema paths.
# list_files, jobs/list, and jobs/tail remain dedicated GPT Actions; canonical
# change review and Job observation use callRuntimeTool. Legacy dedicated patch routes are explicitly forbidden;
# the current apply_patch tool is runtime-only through callRuntimeTool. jobs/stop,
# audit, legacy shell/codex, console, and /mcp also remain forbidden.
forbidden = ["/api/audit/sessions", "/api/audit/session", "/api/audit/stats",
             "/api/jobs/stop",
             "/api/projects/replace_in_file", "/api/projects/write_file",
             "/api/projects/apply_patch", "/api/projects/apply_patch_checked", "/api/projects/validate_patch",
             "/api/messages", "/api/files", "/api/desktop/task_op", "/api/desktop/task",
             "/api/shell/run", "/api/shell/job", "/api/shell/file",
             "/mcp", "/openapi.json", "/console", "/console/app.js", "/console/styles.css"]
paths = set(schema.get("paths", {}).keys())
for fp in forbidden:
    if fp in paths:
        errors.append(f"forbidden path present in schema: {fp}")

# Legacy /api/codex/* sub-routes and run_codex must remain removed from
# GPT Actions/OpenAPI.
legacy_codex = ["/api/codex/command_request_op", "/api/codex/command_request",
                "/api/codex/context", "/api/codex/context_batch",
                "/api/codex/apply_patch", "/api/codex/edit",
                "/api/codex/artifact", "/api/codex/git",
                "/api/codex/job", "/api/codex/report",
                "/api/codex/projects", "/api/codex/run"]
for p in paths:
    if p in legacy_codex:
        errors.append(f"legacy codex path present in schema: {p}")

# Descriptions must not claim server-side projects.toml is the runtime source.
blob = json.dumps(schema)
if "projects.toml" in blob and "runtime project source" in blob.lower():
    errors.append("schema mentions projects.toml as runtime project source")

# Every path must be POST-only.
for path, methods in schema.get("paths", {}).items():
    for method in methods:
        if method != "post":
            errors.append(f"non-POST method '{method}' on path {path}")

# Each operationId must be unique (no duplicates across the schema).
seen_ids = {}
for path, methods in schema.get("paths", {}).items():
    for method, op in methods.items():
        oid = op.get("operationId")
        if oid in seen_ids:
            errors.append(f"duplicate operationId '{oid}' on {method} {path} and {seen_ids[oid]}")
        else:
            seen_ids[oid] = f"{method} {path}"

# Every requestBody schema must declare additionalProperties=false at the top
# level so GPT Actions rejects unknown fields. Inner properties (e.g.
# ToolCallRequest.params) may still allow arbitrary keys.
schemas = schema.get("components", {}).get("schemas", {})
for path, methods in schema.get("paths", {}).items():
    for method, op in methods.items():
        ref = op.get("requestBody", {}).get("content", {}).get("application/json", {}).get("schema", {}).get("$ref", "")
        if not ref:
            continue
        name = ref.split("/")[-1]
        sch = schemas.get(name)
        if sch is None:
            errors.append(f"{method} {path} requestBody references unknown schema '{name}'")
            continue
        if sch.get("additionalProperties") is not False:
            errors.append(f"{method} {path} requestBody schema '{name}' must have additionalProperties=false")

# Mutation/execution actions must mention side effects and Bearer auth (or
# equivalent) so GPT callers understand they are not read-only.
mutation_paths = [
    "/api/projects/register",
    "/api/projects/create",
    "/api/projects/apply_unified_diff",
    "/api/projects/run_shell",
    "/api/projects/git_restore_paths",
    "/api/projects/discard_untracked",
    "/api/projects/run_job",
]
for path in mutation_paths:
    op = schema.get("paths", {}).get(path, {}).get("post", {})
    desc = (op.get("description") or "").lower()
    if "side effect" not in desc:
        errors.append(f"{path} mutation description should mention side effects")
    if "bearer auth" not in desc:
        errors.append(f"{path} mutation description should mention Bearer auth")

# Read-only actions must explicitly say read-only or never writes so GPT
# callers can tell them apart from mutations. callRuntimeTool is excluded
# because it is a generic escape hatch.
readonly_paths = [
    "/api/tools/list",
    "/api/projects/list",
    "/api/runtime/status",
    "/api/jobs/list",
    "/api/jobs/tail",
    "/api/projects/git_status",
    "/api/projects/list_files",
]
for path in readonly_paths:
    op = schema.get("paths", {}).get(path, {}).get("post", {})
    desc = (op.get("description") or "").lower()
    if "read-only" not in desc and "never writes" not in desc:
        errors.append(f"{path} read-only description should mention read-only or never writes")

if errors:
    print("FAIL")
    for e in errors:
        print("  - " + e, file=sys.stderr)
    sys.exit(1)
print(f"OK ops={len(ops)} paths={len(paths)}")
PY
if [ $? -eq 0 ]; then
    pass "/openapi.json operation set + POST-only + no legacy/admin paths + additionalProperties=false + mutation/readonly descriptions"
else
    fail "/openapi.json schema checks failed (see stderr above)"
fi

# ----------------------------------------------------------------------------
# 7b. MCP App console (Phase B) — public static entry + protected data API
# ----------------------------------------------------------------------------

log "---- MCP App console (/console) ----"

# The console HTML shell is public (no Bearer auth) and must reference the
# bundled assets. It never embeds the token.
console_html="$(curl -sS --max-time 10 "http://127.0.0.1:${PORT}/console" 2>/dev/null)"
if echo "$console_html" | grep -q "WebCodex" && \
   echo "$console_html" | grep -q "/console/app.js"; then
    pass "GET /console serves public HTML shell"
else
    fail "GET /console did not return expected HTML shell (got: ${console_html:0:200})"
fi

# The bundled JS is public. Assert on stable properties (non-empty resource,
# correct content type, no token/credential material) rather than any specific
# JavaScript implementation text, which may be refactored. The console page is
# already verified above to reference the bundle and embed no token literal.
console_js="$(curl -sS --max-time 10 "http://127.0.0.1:${PORT}/console/app.js")"
js_bytes="${#console_js}"
js_type="$(curl -sS -o /dev/null -w '%{content_type}' --max-time 10 "http://127.0.0.1:${PORT}/console/app.js")"
console_js_ok=1
if [ "$js_bytes" -le 0 ]; then
    console_js_ok=0
    fail "GET /console/app.js returned an empty resource"
fi
case "$js_type" in
    application/javascript*|text/javascript*|application/x-javascript*)
        ;;
    *)
        console_js_ok=0
        fail "GET /console/app.js content-type '$js_type' is not a JS type"
        ;;
esac
if echo "$console_js" | grep -qi "WEBCODEX_TOKEN\|wc_agent_secret"; then
    console_js_ok=0
    fail "GET /console/app.js contains token or credential material"
fi
if [ "$console_js_ok" = "1" ]; then
    pass "GET /console/app.js returns a non-empty JS resource (${js_bytes} bytes) without token material"
fi

# The bundle must never embed the token key in the DOM.
if echo "$console_html" | grep -qi "webcodex_token"; then
    fail "console HTML leaked WEBCODEX_TOKEN literal"
else
    pass "console HTML does not leak WEBCODEX_TOKEN literal"
fi

# The protected data API must still reject unauthenticated requests even though
# the console page itself is public.
no_auth_status=$(curl -sS -o /dev/null -w "%{http_code}" --max-time 10 \
    -H "Content-Type: application/json" \
    -X POST "http://127.0.0.1:${PORT}/api/runtime/status" \
    -d '{}' 2>/dev/null)
if [ "$no_auth_status" = "401" ]; then
    pass "POST /api/runtime/status rejects unauthenticated request (401)"
else
    fail "POST /api/runtime/status without token returned HTTP ${no_auth_status} (expected 401)"
fi

# runtime_status now carries per-agent last_seen + stale_count for the console.
status_body="$(api_post /api/runtime/status '{}')"
if [ "$(json_get "$status_body" output.agents.stale_count)" != "None" ]; then
    pass "runtime_status exposes agents.stale_count"
else
    fail "runtime_status missing agents.stale_count"
fi

# ----------------------------------------------------------------------------
# 7c. Phase 2: generic callRuntimeTool / /api/tools/list enhancements
# ----------------------------------------------------------------------------

log "---- Phase 2: callRuntimeTool / tools/list ----"

# /api/tools/list must return names + count alongside the back-compat tools array.
body="$(api_post /api/tools/list '{}')"
if tools_list_check="$(printf '%s' "$body" | python3 -c '
import json, sys

try:
    d = json.load(sys.stdin)
except Exception as exc:
    print(f"invalid JSON: {exc}")
    sys.exit(1)

tools = d.get("tools")
names = d.get("names")
count = d.get("count")
categories = d.get("categories")
flows = d.get("recommended_flows")
errors = []

if not isinstance(tools, list) or not tools:
    errors.append("tools must be a non-empty list")
if not isinstance(names, list) or not names:
    errors.append("names must be a non-empty list")
if not isinstance(count, int):
    errors.append("count must be an integer")
elif isinstance(tools, list) and count != len(tools):
    errors.append(f"count {count} does not match tools length {len(tools)}")
if isinstance(names, list):
    # /api/tools/list exposes the current operator-visible surface. The retired
    # start_coding_task compatibility entry is intentionally omitted;
    # work_on_project is the canonical task bootstrap.
    missing = sorted({
        "work_on_project",
        "finish_coding_task",
        "show_changes",
        "git_diff_hunks",
        "observe_jobs",
        "list_tools",
    } - set(names))
    if missing:
        errors.append(f"names missing {missing}")
if not isinstance(categories, dict) or not categories:
    errors.append("categories must be a non-empty object")
if not isinstance(flows, list) or not flows:
    errors.append("recommended_flows must be a non-empty list")

if errors:
    print("; ".join(errors))
    sys.exit(1)
print(f"count={count} tools={len(tools)}")
')"; then
    pass "/api/tools/list names/count/tools/categories/recommended_flows are consistent ($tools_list_check)"
else
    fail "/api/tools/list response invalid ($tools_list_check; body: ${body:0:200})"
fi

# callRuntimeTool: params omitted -> list_tools succeeds.
body="$(api_post /api/tools/call '{"tool":"list_tools"}')"
if printf '%s' "$body" | python3 -c 'import json,sys; sys.exit(0 if json.load(sys.stdin).get("success") is True else 1)'; then
    pass "callRuntimeTool(list_tools) params omitted succeeds"
else
    fail "callRuntimeTool(list_tools) params omitted failed (body: ${body:0:300})"
fi

# callRuntimeTool: params null -> list_tools succeeds.
body="$(api_post /api/tools/call '{"tool":"list_tools","params":null}')"
if printf '%s' "$body" | python3 -c 'import json,sys; sys.exit(0 if json.load(sys.stdin).get("success") is True else 1)'; then
    pass "callRuntimeTool(list_tools) params null succeeds"
else
    fail "callRuntimeTool(list_tools) params null failed (body: ${body:0:300})"
fi

# callRuntimeTool: retired arguments envelope is rejected; use params or flattened fields.
body="$(api_post /api/tools/call '{"tool":"list_tools","arguments":null}')"
if printf '%s' "$body" | python3 -c 'import json,sys; body=json.load(sys.stdin); err=str(body.get("error", "")); sys.exit(0 if body.get("status") == 400 and "arguments" in err and "no longer supported" in err else 1)'; then
    pass "callRuntimeTool(list_tools) rejects retired arguments envelope"
else
    fail "callRuntimeTool(list_tools) accepted retired arguments envelope (body: ${body:0:300})"
fi

# callRuntimeTool: show_changes against the agent project succeeds.
body="$(show_changes_call)"
if [ "$(json_get "$body" success)" = "True" ]; then
    pass "callRuntimeTool(show_changes) routes to agent and succeeds"
else
    fail "callRuntimeTool(show_changes) failed (body: ${body:0:300})"
fi

# callRuntimeTool: unknown tool returns a useful error (not a 5xx / empty).
body="$(api_post /api/tools/call '{"tool":"definitely_not_a_tool"}')"
unk_err="$(json_get "$body" error)"
if [ -n "$unk_err" ] && [ "$unk_err" != "None" ] && \
   echo "$unk_err" | grep -q "definitely_not_a_tool" && \
   (echo "$unk_err" | grep -q "listRuntimeTools" || echo "$unk_err" | grep -q "list_tools"); then
    pass "callRuntimeTool(unknown tool) returns useful discovery hint"
else
    fail "callRuntimeTool(unknown tool) error not useful (got: ${unk_err:0:200})"
fi

# Deterministic workflow tools via generic callRuntimeTool, using flattened
# GPT Action-style fields.
log "---- Deterministic workflow tool smoke ----"

workflow_session_id=""
body="$(api_post /api/tools/call "{\"tool\":\"work_on_project\",\"project\":\"$RUNTIME_PROJECT_ID\",\"instruction\":\"e2e deterministic coding task smoke\"}")"
if workflow_session_id="$(python3 - "$body" <<'PY'
import json, sys

try:
    data = json.loads(sys.argv[1])
except Exception as exc:
    print(f"invalid JSON: {exc}", file=sys.stderr)
    sys.exit(1)

errors = []
output = data.get("output") if isinstance(data, dict) else None
output = output if isinstance(output, dict) else {}
session_id = output.get("session_id")

if data.get("success") is not True:
    errors.append("success must be true")
if not isinstance(session_id, str) or not session_id.startswith("wc_sess_"):
    errors.append("output.session_id must start with wc_sess_")
if output.get("continuation") != "created":
    errors.append("output.continuation must be created")
for field in ["workspace", "workflow", "instructions", "semantic_navigation"]:
    if not isinstance(output.get(field), dict):
        errors.append(f"output.{field} must be an object")
if "readiness" in output and not isinstance(output.get("readiness"), dict):
    errors.append("output.readiness must be an object when present")
for retired in [
    "runtime_status",
    "connection_state",
    "authority",
    "recommended_flow",
    "deterministic",
    "llm_summary",
]:
    if retired in output:
        errors.append(f"compact work_on_project output must omit {retired}")

if errors:
    print("; ".join(errors), file=sys.stderr)
    sys.exit(1)
print(session_id)
PY
)"; then
    pass "callRuntimeTool(work_on_project) returns compact workflow startup"
else
    workflow_session_id=""
    fail "callRuntimeTool(work_on_project) workflow assertions failed (body: ${body:0:300})"
fi

if [ -n "$workflow_session_id" ]; then
    body="$(api_post /api/tools/call "{\"tool\":\"show_changes\",\"project\":\"$RUNTIME_PROJECT_ID\",\"session_id\":\"$workflow_session_id\",\"include_diff\":false}")"
    if python3 - "$body" "$workflow_session_id" <<'PY'
import json, sys

try:
    data = json.loads(sys.argv[1])
except Exception as exc:
    print(f"invalid JSON: {exc}", file=sys.stderr)
    sys.exit(1)

session_id = sys.argv[2]
output = data.get("output") if isinstance(data, dict) else None
output = output if isinstance(output, dict) else {}
session = output.get("session") if isinstance(output.get("session"), dict) else {}
errors = []

if data.get("success") is not True:
    errors.append("success must be true")
if session.get("found") is not True:
    errors.append("output.session.found must be true")
if session.get("session_id") != session_id:
    errors.append("output.session.session_id must match returned session_id")

if errors:
    print("; ".join(errors), file=sys.stderr)
    sys.exit(1)
PY
    then
        pass "callRuntimeTool(show_changes) accepts explicit workflow session_id"
    else
        fail "callRuntimeTool(show_changes) explicit session smoke failed (body: ${body:0:300})"
    fi
else
    fail "callRuntimeTool(show_changes) skipped: work_on_project did not return a session_id"
fi

if [ -n "$workflow_session_id" ]; then
    body="$(api_post /api/tools/call "{\"tool\":\"finish_coding_task\",\"project\":\"$RUNTIME_PROJECT_ID\",\"session_id\":\"$workflow_session_id\",\"include_diff\":false,\"include_hygiene\":true,\"include_handoff\":true,\"include_validation_summary\":true}")"
    if python3 - "$body" "$workflow_session_id" <<'PY'
import json, sys

try:
    data = json.loads(sys.argv[1])
except Exception as exc:
    print(f"invalid JSON: {exc}", file=sys.stderr)
    sys.exit(1)

session_id = sys.argv[2]
output = data.get("output") if isinstance(data, dict) else None
output = output if isinstance(output, dict) else {}
changes = output.get("changes") if isinstance(output.get("changes"), dict) else {}
validation = output.get("validation") if isinstance(output.get("validation"), dict) else {}
errors = []

if data.get("success") is not True:
    errors.append("success must be true")
if output.get("deterministic") is not True:
    errors.append("output.deterministic must be true")
if output.get("llm_summary") is not False:
    errors.append("output.llm_summary must be false")
if output.get("session_id") != session_id:
    errors.append("output.session_id must match returned session_id")
if not isinstance(output.get("workspace"), dict):
    errors.append("output.workspace must exist")
if not isinstance(changes.get("show_changes"), dict):
    errors.append("output.changes.show_changes must exist")
if not isinstance(output.get("hygiene"), dict):
    errors.append("output.hygiene must exist")
if not isinstance(output.get("handoff"), dict):
    errors.append("output.handoff must exist")
if validation.get("available") is not False:
    errors.append("output.validation.available must be false")
if not isinstance(output.get("final_warnings"), list):
    errors.append("output.final_warnings must be an array")

if errors:
    print("; ".join(errors), file=sys.stderr)
    sys.exit(1)
PY
    then
        pass "callRuntimeTool(finish_coding_task) returns deterministic finish summary"
    else
        fail "callRuntimeTool(finish_coding_task) workflow assertions failed (body: ${body:0:300})"
    fi
else
    fail "callRuntimeTool(finish_coding_task) skipped: work_on_project did not return a session_id"
fi

body="$(api_post /api/tools/call "{\"tool\":\"finish_coding_task\",\"project\":\"$RUNTIME_PROJECT_ID\"}")"
missing_session_status="$(json_get "$body" status)"
missing_session_error="$(json_get "$body" error)"
if [ "$missing_session_status" = "400" ] && echo "$missing_session_error" | grep -q "session_id"; then
    pass "callRuntimeTool(finish_coding_task) rejects missing explicit session_id"
else
    fail "callRuntimeTool(finish_coding_task) missing session_id was not rejected clearly (body: ${body:0:300})"
fi

# ----------------------------------------------------------------------------
# 7d. Dedicated mutation actions plus canonical runtime cleanup against probe files only
# ----------------------------------------------------------------------------
#
# These are executable mutations with side effects. To avoid breaking the
# smoke repo, every probe operates ONLY on throwaway probe files inside the
# temporary TEST_REPO (never on README.md, src.rs, or any real project file).
# Probe files are removed afterwards so the worktree returns to a clean state.

log "---- Phase 3: dedicated mutation actions (probe files only) ----"

# Build a JSON request body with python3 for safe escaping. The argument is a
# JSON string that is parsed and re-serialized (validates + normalizes).
build_body() {
    python3 -c '
import json, sys
obj = json.loads(sys.argv[1])
print(json.dumps(obj))
' "$1"
}

# applyUnifiedDiff — apply a probe diff that creates a new file,
# then verify via show_changes that the probe file appears as untracked.
PROBE_PATCH='diff --git a/APPLY_CHECKED_PROBE.txt b/APPLY_CHECKED_PROBE.txt
new file mode 100644
--- /dev/null
+++ b/APPLY_CHECKED_PROBE.txt
@@ -0,0 +1 @@
+probe
'
apc_body="$(python3 -c '
import json, sys
print(json.dumps({"project": sys.argv[1], "diff": sys.argv[2]}))
' "$RUNTIME_PROJECT_ID" "$PROBE_PATCH")"
body="$(api_post /api/projects/apply_unified_diff "$apc_body")"
apc_success="$(json_get "$body" success)"
if [ "$apc_success" = "True" ]; then
    pass "applyUnifiedDiff(probe) returns success"
else
    fail "applyUnifiedDiff(probe) failed (body: ${body:0:300})"
fi
# Verify the probe file now shows up in the worktree via show_changes.
body="$(show_changes_call)"
changed_files="$(json_get "$body" output.files)"
if echo "$changed_files" | grep -q "APPLY_CHECKED_PROBE.txt"; then
    pass "applyUnifiedDiff probe file visible in show_changes"
else
    fail "applyUnifiedDiff probe file not in show_changes (got: ${changed_files:0:200})"
fi

# Canonical runtime delete — delete the probe file created above.
del_body="$(build_body "{\"tool\":\"delete_project_files\",\"params\":{\"project\":\"$RUNTIME_PROJECT_ID\",\"paths\":[\"APPLY_CHECKED_PROBE.txt\"]}}")"
body="$(api_post /api/tools/call "$del_body")"
del_success="$(json_get "$body" success)"
if [ "$del_success" = "True" ]; then
    pass "callRuntimeTool(delete_project_files) returns success"
else
    fail "callRuntimeTool(delete_project_files) failed (body: ${body:0:300})"
fi
# Verify the probe file is gone via list_files root listing.
body="$(api_post /api/projects/list_files "{\"project\":\"$RUNTIME_PROJECT_ID\"}")"
lpf_entries="$(json_get "$body" output.entries)"
if ! echo "$lpf_entries" | grep -q "APPLY_CHECKED_PROBE.txt"; then
    pass "deleteProjectFiles removed probe file"
else
    fail "deleteProjectFiles did not remove probe file (got: ${lpf_entries:0:200})"
fi

# discardUntrackedFiles — create a fresh untracked probe file, then discard it.
body="$(api_post /api/projects/run_shell "{\"project\":\"$RUNTIME_PROJECT_ID\",\"command\":\"printf probe > UNTRACKED_PROBE.txt\"}")"
disc_body="$(build_body "{\"project\":\"$RUNTIME_PROJECT_ID\",\"paths\":[\"UNTRACKED_PROBE.txt\"]}")"
body="$(api_post /api/projects/discard_untracked "$disc_body")"
disc_success="$(json_get "$body" success)"
if [ "$disc_success" = "True" ]; then
    pass "discardUntrackedFiles(probe) returns success"
else
    fail "discardUntrackedFiles(probe) failed (body: ${body:0:300})"
fi
body="$(api_post /api/projects/list_files "{\"project\":\"$RUNTIME_PROJECT_ID\"}")"
lpf_entries="$(json_get "$body" output.entries)"
if ! echo "$lpf_entries" | grep -q "UNTRACKED_PROBE.txt"; then
    pass "discardUntrackedFiles removed untracked probe file"
else
    fail "discardUntrackedFiles did not remove probe file (got: ${lpf_entries:0:200})"
fi

# gitRestorePaths — create a tracked probe file, commit it, modify it, then
# restore it. This verifies restore returns the file to its committed state.
body="$(api_post /api/projects/run_shell "{\"project\":\"$RUNTIME_PROJECT_ID\",\"command\":\"printf original > RESTORE_PROBE.txt && git add RESTORE_PROBE.txt && git commit -m probe >/dev/null 2>&1\"}")"
body="$(api_post /api/projects/run_shell "{\"project\":\"$RUNTIME_PROJECT_ID\",\"command\":\"printf modified > RESTORE_PROBE.txt\"}")"
rest_body="$(build_body "{\"project\":\"$RUNTIME_PROJECT_ID\",\"paths\":[\"RESTORE_PROBE.txt\"]}")"
body="$(api_post /api/projects/git_restore_paths "$rest_body")"
rest_success="$(json_get "$body" success)"
if [ "$rest_success" = "True" ]; then
    pass "gitRestorePaths(probe) returns success"
else
    fail "gitRestorePaths(probe) failed (body: ${body:0:300})"
fi
body="$(api_post /api/tools/call "{\"tool\":\"read_files\",\"params\":{\"project\":\"$RUNTIME_PROJECT_ID\",\"items\":[{\"path\":\"RESTORE_PROBE.txt\"}]}}")"
restore_content="$(json_get "$body" output.items.0.output.text)"
if echo "$restore_content" | grep -q "original"; then
    pass "gitRestorePaths restored probe file to committed content"
else
    fail "gitRestorePaths did not restore content (got: ${restore_content:0:120})"
fi

# Clean up the tracked probe file so the worktree returns to a clean state.
body="$(api_post /api/projects/run_shell "{\"project\":\"$RUNTIME_PROJECT_ID\",\"command\":\"git rm -f RESTORE_PROBE.txt >/dev/null 2>&1 && git commit -m cleanup-probe >/dev/null 2>&1\"}")" || true

# ----------------------------------------------------------------------------
# 7e. Phase 4: structured edit tools (apply_text_edits / write_project_file)
#     via callRuntimeTool, against probe files only
# ----------------------------------------------------------------------------

log "---- Phase 4: structured edit tools (probe files only) ----"

# write_project_file via callRuntimeTool — create EDIT_PROBE.txt.
wpf_body="$(python3 -c '
import json, sys
print(json.dumps({
    "tool": "write_project_file",
    "params": {
        "project": sys.argv[1],
        "path": "EDIT_PROBE.txt",
        "content": "hello world\n"
    }
}))
' "$RUNTIME_PROJECT_ID")"
body="$(api_post /api/tools/call "$wpf_body")"
wpf_success="$(json_get "$body" success)"
wpf_created="$(json_get "$body" output.created)"
if [ "$wpf_success" = "True" ] && [ "$wpf_created" = "True" ]; then
    pass "callRuntimeTool(write_project_file) creates EDIT_PROBE.txt"
else
    fail "callRuntimeTool(write_project_file) did not create probe (success=$wpf_success created=$wpf_created body=${body:0:300})"
fi
wpf_sha="$(json_get "$body" output.sha256)"
if [ -n "$wpf_sha" ] && [ "$wpf_sha" != "None" ] && [ ${#wpf_sha} -eq 64 ]; then
    pass "write_project_file returns 64-char sha256"
else
    fail "write_project_file missing sha256 (got: $wpf_sha)"
fi

# read_files confirms the probe content.
body="$(api_post /api/tools/call "{\"tool\":\"read_files\",\"params\":{\"project\":\"$RUNTIME_PROJECT_ID\",\"items\":[{\"path\":\"EDIT_PROBE.txt\"}]}}")"
if echo "$(json_get "$body" output.items.0.output.text)" | grep -q "hello world"; then
    pass "read_files confirms EDIT_PROBE.txt content"
else
    fail "read_files did not confirm probe content (got: ${body:0:200})"
fi

# apply_text_edits via callRuntimeTool — replace_exact "world" -> "rust" on
# EDIT_PROBE.txt, guarded by the create-time sha256.
ate_body="$(python3 -c '
import json, sys
print(json.dumps({
    "tool": "apply_text_edits",
    "params": {
        "project": sys.argv[1],
        "changes": [{
            "kind": "edit",
            "path": "EDIT_PROBE.txt",
            "expected_sha256": sys.argv[2],
            "edits": [{"kind": "replace_exact", "old_text": "world", "new_text": "rust"}]
        }]
    }
}))
' "$RUNTIME_PROJECT_ID" "$wpf_sha")"
body="$(api_post /api/tools/call "$ate_body")"
ate_success="$(json_get "$body" success)"
ate_changed="$(json_get "$body" output.changed)"
if [ "$ate_success" = "True" ] && [ "$ate_changed" = "True" ]; then
    pass "callRuntimeTool(apply_text_edits) edits EDIT_PROBE.txt"
else
    fail "callRuntimeTool(apply_text_edits) did not edit probe (success=$ate_success changed=$ate_changed body=${body:0:300})"
fi

# read_files confirms the edited content.
body="$(api_post /api/tools/call "{\"tool\":\"read_files\",\"params\":{\"project\":\"$RUNTIME_PROJECT_ID\",\"items\":[{\"path\":\"EDIT_PROBE.txt\"}]}}")"
if echo "$(json_get "$body" output.items.0.output.text)" | grep -q "hello rust"; then
    pass "read_files confirms apply_text_edits edit"
else
    fail "read_files did not confirm edit (got: ${body:0:200})"
fi

# apply_text_edits with a stale expected_sha256 (the create-time hash no longer
# matches the edited file) must reject the whole batch WITHOUT modifying it.
ate_miss="$(python3 -c '
import json, sys
print(json.dumps({
    "tool": "apply_text_edits",
    "params": {
        "project": sys.argv[1],
        "changes": [{
            "kind": "edit",
            "path": "EDIT_PROBE.txt",
            "expected_sha256": sys.argv[2],
            "edits": [{"kind": "replace_exact", "old_text": "rust", "new_text": "x"}]
        }]
    }
}))
' "$RUNTIME_PROJECT_ID" "$wpf_sha")"
body="$(api_post /api/tools/call "$ate_miss")"
ate_error_kind="$(json_get "$body" output.error_kind)"
if [ "$(json_get "$body" success)" = "False" ] && [ "$ate_error_kind" = "sha256_conflict" ]; then
    pass "apply_text_edits(stale sha guard) fails with sha256_conflict"
else
    fail "apply_text_edits(stale sha guard) did not report sha256_conflict (error_kind=$ate_error_kind body: ${body:0:200})"
fi
body="$(api_post /api/tools/call "{\"tool\":\"read_files\",\"params\":{\"project\":\"$RUNTIME_PROJECT_ID\",\"items\":[{\"path\":\"EDIT_PROBE.txt\"}]}}")"
if echo "$(json_get "$body" output.items.0.output.text)" | grep -q "hello rust"; then
    pass "apply_text_edits(stale sha guard) left file unchanged"
else
    fail "apply_text_edits(stale sha guard) modified the file (got: ${body:0:200})"
fi

# Canonical runtime delete removes the probe so the worktree returns to clean.
del_body="$(build_body "{\"tool\":\"delete_project_files\",\"params\":{\"project\":\"$RUNTIME_PROJECT_ID\",\"paths\":[\"EDIT_PROBE.txt\"]}}")"
body="$(api_post /api/tools/call "$del_body")"
if [ "$(json_get "$body" success)" = "True" ]; then
    pass "deleteProjectFiles removes EDIT_PROBE.txt"
else
    fail "deleteProjectFiles did not remove probe (body: ${body:0:300})"
fi

# ----------------------------------------------------------------------------
# 7f. Removed-tool negative contract (probe file only)
#
# replace_in_file was removed entirely, so a call to it must fail
# deterministically as an unknown tool, must not be routed to any compatibility
# path, and must never mutate the worktree.
# ----------------------------------------------------------------------------

log "---- removed legacy edit tool negative contract ----"

# Create a probe file so we can prove a removed-tool call leaves content intact.
neg_body="$(python3 -c '
import json, sys
print(json.dumps({
    "tool": "write_project_file",
    "params": {
        "project": sys.argv[1],
        "path": "NEG_PROBE.txt",
        "content": "unchanged\n"
    }
}))
' "$RUNTIME_PROJECT_ID")"
body="$(api_post /api/tools/call "$neg_body")"
if [ "$(json_get "$body" success)" = "True" ]; then
    pass "write_project_file creates NEG_PROBE.txt for removed-tool probe"
else
    fail "write_project_file did not create NEG_PROBE.txt (body: ${body:0:300})"
fi

# replace_in_file must fail as an unknown tool and never mutate. The unknown
# tool path is a 400 json_error envelope (status + error, no success field).
neg_rif="$(python3 -c '
import json, sys
print(json.dumps({
    "tool": "replace_in_file",
    "params": {
        "project": sys.argv[1],
        "path": "NEG_PROBE.txt",
        "old": "unchanged",
        "new": "mutated"
    }
}))
' "$RUNTIME_PROJECT_ID")"
body="$(api_post /api/tools/call "$neg_rif")"
neg_status="$(json_get "$body" status)"
neg_err="$(json_get "$body" error)"
if [ "$neg_status" = "400" ] && \
   echo "$neg_err" | grep -q "unknown tool" && \
   echo "$neg_err" | grep -q "replace_in_file"; then
    pass "replace_in_file fails deterministically as an unknown tool"
else
    fail "replace_in_file did not fail as an unknown tool (status=$neg_status error: ${neg_err:0:200})"
fi
body="$(api_post /api/tools/call "{\"tool\":\"read_files\",\"params\":{\"project\":\"$RUNTIME_PROJECT_ID\",\"items\":[{\"path\":\"NEG_PROBE.txt\"}]}}")"
if echo "$(json_get "$body" output.items.0.output.text)" | grep -q "unchanged"; then
    pass "replace_in_file unknown-tool failure left file unchanged"
else
    fail "replace_in_file probe was modified (got: ${body:0:200})"
fi

# Clean up the probe file so the worktree returns to a clean state.
del_body="$(build_body "{\"tool\":\"delete_project_files\",\"params\":{\"project\":\"$RUNTIME_PROJECT_ID\",\"paths\":[\"NEG_PROBE.txt\"]}}")"
body="$(api_post /api/tools/call "$del_body")" || true

# ----------------------------------------------------------------------------
# 7g. Unified-diff hardening: a large diff still travels over typed stdin,
#     while a non-applicable diff must return a definite no-change outcome.
#     The diff payload never enters the shell command string.
# ----------------------------------------------------------------------------

log "---- unified diff hardening (large + non-applicable) ----"

# Capture the worktree state before the hardening probes (should be clean).
pre_harden_status="$(api_post /api/projects/git_status "{\"project\":\"$RUNTIME_PROJECT_ID\"}")"
pre_harden_porcelain="$(json_get "$pre_harden_status" output.stdout)"

# A LARGE diff (over the 16,000-byte authored raw shell command limit) that
# creates a new file. It still applies because the diff travels over stdin,
# not the command string.
LARGE_APPLY_PATCH="$(python3 - <<'PY'
print("diff --git a/LARGE_APPLY_PROBE.md b/LARGE_APPLY_PROBE.md")
print("new file mode 100644")
print("--- /dev/null")
print("+++ b/LARGE_APPLY_PROBE.md")
print("@@ -0,0 +1,300 @@")
for i in range(300):
    print(f"+large-line-{i:04d}-" + ("x" * 48))
PY
)"
LARGE_APPLY_PATCH="${LARGE_APPLY_PATCH}"$'\n'
lap_bytes="$(printf '%s' "$LARGE_APPLY_PATCH" | wc -c | tr -d ' ')"
if [ "${lap_bytes:-0}" -gt 16000 ] 2>/dev/null; then
    pass "LARGE_APPLY_PATCH is ${lap_bytes} bytes (> 16000 authored command limit)"
else
    fail "LARGE_APPLY_PATCH must exceed the 16000-byte authored command limit (got ${lap_bytes} bytes)"
fi
lap_body="$(python3 -c 'import json,sys; print(json.dumps({"project": sys.argv[1], "diff": sys.argv[2]}))' "$RUNTIME_PROJECT_ID" "$LARGE_APPLY_PATCH")"
body="$(api_post /api/projects/apply_unified_diff "$lap_body")"
lap_success="$(json_get "$body" success)"
lap_applied="$(json_get "$body" output.applied)"
if [ "$lap_success" = "True" ] && [ "$lap_applied" = "True" ]; then
    pass "applyUnifiedDiff applies large diff over command limit"
else
    fail "applyUnifiedDiff large diff did not apply (success=$lap_success applied=$lap_applied body=${body:0:300})"
fi
# Verify the large probe file now shows up in the worktree.
body="$(show_changes_call)"
if echo "$(json_get "$body" output.files)" | grep -q "LARGE_APPLY_PROBE.md"; then
    pass "large apply probe file visible in show_changes"
else
    fail "large apply probe file not visible (got: ${body:0:200})"
fi
# Clean up the large probe so the worktree returns to a clean state.
del_body="$(build_body "{\"tool\":\"delete_project_files\",\"params\":{\"project\":\"$RUNTIME_PROJECT_ID\",\"paths\":[\"LARGE_APPLY_PROBE.md\"]}}")"
body="$(api_post /api/tools/call "$del_body")" || true

# A diff whose context does not match — applyUnifiedDiff must return the
# non-applicable no-change domain outcome.
BAD_CHECKED_PATCH='--- a/README.md
+++ b/README.md
@@ -1,1 +1,1 @@
-NONEXISTENT_CONTEXT_LINE_FOR_CHECKED
+replacement
'
bcp_body="$(python3 -c 'import json,sys; print(json.dumps({"project": sys.argv[1], "diff": sys.argv[2]}))' "$RUNTIME_PROJECT_ID" "$BAD_CHECKED_PATCH")"
body="$(api_post /api/projects/apply_unified_diff "$bcp_body")"
bcp_success="$(json_get "$body" success)"
bcp_applied="$(json_get "$body" output.applied)"
bcp_can_apply="$(json_get "$body" output.can_apply)"
if [ "$bcp_success" = "True" ] && [ "$bcp_applied" = "False" ] && [ "$bcp_can_apply" = "False" ]; then
    pass "applyUnifiedDiff(non-applicable) does not apply"
else
    fail "applyUnifiedDiff(non-applicable) should not apply (success=$bcp_success applied=$bcp_applied can_apply=$bcp_can_apply body=${body:0:300})"
fi
# Worktree must be unchanged after the non-applicable probe.
post_harden_status="$(api_post /api/projects/git_status "{\"project\":\"$RUNTIME_PROJECT_ID\"}")"
post_harden_porcelain="$(json_get "$post_harden_status" output.stdout)"
if [ "$pre_harden_porcelain" = "$post_harden_porcelain" ]; then
    pass "non-applicable diff leaves worktree unchanged"
else
    fail "non-applicable diff mutated the worktree (pre=${pre_harden_porcelain:0:120} post=${post_harden_porcelain:0:120})"
fi

# ----------------------------------------------------------------------------
# 7h. Full-auto coding loop smoke (dedicated actions plus callRuntimeTool)
# ----------------------------------------------------------------------------
#
# Simulates a GPT Actions auto coding loop using dedicated project/Git actions plus
# callRuntimeTool for canonical read/search/edit tools. Proves
# a custom GPT can complete a small edit → verify → cleanup cycle through the
# recommended flow:
#
#   1. listProjects              — find the agent project
#   2. callRuntimeTool(read_files) — read a tracked file (README.md)
#   3. callRuntimeTool(search_project_texts) — locate the target substring
#   4. callRuntimeTool(show_changes) — confirm initial clean state
#   5. callRuntimeTool           — run apply_text_edits for a reversible text edit
#   6. callRuntimeTool(show_changes) — confirm the diff is visible
#   7. runProjectShellCommand    — lightweight check (grep)
#   8. gitRestorePaths           — restore the modified tracked file
#   9. callRuntimeTool(show_changes) — confirm worktree is clean again
#
# Then an optional unified-diff sub-loop:
#  10. applyUnifiedDiff           — preflight + apply one small raw unified diff
#  11. callRuntimeTool(show_changes) — confirm diff visible
#  12. callRuntimeTool(delete_project_files) — cleanup the probe file
#  13. callRuntimeTool(show_changes) — confirm clean again

log "---- full-auto coding loop smoke (dedicated actions plus callRuntimeTool) ----"

LOOP_MARKER_OLD="Smoke Project"
LOOP_MARKER_NEW="Smoke Project [auto-loop]"

# Step 1: listProjects — find the agent project (re-check as part of the loop).
body="$(api_post /api/projects/list '{}')"
loop_list_json="$(json_get "$body" output)"
if echo "$loop_list_json" | grep -q "\"$RUNTIME_PROJECT_ID\""; then
    pass "loop: listProjects found $RUNTIME_PROJECT_ID"
else
    fail "loop: listProjects did not find $RUNTIME_PROJECT_ID (got: ${loop_list_json:0:200})"
fi

# Step 2: read_files — read README.md.
body="$(api_post /api/tools/call "{\"tool\":\"read_files\",\"params\":{\"project\":\"$RUNTIME_PROJECT_ID\",\"items\":[{\"path\":\"README.md\"}]}}")"
loop_readme="$(json_get "$body" output.items.0.output.text)"
loop_readme_sha="$(json_get "$body" output.items.0.output.sha256)"
if echo "$loop_readme" | grep -q "$LOOP_MARKER_OLD"; then
    pass "loop: read_files sees README.md with target marker"
else
    fail "loop: read_files did not find marker in README.md (got: ${loop_readme:0:120})"
fi
if [ -n "$loop_readme_sha" ] && [ "$loop_readme_sha" != "None" ] && [ ${#loop_readme_sha} -eq 64 ]; then
    pass "loop: read_files returns README.md sha256 guard"
else
    fail "loop: read_files did not return a valid README.md sha256 (got: $loop_readme_sha)"
fi

# Step 3: search_project_texts — locate the target substring through the canonical
# one-query batch path. The sparse match record is the stable assertion surface.
body="$(api_post /api/tools/call "{\"tool\":\"search_project_texts\",\"params\":{\"project\":\"$RUNTIME_PROJECT_ID\",\"queries\":[{\"pattern\":\"$LOOP_MARKER_OLD\",\"path\":\"README.md\",\"limit\":10}]}}")"
if [ "$(json_get "$body" output.items.0.output.matches.0.path)" = "README.md" ]; then
    pass "loop: search_project_texts located target marker in README.md"
else
    fail "loop: search_project_texts did not locate target marker (got: ${body:0:200})"
fi

# Step 4: show_changes — confirm initial clean state.
body="$(show_changes_call)"
loop_pre_clean="$(json_get "$body" output.clean)"
if [ "$loop_pre_clean" = "True" ]; then
    pass "loop: show_changes confirms clean initial state"
else
    fail "loop: worktree not clean before loop (clean=$loop_pre_clean got: ${body:0:200})"
fi

# Step 5: callRuntimeTool(apply_text_edits) — small reversible edit on
# README.md, guarded by the sha256 returned by Step 2 for this fixture.
loop_replace_body="$(python3 -c '
import json, sys
print(json.dumps({
    "tool": "apply_text_edits",
    "params": {
        "project": sys.argv[1],
        "changes": [{
            "kind": "edit",
            "path": "README.md",
            "expected_sha256": sys.argv[2],
            "edits": [{
                "kind": "replace_exact",
                "old_text": sys.argv[3],
                "new_text": sys.argv[4]
            }]
        }]
    }
}))
' "$RUNTIME_PROJECT_ID" "$loop_readme_sha" "$LOOP_MARKER_OLD" "$LOOP_MARKER_NEW")"
body="$(api_post /api/tools/call "$loop_replace_body")"
if [ "$(json_get "$body" success)" = "True" ] && [ "$(json_get "$body" output.changed)" = "True" ]; then
    pass "loop: callRuntimeTool(apply_text_edits) edited README.md"
else
    fail "loop: callRuntimeTool(apply_text_edits) did not edit README.md (body: ${body:0:300})"
fi

# Step 6: show_changes — confirm the diff is now visible.
body="$(show_changes_call)"
loop_post_files="$(json_get "$body" output.files)"
if echo "$loop_post_files" | grep -q "README.md"; then
    pass "loop: show_changes shows README.md modified"
else
    fail "loop: show_changes did not show README.md modified (files=${loop_post_files:0:200})"
fi

# Step 7: runProjectShellCommand — lightweight check (grep for the edited marker).
body="$(api_post /api/projects/run_shell "{\"project\":\"$RUNTIME_PROJECT_ID\",\"command\":\"grep -c 'auto-loop' README.md\"}")"
loop_shell_stdout="$(json_get "$body" output.stdout_tail)"
if [ "$(json_get "$body" success)" = "True" ] && echo "$loop_shell_stdout" | grep -qE '^[0-9]+'; then
    pass "loop: runProjectShellCommand confirms edit via grep (matches=$loop_shell_stdout)"
else
    fail "loop: runProjectShellCommand grep check failed (stdout=$loop_shell_stdout body=${body:0:200})"
fi

# Step 8: gitRestorePaths — restore README.md to its committed state.
loop_restore_body="$(build_body "{\"project\":\"$RUNTIME_PROJECT_ID\",\"paths\":[\"README.md\"]}")"
body="$(api_post /api/projects/git_restore_paths "$loop_restore_body")"
if [ "$(json_get "$body" success)" = "True" ]; then
    pass "loop: gitRestorePaths restored README.md"
else
    fail "loop: gitRestorePaths failed (body: ${body:0:300})"
fi

# Step 9: show_changes — confirm worktree is clean again.
body="$(show_changes_call)"
loop_final_clean="$(json_get "$body" output.clean)"
if [ "$loop_final_clean" = "True" ]; then
    pass "loop: show_changes confirms worktree clean after restore"
else
    fail "loop: worktree not clean after restore (clean=$loop_final_clean got: ${body:0:200})"
fi
# Double-check via git_status that README.md is back to its committed content.
body="$(api_post /api/tools/call "{\"tool\":\"read_files\",\"params\":{\"project\":\"$RUNTIME_PROJECT_ID\",\"items\":[{\"path\":\"README.md\"}]}}")"
if echo "$(json_get "$body" output.items.0.output.text)" | grep -q "$LOOP_MARKER_OLD"; then
    pass "loop: README.md content restored to original marker"
else
    fail "loop: README.md content not restored (got: ${body:0:200})"
fi

# --- Optional unified-diff sub-loop: apply → diff → cleanup ---

# Step 10: applyUnifiedDiff — the tool owns its preflight and applies once.
LOOP_PATCH='diff --git a/LOOP_PATCH_PROBE.md b/LOOP_PATCH_PROBE.md
new file mode 100644
--- /dev/null
+++ b/LOOP_PATCH_PROBE.md
@@ -0,0 +1 @@
+loop-patch-probe
'
loop_vp_body="$(python3 -c 'import json,sys; print(json.dumps({"project": sys.argv[1], "diff": sys.argv[2]}))' "$RUNTIME_PROJECT_ID" "$LOOP_PATCH")"
body="$(api_post /api/projects/apply_unified_diff "$loop_vp_body")"
if [ "$(json_get "$body" success)" = "True" ] && [ "$(json_get "$body" output.applied)" = "True" ]; then
    pass "loop: applyUnifiedDiff applied probe diff"
else
    fail "loop: applyUnifiedDiff did not apply probe diff (body: ${body:0:300})"
fi

# Step 11: show_changes — confirm the probe file is visible.
body="$(show_changes_call)"
if echo "$(json_get "$body" output.files)" | grep -q "LOOP_PATCH_PROBE.md"; then
    pass "loop: show_changes shows probe file after apply"
else
    fail "loop: probe file not visible after apply (got: ${body:0:200})"
fi

# Step 12: callRuntimeTool(delete_project_files) — cleanup the probe file.
loop_del_body="$(build_body "{\"tool\":\"delete_project_files\",\"params\":{\"project\":\"$RUNTIME_PROJECT_ID\",\"paths\":[\"LOOP_PATCH_PROBE.md\"]}}")"
body="$(api_post /api/tools/call "$loop_del_body")"
if [ "$(json_get "$body" success)" = "True" ]; then
    pass "loop: delete_project_files removed probe file"
else
    fail "loop: delete_project_files did not remove probe file (body: ${body:0:300})"
fi

# Step 13: show_changes — confirm clean again.
body="$(show_changes_call)"
loop_patch_final_clean="$(json_get "$body" output.clean)"
if [ "$loop_patch_final_clean" = "True" ]; then
    pass "loop: show_changes confirms clean after patch cleanup"
else
    fail "loop: worktree not clean after patch cleanup (clean=$loop_patch_final_clean)"
fi

# ----------------------------------------------------------------------------
# 7i. Runtime write tool + dedicated startProjectShellJob smoke (probe files only)
# ----------------------------------------------------------------------------
#
# Proves runtime-only write_project_file through callRuntimeTool and the
# dedicated async job actions work end-to-end:
#
#   1. callRuntimeTool(write_project_file) — create WRITE_ACTION_PROBE.txt
#   2. callRuntimeTool(read_files)        — confirm content
#   3. callRuntimeTool(write_project_file) — overwrite with an expected_sha256 guard
#   4. callRuntimeTool(read_files) — confirm overwritten content
#   5. callRuntimeTool(delete_project_files) — cleanup the probe file
#   6. startProjectShellJob — start `printf job-ok` asynchronously
#   7. callRuntimeTool(observe_jobs) — poll until completed
#   8. getRuntimeJobTail   — confirm the output contains job-ok

log "---- runtime write_project_file + dedicated startProjectShellJob smoke ----"

# Step 1: callRuntimeTool(write_project_file) — create WRITE_ACTION_PROBE.txt.
waf_create_body="$(python3 -c '
import json, sys
print(json.dumps({
    "tool": "write_project_file",
    "params": {
        "project": sys.argv[1],
        "path": "WRITE_ACTION_PROBE.txt",
        "content": "write-action-probe-v1\n"
    }
}))
' "$RUNTIME_PROJECT_ID")"
body="$(api_post /api/tools/call "$waf_create_body")"
if [ "$(json_get "$body" success)" = "True" ] && [ "$(json_get "$body" output.created)" = "True" ]; then
    pass "callRuntimeTool(write_project_file) creates WRITE_ACTION_PROBE.txt"
else
    fail "callRuntimeTool(write_project_file) did not create probe (body: ${body:0:300})"
fi
waf_sha="$(json_get "$body" output.sha256)"
if [ -n "$waf_sha" ] && [ "$waf_sha" != "None" ] && [ ${#waf_sha} -eq 64 ]; then
    pass "write_project_file returns 64-char sha256 for new file"
else
    fail "write_project_file missing sha256 (got: $waf_sha)"
fi

# Step 2: read_files — confirm content.
body="$(api_post /api/tools/call "{\"tool\":\"read_files\",\"params\":{\"project\":\"$RUNTIME_PROJECT_ID\",\"items\":[{\"path\":\"WRITE_ACTION_PROBE.txt\"}]}}")"
if echo "$(json_get "$body" output.items.0.output.text)" | grep -q "write-action-probe-v1"; then
    pass "read_files confirms WRITE_ACTION_PROBE.txt content"
else
    fail "read_files did not confirm probe content (got: ${body:0:200})"
fi

# Step 3: callRuntimeTool(write_project_file) — overwrite with an expected_sha256
# guard. Use the sha256 returned by the create step so the guard matches exactly.
waf_overwrite_body="$(python3 -c '
import json, sys
print(json.dumps({
    "tool": "write_project_file",
    "params": {
        "project": sys.argv[1],
        "path": "WRITE_ACTION_PROBE.txt",
        "content": "write-action-probe-v2\n",
        "overwrite": True,
        "expected_sha256": sys.argv[2]
    }
}))
' "$RUNTIME_PROJECT_ID" "$waf_sha")"
body="$(api_post /api/tools/call "$waf_overwrite_body")"
if [ "$(json_get "$body" success)" = "True" ]; then
    pass "callRuntimeTool(write_project_file) overwrites with matching expected_sha256 guard"
else
    fail "callRuntimeTool(write_project_file) overwrite with guard failed (body: ${body:0:300})"
fi

# Step 4: read_files — confirm overwritten content.
body="$(api_post /api/tools/call "{\"tool\":\"read_files\",\"params\":{\"project\":\"$RUNTIME_PROJECT_ID\",\"items\":[{\"path\":\"WRITE_ACTION_PROBE.txt\"}]}}")"
if echo "$(json_get "$body" output.items.0.output.text)" | grep -q "write-action-probe-v2"; then
    pass "read_files confirms overwritten content"
else
    fail "read_files did not confirm overwritten content (got: ${body:0:200})"
fi

# Step 5: callRuntimeTool(delete_project_files) — cleanup the probe.
waf_del_body="$(build_body "{\"tool\":\"delete_project_files\",\"params\":{\"project\":\"$RUNTIME_PROJECT_ID\",\"paths\":[\"WRITE_ACTION_PROBE.txt\"]}}")"
body="$(api_post /api/tools/call "$waf_del_body")"
if [ "$(json_get "$body" success)" = "True" ]; then
    pass "deleteProjectFiles removes WRITE_ACTION_PROBE.txt"
else
    fail "deleteProjectFiles did not remove probe (body: ${body:0:300})"
fi

# Step 6: startProjectShellJob — start a lightweight async command.
sjr_body="$(python3 -c '
import json, sys
print(json.dumps({
    "project": sys.argv[1],
    "command": "printf job-ok"
}))
' "$RUNTIME_PROJECT_ID")"
body="$(api_post /api/projects/run_job "$sjr_body")"
sjr_success="$(json_get "$body" success)"
SJ_JOB_ID="$(json_get "$body" output.job_id)"
if [ "$sjr_success" = "True" ] && [ -n "$SJ_JOB_ID" ] && [ "$SJ_JOB_ID" != "None" ]; then
    pass "startProjectShellJob started async job (job_id=$SJ_JOB_ID)"
else
    fail "startProjectShellJob did not start a job (success=$sjr_success body=${body:0:300})"
fi

# Step 7: observe_jobs — poll until completed.
sj_done=0
sj_poll_tries=0
sj_status=""
while [ "$sj_poll_tries" -lt 20 ]; do
    check_deadline
    body="$(observe_one_job_call "$SJ_JOB_ID" 1)"
    sj_status="$(json_get "$body" output.items.0.output.status)"
    case "$sj_status" in
        completed|failed|stopped|lost)
            sj_done=1
            break
            ;;
    esac
    sj_poll_tries=$((sj_poll_tries + 1))
    sleep 1
done
if [ "$sj_done" = "1" ] && [ "$sj_status" = "completed" ]; then
    pass "observe_jobs confirms async job completed"
else
    fail "observe_jobs did not confirm completion (status=$sj_status tries=$sj_poll_tries body=${body:0:200})"
fi

# Step 8: getRuntimeJobTail — confirm the output contains job-ok.
body="$(api_post /api/jobs/tail "{\"job_id\":\"$SJ_JOB_ID\",\"tail_lines\":50}")"
sj_tail="$(json_get "$body" output.stdout_tail)"
if echo "$sj_tail" | grep -q "job-ok"; then
    pass "getRuntimeJobTail confirms async job output (job-ok)"
else
    fail "getRuntimeJobTail did not show job-ok (stdout=$sj_tail body=${body:0:200})"
fi

# Confirm the worktree is clean after the dedicated action smoke (the job ran
# `printf` which does not touch the repo).
body="$(show_changes_call)"
ded_final_clean="$(json_get "$body" output.clean)"
if [ "$ded_final_clean" = "True" ]; then
    pass "dedicated action smoke leaves worktree clean"
else
    fail "dedicated action smoke left worktree dirty (clean=$ded_final_clean)"
fi

# ----------------------------------------------------------------------------
# 8. Summary
# ----------------------------------------------------------------------------

log "---- summary ----"
log "passed: $PASS"
log "failed: $FAIL"
log "elapsed: $(elapsed)s / ${TIMEOUT_SECS}s"

if [ "$FAIL" -ne 0 ]; then
    print_logs_hint
    exit 1
fi

log "E2E smoke PASSED"
exit 0
