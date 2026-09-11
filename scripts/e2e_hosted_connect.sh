#!/usr/bin/env bash
set -euo pipefail

# Real-process hosted-connect smoke. Everything is loopback-only and all
# configuration/state lives below one temporary directory removed by the trap.

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$REPO_DIR"

TIMEOUT_SECS="${E2E_TIMEOUT_SECS:-180}"
CARGO_BIN="${CARGO_BIN:-cargo}"
TMP_ROOT=""
SERVER_PID=""
PROFILE=""
STARTED_AT="$(date +%s)"

log() {
    printf '[hosted-connect-e2e] %s\n' "$*"
}

die() {
    printf '[hosted-connect-e2e][FAIL] %s\n' "$*" >&2
    exit 1
}

cleanup() {
    trap - EXIT INT TERM
    if [ -n "$TMP_ROOT" ] && [ -n "$PROFILE" ] && [ -x "$REPO_DIR/target/debug/webcodex" ]; then
        HOME="$TMP_ROOT/home" \
        XDG_CONFIG_HOME="$TMP_ROOT/config" \
        XDG_STATE_HOME="$TMP_ROOT/state" \
        "$REPO_DIR/target/debug/webcodex" runner stop --profile "$PROFILE" >/dev/null 2>&1 || true
    fi
    if [ -n "$SERVER_PID" ] && kill -0 "$SERVER_PID" 2>/dev/null; then
        kill "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
    fi
    if [ -n "$TMP_ROOT" ]; then
        case "$TMP_ROOT" in
            /tmp/webcodex-hosted-connect-e2e.*)
                rm -rf -- "$TMP_ROOT"
                ;;
            *)
                printf '[hosted-connect-e2e][WARN] refusing unexpected cleanup path: %s\n' "$TMP_ROOT" >&2
                ;;
        esac
    fi
}
trap cleanup EXIT INT TERM

deadline_ok() {
    [ "$(( $(date +%s) - STARTED_AT ))" -lt "$TIMEOUT_SECS" ]
}

process_active() {
    local pid="$1"
    local state
    kill -0 "$pid" 2>/dev/null || return 1
    state="$(ps -p "$pid" -o stat= 2>/dev/null | tr -d '[:space:]')"
    case "$state" in
        ""|Z*) return 1 ;;
        *) return 0 ;;
    esac
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

if ! command -v curl >/dev/null || ! command -v python3 >/dev/null \
    || ! command -v git >/dev/null || ! command -v timeout >/dev/null; then
    die "curl, python3, git, and timeout are required"
fi

log "building Server, Runner, and CLI binaries"
"$CARGO_BIN" build --quiet \
    -p webcodex --bin webcodex-server \
    -p webcodex-runner --bin webcodex-runner \
    -p webcodex-cli --bin webcodex

PORT="$(free_port)"
BOOTSTRAP_KEY="$(random_secret)"
SHARED_KEY_A="hosted-a-$(random_secret)"
SHARED_KEY_B="hosted-b-$(random_secret)"
TMP_ROOT="$(mktemp -d /tmp/webcodex-hosted-connect-e2e.XXXXXX)"
mkdir -p "$TMP_ROOT/data" "$TMP_ROOT/project" "$TMP_ROOT/second-project" "$TMP_ROOT/home"
(
    cd "$TMP_ROOT/project"
    git init -q -b main
    git config user.email e2e@example.invalid
    git config user.name "WebCodex E2E"
    printf '# hosted connect smoke\n' > README.md
    git add README.md
    git commit -q -m init
)
printf '# second hosted project\n' >"$TMP_ROOT/second-project/README.md"

log "exercising the real bounded hosted log writer"
ROTATION_PROFILE="rotation-e2e"
ROTATION_STATE="$TMP_ROOT/state/webcodex/clients/$ROTATION_PROFILE"
mkdir -p "$ROTATION_STATE"
printf 'profile = "%s"\n' "$ROTATION_PROFILE" >"$ROTATION_STATE/hosted-connect"
python3 -c '
import sys
out = sys.stdout.buffer
for start in range(0, 350000, 1000):
    out.write(b"".join(
        f"{line:06d} ".encode() + b"x" * 96 + b"\n"
        for line in range(start, min(start + 1000, 350000))
    ))
' | timeout 45 "$REPO_DIR/target/debug/webcodex" \
    __hosted-log-writer "$ROTATION_STATE"
for log_file in runner.log runner.log.1 runner.log.2; do
    [ -f "$ROTATION_STATE/$log_file" ] \
        || die "runtime rotation did not create $log_file"
    [ "$(stat -c '%a' "$ROTATION_STATE/$log_file")" = "600" ] \
        || die "$log_file is not mode 0600"
    [ "$(stat -c '%s' "$ROTATION_STATE/$log_file")" -le 10485760 ] \
        || die "$log_file exceeded the 10 MiB bound"
done
ROTATION_TOTAL="$(du -cb "$ROTATION_STATE"/runner.log* | tail -1 | awk '{print $1}')"
[ "$ROTATION_TOTAL" -le 31457280 ] \
    || die "runtime log rotation exceeded the three-file bound"
HOME="$TMP_ROOT/home" \
XDG_CONFIG_HOME="$TMP_ROOT/config" \
XDG_STATE_HOME="$TMP_ROOT/state" \
"$REPO_DIR/target/debug/webcodex" runner logs --profile "$ROTATION_PROFILE" --lines 100 \
    >"$TMP_ROOT/rotation-tail.out"
[ "$(head -1 "$TMP_ROOT/rotation-tail.out" | cut -d' ' -f1)" = "349900" ] \
    || die "bounded runner logs did not return the expected first tail line"
[ "$(tail -1 "$TMP_ROOT/rotation-tail.out" | cut -d' ' -f1)" = "349999" ] \
    || die "bounded runner logs did not return the expected final tail line"

log "starting temporary shared-key-enabled Server"
WEBCODEX_ADDR="127.0.0.1:${PORT}" \
WEBCODEX_DATA="$TMP_ROOT/data" \
WEBCODEX_TOKEN="$BOOTSTRAP_KEY" \
WEBCODEX_SHARED_KEY_ENABLED=true \
RUST_LOG=warn \
"$REPO_DIR/target/debug/webcodex-server" >"$TMP_ROOT/server.log" 2>&1 &
SERVER_PID=$!

for _ in $(seq 1 80); do
    deadline_ok || die "Server readiness exceeded the test deadline"
    if post "$BOOTSTRAP_KEY" /api/runtime/status '{}' >/dev/null 2>&1; then
        break
    fi
    sleep 0.25
done
post "$BOOTSTRAP_KEY" /api/runtime/status '{}' >/dev/null \
    || die "Server did not become ready"

log "running the real one-command connection"
HOME="$TMP_ROOT/home" \
XDG_CONFIG_HOME="$TMP_ROOT/config" \
XDG_STATE_HOME="$TMP_ROOT/state" \
"$REPO_DIR/target/debug/webcodex" connect "http://127.0.0.1:${PORT}" \
    --key "$SHARED_KEY_A" \
    --project "$TMP_ROOT/project" \
    >"$TMP_ROOT/connect-first.out"

PROFILE="$(awk -F': *' '$1 == "Profile" {print $2}' "$TMP_ROOT/connect-first.out")"
CLIENT_ID="$(awk -F': *' '$1 == "Client" {print $2}' "$TMP_ROOT/connect-first.out")"
RUNTIME_PROJECT="$(awk '$0 ~ /^Runtime project:/ {sub(/^Runtime project:[[:space:]]*/, ""); print}' "$TMP_ROOT/connect-first.out")"
[ -n "$PROFILE" ] && [ -n "$CLIENT_ID" ] && [ -n "$RUNTIME_PROJECT" ] \
    || die "connect output did not include profile, client, and project"
[ "$RUNTIME_PROJECT" = "agent:${CLIENT_ID}:project" ] \
    || die "connect returned an unexpected runtime project id"

PROFILE_DIR="$TMP_ROOT/config/webcodex/clients/$PROFILE"
STATE_DIR="$TMP_ROOT/state/webcodex/clients/$PROFILE"
[ "$(stat -c '%a' "$PROFILE_DIR/runner.toml")" = "600" ] \
    || die "runner.toml is not mode 0600"
[ "$(stat -c '%a' "$STATE_DIR/runner.toml")" = "600" ] \
    || die "runner state is not mode 0600"
[ "$(stat -c '%a' "$STATE_DIR/runner.log")" = "600" ] \
    || die "runner log is not mode 0600"
[ -f "$PROFILE_DIR/project-registry/project.toml" ] \
    || die "connect did not register the project locally"
[ -f "$STATE_DIR/runner.toml" ] && [ -f "$STATE_DIR/runner.log" ] \
    || die "connect did not persist Runner state and logs"
FIRST_PID="$(python3 -c 'import sys,tomllib; print(tomllib.load(open(sys.argv[1],"rb"))["pid"])' "$STATE_DIR/runner.toml")"
LOG_WRITER_PID="$(python3 -c 'import sys,tomllib; print(tomllib.load(open(sys.argv[1],"rb"))["log_writer"]["pid"])' "$STATE_DIR/runner.toml")"
kill -0 "$FIRST_PID" 2>/dev/null || die "Runner did not survive connect command exit"
process_active "$LOG_WRITER_PID" \
    || die "hosted log writer did not survive connect command exit"
[ ! -e "$PROFILE_DIR/.hosted-key-disclosed" ] \
    || die "an explicitly supplied key created a disclosure marker"

PROJECTS_A="$(post "$SHARED_KEY_A" /api/projects/list '{}')"
[ "$(printf '%s' "$PROJECTS_A" | json_field output.projects.0.id)" = "$RUNTIME_PROJECT" ] \
    || die "same-key project visibility failed"
READ_RESPONSE="$(post "$SHARED_KEY_A" /api/tools/call \
    "{\"tool\":\"read_files\",\"params\":{\"project\":\"${RUNTIME_PROJECT}\",\"items\":[{\"path\":\"README.md\"}]}}")"
if [ "$(printf '%s' "$READ_RESPONSE" | json_field success)" != "True" ]; then
    READ_ERROR="$(printf '%s' "$READ_RESPONSE" | python3 -c '
import json,sys
value=json.load(sys.stdin)
print(json.dumps({
    "success": value.get("success"),
    "error": value.get("error"),
    "output": value.get("output"),
}, separators=(",",":")))
')"
    die "same-key read_files failed (${READ_ERROR})"
fi
printf '%s' "$READ_RESPONSE" | grep -q 'hosted connect smoke' \
    || die "same-key read_files returned unexpected content"

PROJECTS_B="$(post "$SHARED_KEY_B" /api/projects/list '{}')"
[ "$(printf '%s' "$PROJECTS_B" | json_field output.count)" = "0" ] \
    || die "a different key discovered the connected project"

log "verifying client-id collision failure cleans up the rejected Runner"
set +e
HOME="$TMP_ROOT/home" \
XDG_CONFIG_HOME="$TMP_ROOT/config" \
XDG_STATE_HOME="$TMP_ROOT/state" \
"$REPO_DIR/target/debug/webcodex" connect "http://127.0.0.1:${PORT}" \
    --key "$SHARED_KEY_B" \
    --client-id "$CLIENT_ID" \
    --project "$TMP_ROOT/project" \
    >"$TMP_ROOT/connect-collision.out" 2>"$TMP_ROOT/connect-collision.err"
COLLISION_STATUS=$?
set -e
[ "$COLLISION_STATUS" -ne 0 ] || die "cross-key client-id collision unexpectedly succeeded"
grep -q 'Runner logs:' "$TMP_ROOT/connect-collision.err" \
    || die "collision failure did not report the Runner log path"
ACTIVE_STATE_COUNT="$(find "$TMP_ROOT/state/webcodex/clients" -name runner.toml -type f | wc -l)"
[ "$ACTIVE_STATE_COUNT" = "1" ] \
    || die "collision failure left an extra active Runner state"

log "re-running connect to verify profile, identity, and process reuse"
HOME="$TMP_ROOT/home" \
XDG_CONFIG_HOME="$TMP_ROOT/config" \
XDG_STATE_HOME="$TMP_ROOT/state" \
"$REPO_DIR/target/debug/webcodex" connect "http://127.0.0.1:${PORT}" \
    --key "$SHARED_KEY_A" \
    --project "$TMP_ROOT/project" \
    >"$TMP_ROOT/connect-second.out"
SECOND_PID="$(python3 -c 'import sys,tomllib; print(tomllib.load(open(sys.argv[1],"rb"))["pid"])' "$STATE_DIR/runner.toml")"
[ "$FIRST_PID" = "$SECOND_PID" ] || die "repeated connect started a duplicate Runner"
[ "$(awk -F': *' '$1 == "Profile" {print $2}' "$TMP_ROOT/connect-second.out")" = "$PROFILE" ] \
    || die "repeated connect changed the profile"

log "adding a second project without replacing the first"
HOME="$TMP_ROOT/home" \
XDG_CONFIG_HOME="$TMP_ROOT/config" \
XDG_STATE_HOME="$TMP_ROOT/state" \
"$REPO_DIR/target/debug/webcodex" connect "http://127.0.0.1:${PORT}" \
    --key "$SHARED_KEY_A" \
    --project "$TMP_ROOT/second-project" \
    >"$TMP_ROOT/connect-third.out"
PROJECTS_TWO="$(post "$SHARED_KEY_A" /api/projects/list '{}')"
printf '%s' "$PROJECTS_TWO" | python3 -c '
import json,sys
projects=json.load(sys.stdin).get("output",{}).get("projects",[])
ids={project.get("id") for project in projects}
expected=set(sys.argv[1:])
raise SystemExit(0 if ids == expected else 1)
' "$RUNTIME_PROJECT" "agent:${CLIENT_ID}:second-project" \
    || die "adding a second project replaced or duplicated project registration"

HOME="$TMP_ROOT/home" \
XDG_CONFIG_HOME="$TMP_ROOT/config" \
XDG_STATE_HOME="$TMP_ROOT/state" \
"$REPO_DIR/target/debug/webcodex" runner status --profile "$PROFILE" \
    >"$TMP_ROOT/status.out"
grep -q 'runner mode:.*hosted local process' "$TMP_ROOT/status.out" \
    || die "runner status did not recognize the hosted Runner"
grep -q 'client online:.*yes' "$TMP_ROOT/status.out" \
    || die "runner status did not confirm Server visibility"

for safe_output in "$TMP_ROOT/connect-first.out" "$TMP_ROOT/connect-second.out" \
    "$TMP_ROOT/connect-third.out" "$TMP_ROOT/status.out" \
    "$STATE_DIR"/runner.log* "$STATE_DIR/runner.toml"; do
    if grep -F "$SHARED_KEY_A" "$safe_output" >/dev/null 2>&1; then
        die "shared key leaked into $(basename "$safe_output")"
    fi
done
[ -z "$(git -C "$TMP_ROOT/project" status --porcelain)" ] \
    || die "connect modified the user project"

HOME="$TMP_ROOT/home" \
XDG_CONFIG_HOME="$TMP_ROOT/config" \
XDG_STATE_HOME="$TMP_ROOT/state" \
"$REPO_DIR/target/debug/webcodex" runner stop --profile "$PROFILE" \
    >"$TMP_ROOT/stop.out"
[ ! -f "$STATE_DIR/runner.toml" ] || die "runner stop left active Runner state"
for _ in $(seq 1 40); do
    if ! process_active "$LOG_WRITER_PID"; then
        break
    fi
    sleep 0.05
done
if process_active "$LOG_WRITER_PID"; then
    die "runner stop left the hosted log writer running"
fi
HOME="$TMP_ROOT/home" \
XDG_CONFIG_HOME="$TMP_ROOT/config" \
XDG_STATE_HOME="$TMP_ROOT/state" \
"$REPO_DIR/target/debug/webcodex" runner status --profile "$PROFILE" \
    >"$TMP_ROOT/status-stopped.out"
grep -q 'runner active:.*false' "$TMP_ROOT/status-stopped.out" \
    || die "runner status did not report the stopped Runner"
PROFILE=""
log "PASS"
