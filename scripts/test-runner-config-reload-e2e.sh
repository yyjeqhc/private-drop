#!/usr/bin/env bash
set -euo pipefail

if [ "${WEBCODEX_E2E_AGENT_RELOAD:-0}" != "1" ]; then
    printf '[agent-reload-e2e] skipped (set WEBCODEX_E2E_AGENT_RELOAD=1)\n'
    exit 0
fi
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CARGO_BIN="${CARGO_BIN:-cargo}"
CLIENT_ID="agent-reload-e2e"
PROJECT_ID="fixture"
RUNTIME_PROJECT="agent:${CLIENT_ID}:${PROJECT_ID}"
TOKEN="webcodex-runner-reload-e2e-only"
STARTUP_DISPLAY="Agent Reload E2E"
TMP_ROOT=""
SERVER_PID=""
RUNNER_PID=""
PORT=""
STATUS_FILE=""
STAGE="startup"
PASS_COUNT=0

ok() {
    PASS_COUNT=$((PASS_COUNT + 1))
    printf '[agent-reload-e2e][ok] %s\n' "$1"
}

fail() {
    printf '[agent-reload-e2e][FAIL] %s\n' "$1" >&2
    exit 1
}
require_command() {
    command -v "$1" >/dev/null 2>&1 || fail "required command unavailable: $1"
}
process_alive() {
    [ -n "$1" ] && kill -0 "$1" 2>/dev/null
}
stop_group() {
    local pid="$1"
    [ -n "$pid" ] || return 0
    if kill -0 -- "-$pid" 2>/dev/null; then
        kill -TERM -- "-$pid" 2>/dev/null || true
        for _ in $(seq 1 50); do
            kill -0 -- "-$pid" 2>/dev/null || break
            sleep 0.1
        done
        kill -KILL -- "-$pid" 2>/dev/null || true
    fi
    wait "$pid" 2>/dev/null || true
}
diagnostics() {
    printf '[agent-reload-e2e][diagnostic] stage=%s agent_alive=%s server_alive=%s\n' \
        "$STAGE" "$(process_alive "${RUNNER_PID:-}" && printf yes || printf no)" \
        "$(process_alive "${SERVER_PID:-}" && printf yes || printf no)" >&2
    if [ -n "${STATUS_FILE:-}" ] && [ -s "$STATUS_FILE" ]; then
        python3 - "$STATUS_FILE" <<'PY' >&2 || true
import json, sys
try:
    d = json.load(open(sys.argv[1], encoding="utf-8"))
    c = next(iter(d.get("output", {}).get("agents", {}).get("clients", [])), {})
    safe = {k: c.get(k) for k in ("client_id", "connected", "display_name", "transport")}
    safe["tool_providers"] = c.get("tool_providers")
    print("[agent-reload-e2e][diagnostic] last_status=" + json.dumps(safe, sort_keys=True))
except Exception:
    print("[agent-reload-e2e][diagnostic] last_status=unavailable")
PY
    fi
    for log in "${SERVER_LOG:-}" "${RUNNER_LOG:-}"; do
        [ -n "$log" ] && [ -f "$log" ] || continue
        printf '[agent-reload-e2e][diagnostic] bounded_log_tail:\n' >&2
        tail -n 40 "$log" | awk -v token="$TOKEN" -v tmp="${TMP_ROOT:-}" \
            '{gsub(token,"<redacted>"); if (tmp != "") gsub(tmp,"<tmp>"); print substr($0,1,500)}' >&2
    done
}
cleanup() {
    local status=$?
    trap - EXIT INT TERM
    [ "$status" -eq 0 ] || diagnostics
    stop_group "${RUNNER_PID:-}"
    stop_group "${SERVER_PID:-}"
    [ -z "${PORT:-}" ] || wait_for_port closed || true
    [ -z "${TMP_ROOT:-}" ] || rm -rf "$TMP_ROOT"
    exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT TERM
find_port() {
    python3 - <<'PY'
import socket
with socket.socket() as sock:
    sock.bind(("127.0.0.1", 0))
    print(sock.getsockname()[1])
PY
}
wait_for_port() {
    local expected="$1"
    for _ in $(seq 1 300); do
        if (echo >/dev/tcp/127.0.0.1/"$PORT") 2>/dev/null; then
            [ "$expected" = open ] && return 0
        else
            [ "$expected" = closed ] && return 0
        fi
        [ "$expected" = closed ] || process_alive "$SERVER_PID" || return 1
        sleep 0.1
    done
    return 1
}
api_post() {
    curl -sS --noproxy '*' --max-time 12 -H "Authorization: Bearer ${TOKEN}" \
        -H 'Content-Type: application/json' -X POST \
        "http://127.0.0.1:${PORT}$1" -d "$2"
}
write_runner_config() {
    local marker="$1" max_timeout="$2" max_output="$3" strategy="$4"
    local enabled="$5" provider_timeout="$6" display="$7" max_jobs="$8"
    local search_mapping="$9"
    cat >"$RUNNER_CONFIG.next" <<EOF
server_url = "http://127.0.0.1:${PORT}"
token = "${TOKEN}"
client_id = "${CLIENT_ID}"
display_name = "${display}"
project_registry_dir = "${PROJECTS_DIR}"
poll_interval_ms = 100
max_concurrent_jobs = ${max_jobs}
transport = "polling"
[policy]
allow_raw_shell = true
allow_cwd_anywhere = false
allowed_roots = ["${FIXTURE}"]
max_timeout_secs = ${max_timeout}
max_output_bytes = ${max_output}
[shell]
default_profile = "reload-test"
[shell.profiles.reload-test]
program = "sh"
args = ["-lc"]
env = { WEBCODEX_RELOAD_MARKER = "${marker}" }
[tool_providers]
strategy = "${strategy}"
[tool_providers.claude_code]
enabled = ${enabled}
command = "claude-does-not-need-to-exist-for-lazy-status"
args = ["mcp", "serve"]
timeout_secs = ${provider_timeout}
[tool_providers.claude_code.mapping]
search_project_text = "${search_mapping}"
EOF
    mv "$RUNNER_CONFIG.next" "$RUNNER_CONFIG"
}
status_matches() {
    local generation="$1" result="$2" error="$3" restart="$4"
    local fields="$5" strategy="$6" enabled="$7"
    api_post /api/runtime/status '{}' >"$STATUS_FILE" 2>/dev/null || return 1
    python3 - "$STATUS_FILE" "$CLIENT_ID" "$STARTUP_DISPLAY" "$generation" \
        "$result" "$error" "$restart" "$fields" "$strategy" "$enabled" <<'PY' >/dev/null 2>&1
import json, sys
d = json.load(open(sys.argv[1], encoding="utf-8"))
c = next(x for x in d["output"]["agents"]["clients"] if x["client_id"] == sys.argv[2])
assert c["connected"] and c["display_name"] == sys.argv[3] and c["projects_count"] == 1
p = c["tool_providers"]
r = p["config_reload"]
expected_error = None if sys.argv[6] == "null" else sys.argv[6]
expected_fields = [] if sys.argv[8] == "-" else sys.argv[8].split(",")
assert r["generation"] == int(sys.argv[4]) and r["last_reload_result"] == sys.argv[5]
assert r.get("last_reload_error_code") == expected_error
assert r["restart_required"] is (sys.argv[7] == "true")
assert r["restart_required_fields"] == expected_fields == sorted(set(expected_fields))
keys = {"generation", "last_reload_result", "restart_required", "restart_required_fields"}
if expected_error is not None: keys.add("last_reload_error_code")
assert set(r) == keys
assert p["strategy"] == sys.argv[9]
cc = p["claude_code"]
assert cc["enabled"] is (sys.argv[10] == "true")
assert not cc["available"] and cc["process_state"] == "not_started"
assert cc["discovered_tool_names"] == [] and cc.get("last_call") is None
PY
}
wait_for_status() {
    for _ in $(seq 1 200); do
        process_alive "$RUNNER_PID" && process_alive "$SERVER_PID" || return 1
        status_matches "$@" && return 0
        sleep 0.1
    done
    return 1
}
assert_runner_pid() {
    [ "$RUNNER_PID" = "$START_RUNNER_PID" ] && process_alive "$RUNNER_PID" \
        || fail "agent PID changed or exited during reload"
}
request_body() {
    python3 -c 'import json,sys; print(json.dumps({"project":sys.argv[1],"command":sys.argv[2],"timeout_secs":5}))' \
        "$RUNTIME_PROJECT" "$1"
}
run_shell_request() {
    api_post /api/projects/run_shell "$(request_body "$1")" >"$RESPONSE_FILE"
}
assert_marker() {
    run_shell_request 'printf %s "$WEBCODEX_RELOAD_MARKER"'
    python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); assert d["success"] and d["output"]["stdout_tail"]==sys.argv[2] and d["output"]["stderr_tail"]=="" and d["output"]["exit_code"]==0' \
        "$RESPONSE_FILE" "$1" || fail "shell marker was not $1"
}
start_job() {
    api_post /api/projects/run_job "$(request_body "$1")" | python3 -c \
        'import json,sys; d=json.load(sys.stdin); assert d["success"]; print(d["output"]["job_id"])'
}
job_status() {
    api_post /api/tools/call "{\"tool\":\"observe_jobs\",\"items\":[{\"job_id\":\"$1\"}],\"tail_lines\":1}" | python3 -c \
        'import json,sys; d=json.load(sys.stdin); assert d["success"]; item=d["output"]["items"][0]; assert item["success"]; print(item["output"]["status"])'
}
for command in awk curl git mv python3 setsid tail "$CARGO_BIN"; do require_command "$command"; done
cd "$ROOT"
REPO_STATUS_BEFORE="$(git status --short)"
if [ "${WEBCODEX_E2E_SKIP_BUILD:-0}" != "1" ]; then
    "$CARGO_BIN" build --quiet -p webcodex -p webcodex-runner --bins
fi
[ -x target/debug/webcodex-server ] && [ -x target/debug/webcodex-runner ] \
    || fail "debug server/agent binaries are unavailable"
TMP_ROOT="$(mktemp -d -t webcodex-runner-reload-e2e-XXXXXX)"
DATA_DIR="$TMP_ROOT/data"
PROJECTS_DIR="$TMP_ROOT/project-registry"
FIXTURE="$TMP_ROOT/fixture"
ISOLATED_HOME="$TMP_ROOT/home"
RUNTIME_TMP="$TMP_ROOT/tmp"
RUNNER_CONFIG="$TMP_ROOT/runner.toml"
GENERATION_TWO_CONFIG="$TMP_ROOT/generation-2.toml"
SERVER_LOG="$TMP_ROOT/server.log"
RUNNER_LOG="$TMP_ROOT/agent.log"
STATUS_FILE="$TMP_ROOT/status.json"
RESPONSE_FILE="$TMP_ROOT/response.json"
GATE="$TMP_ROOT/concurrency-gate"
mkdir -p "$DATA_DIR" "$PROJECTS_DIR" "$FIXTURE" "$RUNTIME_TMP" \
    "$ISOLATED_HOME/.config" "$ISOLATED_HOME/.local/share" \
    "$ISOLATED_HOME/.local/state" "$ISOLATED_HOME/.cache"
: >"$TMP_ROOT/empty.env"
git -C "$FIXTURE" init -b main >/dev/null
git -C "$FIXTURE" config user.email e2e@example.invalid
git -C "$FIXTURE" config user.name 'WebCodex E2E'
printf 'reload fixture\n' >"$FIXTURE/fixture.txt"
git -C "$FIXTURE" add fixture.txt
git -C "$FIXTURE" commit -m fixture >/dev/null
cat >"$PROJECTS_DIR/${PROJECT_ID}.toml" <<EOF
id = "${PROJECT_ID}"
path = "${FIXTURE}"
name = "Agent Reload Fixture"
allow_patch = true
kind = "text"
shell_profile = "reload-test"
EOF
PORT="$(find_port)"
write_runner_config generation-1 5 65536 native false 30 "$STARTUP_DISPLAY" 1 \
    project_search_generation_1
setsid env -i PATH="$PATH" LANG=C HOME="$ISOLATED_HOME" \
    XDG_CONFIG_HOME="$ISOLATED_HOME/.config" XDG_DATA_HOME="$ISOLATED_HOME/.local/share" \
    XDG_STATE_HOME="$ISOLATED_HOME/.local/state" XDG_CACHE_HOME="$ISOLATED_HOME/.cache" \
    TMPDIR="$RUNTIME_TMP" WEBCODEX_ENV_FILE="$TMP_ROOT/empty.env" \
    WEBCODEX_ADDR="127.0.0.1:${PORT}" WEBCODEX_DATA="$DATA_DIR" WEBCODEX_TOKEN="$TOKEN" \
    RUST_LOG=warn target/debug/webcodex-server >"$SERVER_LOG" 2>&1 &
SERVER_PID=$!
wait_for_port open || fail "isolated server port did not open"
setsid env -i PATH="$PATH" LANG=C HOME="$ISOLATED_HOME" \
    XDG_CONFIG_HOME="$ISOLATED_HOME/.config" XDG_DATA_HOME="$ISOLATED_HOME/.local/share" \
    XDG_STATE_HOME="$ISOLATED_HOME/.local/state" XDG_CACHE_HOME="$ISOLATED_HOME/.cache" \
    TMPDIR="$RUNTIME_TMP" WEBCODEX_ENV_FILE="$TMP_ROOT/empty.env" RUST_LOG=warn \
    target/debug/webcodex-runner --config "$RUNNER_CONFIG" >"$RUNNER_LOG" 2>&1 &
RUNNER_PID=$!
START_RUNNER_PID="$RUNNER_PID"
STAGE="generation 1 baseline"
wait_for_status 1 not_attempted null false - native false || fail "baseline status did not arrive"
assert_marker generation-1
assert_runner_pid
ok "generation 1 registered and dispatched marker generation-1"
STAGE="generation 2 valid hot-only reload"
write_runner_config generation-2 2 32768 claude_code_then_native true 17 "$STARTUP_DISPLAY" 1 \
    project_search_generation_2
cp "$RUNNER_CONFIG" "$GENERATION_TWO_CONFIG"
kill -HUP "$RUNNER_PID"
wait_for_status 2 success null false - claude_code_then_native true \
    || fail "valid reload status did not arrive"
assert_runner_pid
assert_marker generation-2
run_shell_request 'sleep 3; printf unexpected'
python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); out=d["output"]; assert not d["success"] and out["failure_kind"]=="timeout" and "unexpected" not in out.get("stdout_tail", "")' \
    "$RESPONSE_FILE" || fail "generation 2 timeout policy was not enforced"
status_matches 2 success null false - claude_code_then_native true \
    || fail "provider status changed or Claude started after passive checks"
ok "generation 2 applied shell, timeout policy, and lazy provider status"
STAGE="invalid TOML fail-closed"
printf 'display_name = "unterminated\n' >"$RUNNER_CONFIG.next"
mv "$RUNNER_CONFIG.next" "$RUNNER_CONFIG"
kill -HUP "$RUNNER_PID"
wait_for_status 2 failure config_parse_failed false - claude_code_then_native true \
    || fail "invalid reload failure status did not arrive"
assert_runner_pid
assert_marker generation-2
cp "$GENERATION_TWO_CONFIG" "$RUNNER_CONFIG.next"
mv "$RUNNER_CONFIG.next" "$RUNNER_CONFIG"
ok "invalid TOML kept generation 2 and its active request snapshot"

STAGE="generation 3 mixed reload"
write_runner_config generation-3 5 24576 native false 11 'Restart-Only Display' 2 \
    project_search_generation_3
kill -HUP "$RUNNER_PID"
wait_for_status 3 partial null true display_name,max_concurrent_jobs native false \
    || fail "mixed reload status did not arrive"
assert_runner_pid
command='sleep 3; printf %s "$WEBCODEX_RELOAD_MARKER"'
run_shell_request "$command"
python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); assert d["success"] and d["output"]["stdout_tail"]=="generation-3"' \
    "$RESPONSE_FILE" || fail "generation 3 hot policy/shell snapshot was not active"
JOB_ONE="$(start_job "while [ ! -e '$GATE' ]; do sleep 0.05; done; printf first")"
JOB_TWO="$(start_job 'printf %s "$WEBCODEX_RELOAD_MARKER"')"
queued_seen=false
for _ in $(seq 1 50); do
    one="$(job_status "$JOB_ONE" 2>/dev/null || true)"
    two="$(job_status "$JOB_TWO" 2>/dev/null || true)"
    if [ "$one" = running ] && [ "$two" = agent_queued ]; then queued_seen=true; break; fi
    [ "$two" != completed ] || break
    sleep 0.1
done
[ "$queued_seen" = true ] || fail "startup max_concurrent_jobs=1 was not retained"
touch "$GATE"
for _ in $(seq 1 80); do
    [ "$(job_status "$JOB_ONE" 2>/dev/null || true)" = completed ] && \
        [ "$(job_status "$JOB_TWO" 2>/dev/null || true)" = completed ] && break
    sleep 0.1
done
[ "$(job_status "$JOB_ONE")" = completed ] && [ "$(job_status "$JOB_TWO")" = completed ] \
    || fail "mixed-reload concurrency probe jobs did not complete"
ok "generation 3 applied hot fields and retained startup identity/concurrency"

STAGE="generation 4 recovery"
write_runner_config generation-4 3 16384 claude_code_then_native true 13 "$STARTUP_DISPLAY" 1 \
    project_search_generation_4
kill -HUP "$RUNNER_PID"
wait_for_status 4 success null false - claude_code_then_native true \
    || fail "recovery reload status did not arrive"
assert_runner_pid
assert_marker generation-4
ok "generation 4 cleared restart-required summary"

STAGE="shutdown and cleanup"
[ -z "$(git -C "$FIXTURE" status --porcelain)" ] || fail "fixture worktree is dirty"
status_matches 4 success null false - claude_code_then_native true \
    || fail "final status changed or Claude was started"
stop_group "$RUNNER_PID"
kill -0 -- "-$RUNNER_PID" 2>/dev/null && fail "agent process group remained" || true
RUNNER_PID=""
stop_group "$SERVER_PID"
kill -0 -- "-$SERVER_PID" 2>/dev/null && fail "server process group remained" || true
SERVER_PID=""
wait_for_port closed || fail "server port remained open"
[ "$(git status --short)" = "$REPO_STATUS_BEFORE" ] || fail "repository gained test residue"
REMOVED_ROOT="$TMP_ROOT"
rm -rf "$REMOVED_ROOT"
TMP_ROOT=""
[ ! -e "$REMOVED_ROOT" ] || fail "temporary fixture remained"
ok "agent/server groups, port, fixture, repository, and temporary root are clean"

printf '[agent-reload-e2e] passed checks=%s generations=1,2,2,3,4 pid_stable=yes claude_started=no\n' \
    "$PASS_COUNT"
