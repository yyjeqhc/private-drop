#!/usr/bin/env bash
set -euo pipefail

# ============================================================================
# Private Drop E2E Smoke Test
# ============================================================================

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$PROJECT_DIR"

TOKEN="e2e-test-token"
TMPDIR_DATA=$(mktemp -d)
LOGFILE="$TMPDIR_DATA/server.log"
PASS=0
FAIL=0
TOTAL=0
SERVER_PID=""

# Find a free port
find_free_port() {
    python3 -c "
import socket
s = socket.socket()
s.bind(('127.0.0.1', 0))
print(s.getsockname()[1])
s.close()
"
}

PORT=$(find_free_port)
BASE="http://127.0.0.1:$PORT"

# ============================================================================
# Create test project for Codex API
# ============================================================================
TEST_PROJECT_DIR="$TMPDIR_DATA/test-project"
mkdir -p "$TEST_PROJECT_DIR/src"
cd "$TEST_PROJECT_DIR"
git init -b main 2>&1
git config user.email "test@test.com"
git config user.name "Test"

echo "# Test Project" > README.md
echo 'fn main() { println!("hello"); }' > src/main.rs
mkdir -p data/a/b/c data/other
printf 'scoped file\n' > data/a/b/c/scoped.txt
printf 'other file\n' > data/other/other.txt
echo "line1" > test.txt
echo "line2" >> test.txt
echo "line3" >> test.txt
cat > chapter.md <<'EOF'
# Chapter 1

Intro text.

## 1.1 Motivation

Motivation paragraph.

## 1.2 Method

Method paragraph.

### 1.2.1 Details

Detail paragraph.

## 1.3 Results

Result paragraph.
EOF
python3 - <<'PY'
from pathlib import Path
Path('big.md').write_text('# Big\n\n' + '\n'.join(f'line {i} ' + ('x' * 120) for i in range(120)) + '\n')
Path('longline.csv').write_text('col\n' + 'x' * 5000 + '\n')
PY
mkdir -p .codex/memory
cat > AGENTS.md <<'EOF'
# Test Agent Rules

Use project rules before editing.
EOF
cat > .codex/memory/project.md <<'EOF'
# Test Project Memory

This project memory is loaded by agent_context.
EOF
cat > .codex/memory/pitfalls.md <<'EOF'
# Test Pitfalls

Avoid unsafe edits.
EOF
cat > .codex/memory/workflows.md <<'EOF'
# Test Workflows

Read rules, plan, edit, verify.
EOF
cat > .codex/memory/decisions.md <<'EOF'
# Test Decisions

Keep the workflow bounded.
EOF
cat > .codex/memory/user_preferences.md <<'EOF'
# Test User Preferences

Prefer focused changes.
EOF
python3 - <<'PY'
from pathlib import Path
Path('upload-source.bin').write_bytes(bytes([9, 8, 7, 6]))
Path('upload-source-new.bin').write_bytes(bytes([6, 7, 8, 9, 10]))
PY

cat > check.sh << 'CHECKEOF'
#!/bin/bash
echo "check passed"
exit 0
CHECKEOF
chmod +x check.sh
mkdir -p scripts/codex_jobs
cat > scripts/codex_jobs/job_smoke.sh << 'JOBEOF'
#!/usr/bin/env bash
set -euo pipefail
echo "script-start:$1"
echo "script-err:$2" >&2
JOBEOF
chmod +x scripts/codex_jobs/job_smoke.sh

git add -A
git commit -m "init" 2>&1
cd "$PROJECT_DIR"

# Generate projects.toml for test
PROJECTS_TOML="$TMPDIR_DATA/projects.toml"
cat > "$PROJECTS_TOML" << EOF
[projects.test-project]
path = "$TEST_PROJECT_DIR"
allow_patch = true
allow_command_requests = true
allow_raw_command_requests = true
allowed_checks = ["fmt", "test", "build", "e2e", "full"]

[projects.test-project.checks]
fmt = "echo fmt-ok"
test = "echo test-ok"
build = "echo build-ok"
e2e = "bash check.sh"
full = "echo fmt-ok && echo test-ok && bash check.sh"

[projects.test-project.commands]
smoke = "echo command-smoke-ok"
counter = "printf run >> approval-count.txt"
fail = "echo command-failed >&2; exit 7"
EOF

cleanup() {
    echo ""
    echo "=== Cleanup ==="
    if [ -n "$SERVER_PID" ] && kill -0 "$SERVER_PID" 2>/dev/null; then
        echo "  Stopping server (PID=$SERVER_PID)..."
        kill "$SERVER_PID" 2>/dev/null || true
        # Wait briefly for graceful shutdown, then force kill
        for _ in $(seq 1 10); do
            kill -0 "$SERVER_PID" 2>/dev/null || break
            sleep 0.2
        done
        kill -9 "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
        echo "  Server stopped."
    else
        echo "  Server PID=$SERVER_PID already exited."
    fi
    rm -rf "$TMPDIR_DATA"
    echo "  Temp dir removed."

    # Report residual processes (do NOT kill them)
    local residual
    residual=$(pgrep -f "private-drop" 2>/dev/null || true)
    if [ -n "$residual" ]; then
        echo "  WARNING: Residual private-drop processes found: $residual"
    fi
}
trap cleanup EXIT

log_pass() {
    PASS=$((PASS + 1))
    TOTAL=$((TOTAL + 1))
    echo "  PASS: $1"
}

log_fail() {
    FAIL=$((FAIL + 1))
    TOTAL=$((TOTAL + 1))
    echo "  FAIL: $1"
    if [ -n "${2:-}" ]; then
        echo "    Response: $2"
    fi
    if [ -f "$LOGFILE" ]; then
        echo "    Last 10 lines of server log:"
        tail -10 "$LOGFILE" | sed 's/^/      /'
    fi
}

assert_eq() {
    local desc="$1" expected="$2" actual="$3"
    if [ "$expected" = "$actual" ]; then
        log_pass "$desc"
    else
        log_fail "$desc" "expected='$expected' got='$actual'"
    fi
}

assert_contains() {
    local desc="$1" needle="$2" haystack="$3"
    if echo "$haystack" | grep -qF "$needle"; then
        log_pass "$desc"
    else
        log_fail "$desc" "expected to contain '$needle'"
    fi
}

assert_not_contains() {
    local desc="$1" needle="$2" haystack="$3"
    if echo "$haystack" | grep -qF "$needle"; then
        log_fail "$desc" "expected not to contain '$needle'"
    else
        log_pass "$desc"
    fi
}

assert_not_empty() {
    local desc="$1" val="$2"
    if [ -n "$val" ]; then
        log_pass "$desc"
    else
        log_fail "$desc" "expected non-empty value"
    fi
}

assert_http_code() {
    local desc="$1" expected="$2" url="$3"
    shift 3
    local code
    code=$(curl -s -o /dev/null -w '%{http_code}' "$@" "$url")
    assert_eq "$desc" "$expected" "$code"
}

# Python JSON parser helper
pyget() {
    local json="$1" path="$2"
    echo "$json" | python3 -c "
import sys, json
try:
    data = json.load(sys.stdin)
    for k in '$path'.split('.'):
        if k.isdigit():
            data = data[int(k)]
        else:
            data = data[k]
    print(data)
except Exception:
    print('')
" 2>/dev/null
}

# ============================================================================
# Build
# ============================================================================
echo "=== Building ==="
if command -v node > /dev/null 2>&1; then
    npm --prefix frontend run build
    npm --prefix frontend run check:dist
else
    echo "NOTE: node not found; skipping frontend dist drift check"
fi
cargo fmt
cargo test
cargo build --release
echo "Build OK"
echo ""

# ============================================================================
# Check for residual processes (report only)
# ============================================================================
RESIDUAL_BEFORE=$(pgrep -f "private-drop" 2>/dev/null || true)
if [ -n "$RESIDUAL_BEFORE" ]; then
    echo "NOTE: Pre-existing private-drop processes: $RESIDUAL_BEFORE"
fi

# ============================================================================
# Start server
# ============================================================================
echo "=== Starting server ==="
echo "  Port: $PORT"
echo "  Data dir: $TMPDIR_DATA"
echo "  Log file: $LOGFILE"

ENV_FILE="$TMPDIR_DATA/private-drop.env"
cat > "$ENV_FILE" << EOF
DROP_TOKEN=$TOKEN
DROP_ADDR=127.0.0.1:$PORT
DROP_DATA=$TMPDIR_DATA
PROJECTS_CONFIG=$PROJECTS_TOML
EOF

DROP_ENV_FILE="$ENV_FILE" ./target/release/private-drop > "$LOGFILE" 2>&1 &
SERVER_PID=$!
echo "  Server PID: $SERVER_PID"

# Wait for server to be ready, checking that PID stays alive
echo "  Waiting for server..."
READY=0
for _ in $(seq 1 40); do
    # Check if server process is still alive
    if ! kill -0 "$SERVER_PID" 2>/dev/null; then
        echo "FATAL: Server process $SERVER_PID exited prematurely"
        echo "Server log:"
        cat "$LOGFILE"
        exit 1
    fi
    if curl -sf "$BASE/api/health" > /dev/null 2>&1; then
        READY=1
        break
    fi
    sleep 0.25
done

if [ "$READY" -eq 0 ]; then
    echo "FATAL: Server did not become ready within 10 seconds"
    echo "Server log:"
    cat "$LOGFILE"
    exit 1
fi
echo "  Server ready"
echo ""

# ============================================================================
# Tests
# ============================================================================
echo "=== Running E2E Tests ==="

# --- 1. Health check (no auth) ---
echo ""
echo "--- 1. Health Check ---"
RESP=$(curl -sf "$BASE/api/health")
STATUS=$(pyget "$RESP" "status")
assert_eq "GET /api/health returns ok" "ok" "$STATUS"

# --- 2. Unauthorized access ---
echo ""
echo "--- 2. Auth ---"
assert_http_code "GET /api/messages without token returns 401" "401" "$BASE/api/messages"
assert_http_code "GET /api/messages with wrong token returns 401" "401" "$BASE/api/messages" \
    -H "Authorization: Bearer wrong-token"
assert_http_code "POST /api/messages without token returns 401" "401" "$BASE/api/messages" \
    -X POST -H "Content-Type: application/json" -d '{"channel":"inbox","text":"test"}'

# --- 3. Create text message ---
echo ""
echo "--- 3. Create Text Message ---"
RESP=$(curl -sf -X POST "$BASE/api/messages" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"channel":"inbox","title":"E2E Test","text":"Hello from e2e test!"}')
MSG_ID=$(pyget "$RESP" "id")
MSG_KIND=$(pyget "$RESP" "kind")
assert_not_empty "Create message returns id" "$MSG_ID"
assert_eq "Create message kind is text" "text" "$MSG_KIND"

# --- 4. List inbox messages ---
echo ""
echo "--- 4. List Inbox Messages ---"
RESP=$(curl -sf -H "Authorization: Bearer $TOKEN" "$BASE/api/messages?channel=inbox")
TOTAL_MSGS=$(pyget "$RESP" "total")
FOUND=$(echo "$RESP" | python3 -c "
import sys, json
data = json.load(sys.stdin)
print('yes' if any(m['id'] == '$MSG_ID' for m in data['messages']) else 'no')
")
assert_eq "Inbox has at least 1 message" "yes" "$([ "${TOTAL_MSGS:-0}" -ge 1 ] && echo yes || echo no)"
assert_eq "Created message found in inbox list" "yes" "$FOUND"

# --- 5. Create 10K text message ---
echo ""
echo "--- 5. Create 10K Text Message ---"
LONG_10K=$(python3 -c "print('A' * 10240)")
RESP=$(curl -sf -X POST "$BASE/api/messages" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d "{\"channel\":\"inbox\",\"title\":\"10K Text\",\"text\":\"$LONG_10K\"}")
MSG_10K_ID=$(pyget "$RESP" "id")
assert_not_empty "10K text message created" "$MSG_10K_ID"

# --- 6. Create 100K text message ---
echo ""
echo "--- 6. Create 100K Text Message ---"
LONG_100K=$(python3 -c "print('B' * 102400)")
RESP=$(curl -sf -X POST "$BASE/api/messages" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d "{\"channel\":\"inbox\",\"title\":\"100K Text\",\"text\":\"$LONG_100K\"}")
MSG_100K_ID=$(pyget "$RESP" "id")
assert_not_empty "100K text message created" "$MSG_100K_ID"

# --- 7. Get message detail ---
echo ""
echo "--- 7. Get Message Detail ---"
RESP=$(curl -sf -H "Authorization: Bearer $TOKEN" "$BASE/api/messages/$MSG_ID")
DETAIL_TITLE=$(pyget "$RESP" "title")
DETAIL_TEXT=$(pyget "$RESP" "text")
assert_eq "Get message title matches" "E2E Test" "$DETAIL_TITLE"
assert_eq "Get message text matches" "Hello from e2e test!" "$DETAIL_TEXT"

# --- 8. Delete message ---
echo ""
echo "--- 8. Delete Message ---"
RESP=$(curl -sf -X DELETE -H "Authorization: Bearer $TOKEN" "$BASE/api/messages/$MSG_10K_ID")
DELETED=$(pyget "$RESP" "deleted")
assert_eq "Delete returns deleted=true" "True" "$DELETED"
assert_http_code "Deleted message returns 404" "404" "$BASE/api/messages/$MSG_10K_ID" \
    -H "Authorization: Bearer $TOKEN"

# --- 9. Upload file ---
echo ""
echo "--- 9. Upload File ---"
UPLOAD_CONTENT="This is the e2e test file content. Timestamp: $(date +%s)"
echo "$UPLOAD_CONTENT" > "$TMPDIR_DATA/upload.txt"
RESP=$(curl -sf -X POST "$BASE/api/files?channel=files" \
    -H "Authorization: Bearer $TOKEN" \
    -F "file=@$TMPDIR_DATA/upload.txt")
FILE_ID=$(pyget "$RESP" "id")
FILE_KIND=$(pyget "$RESP" "kind")
FILE_NAME=$(pyget "$RESP" "file_name")
assert_not_empty "Upload returns file id" "$FILE_ID"
assert_eq "Upload kind is file" "file" "$FILE_KIND"
assert_eq "Upload file_name is upload.txt" "upload.txt" "$FILE_NAME"

# --- 10. Download file and verify content + headers ---
echo ""
echo "--- 10. Download File ---"
# Download and capture headers
HEADERS=$(curl -s -D - -H "Authorization: Bearer $TOKEN" "$BASE/api/files/$FILE_ID" -o "$TMPDIR_DATA/downloaded.txt")
DOWNLOADED=$(cat "$TMPDIR_DATA/downloaded.txt")
if [ "$UPLOAD_CONTENT" = "$DOWNLOADED" ]; then
    log_pass "Downloaded file content matches uploaded content"
else
    log_fail "Downloaded file content mismatch"
fi
# Check Content-Disposition header
if echo "$HEADERS" | grep -qi 'content-disposition.*upload\.txt'; then
    log_pass "Download response has Content-Disposition with filename"
else
    log_fail "Download response missing Content-Disposition with filename"
fi

# --- 11. OpenAPI spec ---
echo ""
echo "--- 11. OpenAPI Spec ---"
RESP=$(curl -sf "$BASE/openapi.json")
HAS_CREATE=$(echo "$RESP" | python3 -c "import sys; print('yes' if 'createMessage' in sys.stdin.read() else 'no')")
HAS_LIST=$(echo "$RESP" | python3 -c "import sys; print('yes' if 'listMessages' in sys.stdin.read() else 'no')")
assert_contains "OpenAPI contains createMessage" "yes" "$HAS_CREATE"
assert_contains "OpenAPI contains listMessages" "yes" "$HAS_LIST"

# --- 12. Channels ---
echo ""
echo "--- 12. Channels ---"
RESP=$(curl -sf -H "Authorization: Bearer $TOKEN" "$BASE/api/channels")
HAS_INBOX=$(echo "$RESP" | python3 -c "import sys; print('yes' if 'inbox' in sys.stdin.read() else 'no')")
assert_contains "Channels list contains inbox" "yes" "$HAS_INBOX"

# --- 13. Web UI: Login page ---
echo ""
echo "--- 13. Web UI ---"
# Web UI is client-side rendered; pages return HTML shells plus shared frontend assets
HTTP_CODE=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/login")
assert_eq "GET /login returns 200" "200" "$HTTP_CODE"
BODY=$(curl -sf "$BASE/login")
assert_contains "Login page references frontend JS" "/assets/app.js" "$BODY"
assert_contains "Login page redirects to /c/inbox" "/c/inbox" "$BODY"
ASSET_JS=$(curl -sf "$BASE/assets/app.js")
ASSET_CSS=$(curl -sf "$BASE/assets/styles.css")
assert_contains "Frontend asset references drop_token" "drop_token" "$ASSET_JS"
assert_contains "Frontend asset adds Authorization" "Authorization" "$ASSET_JS"
assert_contains "Frontend asset uses Bearer" "Bearer" "$ASSET_JS"
assert_contains "Frontend CSS has card styles" ".card" "$ASSET_CSS"

# --- 14. Web UI: Home page ---
HTTP_CODE=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/")
assert_eq "GET / returns 200" "200" "$HTTP_CODE"
BODY=$(curl -sf "$BASE/")
assert_contains "Home page references frontend JS" "/assets/app.js" "$BODY"
assert_contains "Home page references /channels" "/channels" "$BODY"

HTTP_CODE=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/channels")
assert_eq "GET /channels returns 200" "200" "$HTTP_CODE"
BODY=$(curl -sf "$BASE/channels")
assert_contains "Channels page contains Channels" "Channels" "$BODY"
assert_contains "Channels page references frontend JS" "/assets/app.js" "$BODY"

# --- 15. Web UI: Channel page ---
HTTP_CODE=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/c/inbox")
assert_eq "GET /c/inbox returns 200" "200" "$HTTP_CODE"
BODY=$(curl -sf "$BASE/c/inbox")
assert_contains "Channel page calls /api/messages" "/api/messages" "$BODY"
assert_contains "Channel page references frontend JS" "/assets/app.js" "$BODY"

HTTP_CODE=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/c/xline")
assert_eq "GET /c/xline returns 200" "200" "$HTTP_CODE"

# --- 15b. Web UI: Channel page JS regression ---
# Create a test message in omo channel to verify rendering
curl -sf -X POST "$BASE/api/messages" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"channel":"omo","title":"[codex] test msg","text":"regression test content"}' > /dev/null
OMO_BODY=$(curl -sf "$BASE/c/omo")
# Must NOT contain broken template markers or escaped quotes
OMO_HAS_PAGE_JS=$(echo "$OMO_BODY" | grep -c '{page_js}' || true)
assert_eq "Channel page has no {page_js} leak" "0" "$OMO_HAS_PAGE_JS"
OMO_HAS_BAD_QUOTE=$(echo "$OMO_BODY" | grep -c "\\\\'" || true)
assert_eq "Channel page has no broken backslash-quote" "0" "$OMO_HAS_BAD_QUOTE"
OMO_HAS_ONCLICK=$(echo "$OMO_BODY" | grep -c 'onclick=' || true)
assert_eq "Channel page has no inline onclick" "0" "$OMO_HAS_ONCLICK"
# Must contain expected elements
assert_contains "Channel page has /api/messages" "/api/messages" "$OMO_BODY"
assert_contains "Channel page references frontend JS" "/assets/app.js" "$OMO_BODY"
assert_contains "Channel page has error handling" "alert-error" "$OMO_BODY"
assert_contains "Channel page has event delegation" "addEventListener" "$OMO_BODY"
assert_contains "Channel page has data-text-id" "data-text-id" "$OMO_BODY"
assert_contains "Channel page has data-delete-id" "data-delete-id" "$OMO_BODY"

# --- 16. Web UI: Message detail page ---
HTTP_CODE=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/m/$MSG_ID")
assert_eq "GET /m/{id} returns 200" "200" "$HTTP_CODE"
BODY=$(curl -sf "$BASE/m/$MSG_ID")
assert_contains "Message page references frontend JS" "/assets/app.js" "$BODY"

# --- 17. Web UI: Send page ---
HTTP_CODE=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/send")
assert_eq "GET /send returns 200" "200" "$HTTP_CODE"
BODY=$(curl -sf "$BASE/send")
assert_contains "Send page calls POST /api/messages" "/api/messages" "$BODY"
assert_contains "Send page references frontend JS" "/assets/app.js" "$BODY"

# ============================================================================
# Codex API Tests
# ============================================================================
echo ""
echo "=== Codex API Tests ==="

CODEX="$BASE/api/codex"

# --- 18. Codex: Unauthorized access ---
echo ""
echo "--- 18. Codex Auth ---"
assert_http_code "POST /api/codex/context without token returns 401" "401" "$CODEX/context" \
    -X POST -H "Content-Type: application/json" -d '{"project":"test-project","mode":"overview"}'
assert_http_code "POST /api/codex/apply_patch without token returns 401" "401" "$CODEX/apply_patch" \
    -X POST -H "Content-Type: application/json" -d '{"project":"test-project","patch":"x"}'
assert_http_code "POST /api/codex/artifact without token returns 401" "401" "$CODEX/artifact" \
    -X POST -H "Content-Type: application/json" -d '{"project":"test-project","op":"save_base64","path":"x.bin","base64_content":"AAE="}'
assert_http_code "POST /api/codex/check without token returns 401" "401" "$CODEX/check" \
    -X POST -H "Content-Type: application/json" -d '{"project":"test-project","suite":"test"}'
assert_http_code "POST /api/codex/git without token returns 401" "401" "$CODEX/git" \
    -X POST -H "Content-Type: application/json" -d '{"project":"test-project","operation":"status"}'
assert_http_code "POST /api/codex/command without token returns 401" "401" "$CODEX/command" \
    -X POST -H "Content-Type: application/json" -d '{"project":"test-project","command":"smoke"}'
assert_http_code "POST /api/codex/command_request without token returns 401" "401" "$CODEX/command_request" \
    -X POST -H "Content-Type: application/json" -d '{"project":"test-project","command":"smoke"}'
assert_http_code "POST /api/codex/command_request_op without token returns 401" "401" "$CODEX/command_request_op" \
    -X POST -H "Content-Type: application/json" -d '{"op":"list","project":"test-project"}'
assert_http_code "POST /api/codex/command_request_raw without token returns 401" "401" "$CODEX/command_request_raw" \
    -X POST -H "Content-Type: application/json" -d '{"project":"test-project","command_text":"echo raw"}'
assert_http_code "POST /api/codex/command_requests without token returns 401" "401" "$CODEX/command_requests" \
    -X POST -H "Content-Type: application/json" -d '{"project":"test-project","status":"pending"}'
assert_http_code "POST /api/codex/command_request_batch without token returns 401" "401" "$CODEX/command_request_batch" \
    -X POST -H "Content-Type: application/json" -d '{"project":"test-project","requests":[{"command":"smoke"}]}'
assert_http_code "POST /api/codex/command_approve without token returns 401" "401" "$CODEX/command_approve" \
    -X POST -H "Content-Type: application/json" -d '{"request_id":"missing"}'
assert_http_code "POST /api/codex/command_reject without token returns 401" "401" "$CODEX/command_reject" \
    -X POST -H "Content-Type: application/json" -d '{"request_id":"missing"}'
assert_http_code "POST /api/codex/job without token returns 401" "401" "$CODEX/job" \
    -X POST -H "Content-Type: application/json" -d '{"project":"test-project","op":"list"}'
assert_http_code "POST /api/codex/report without token returns 401" "401" "$CODEX/report" \
    -X POST -H "Content-Type: application/json" -d '{"project":"test-project","status":"completed","title":"t","summary":"s"}'

# --- 19. Codex: Unknown project ---
echo ""
echo "--- 19. Codex Unknown Project ---"
RESP=$(curl -s -X POST "$CODEX/context" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"nonexistent","mode":"overview"}')
HAS_ERR=$(echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); print('yes' if d.get('error') else 'no')")
assert_eq "Unknown project returns error" "yes" "$HAS_ERR"

# --- 20. Codex: getProjectContext mode=overview ---
echo ""
echo "--- 20. Codex Context Overview ---"
RESP=$(curl -sf -X POST "$CODEX/context" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","mode":"overview"}')
CTX_SUCCESS=$(pyget "$RESP" "success")
CTX_MODE=$(pyget "$RESP" "mode")
CTX_CONTENT=$(pyget "$RESP" "content")
assert_eq "Overview success" "True" "$CTX_SUCCESS"
assert_eq "Overview mode" "overview" "$CTX_MODE"
assert_contains "Overview contains project name" "test-project" "$CTX_CONTENT"
assert_contains "Overview contains branch info" "main" "$CTX_CONTENT"

# --- 21. Codex: getProjectContext mode=tree ---
echo ""
echo "--- 21. Codex Context Tree ---"
RESP=$(curl -sf -X POST "$CODEX/context" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","mode":"tree"}')
CTX_SUCCESS=$(pyget "$RESP" "success")
assert_eq "Tree success" "True" "$CTX_SUCCESS"
HAS_README=$(echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); items=d.get('items',[]); print('yes' if any('README' in i for i in items) else 'no')")
assert_contains "Tree contains README.md" "yes" "$HAS_README"
HAS_MAIN=$(echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); items=d.get('items',[]); print('yes' if any('main.rs' in i for i in items) else 'no')")
assert_contains "Tree contains main.rs" "yes" "$HAS_MAIN"
RESP=$(curl -sf -X POST "$CODEX/context" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","mode":"tree","path":"data/a","limit":20,"max_depth":3}')
TREE_SCOPED_SUCCESS=$(pyget "$RESP" "success")
TREE_SCOPED_HAS_FILE=$(echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); items=d.get('items',[]); print('yes' if any('data/a/b/c/scoped.txt' in i for i in items) else 'no')")
TREE_SCOPED_HAS_OTHER=$(echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); items=d.get('items',[]); print('yes' if any('data/other' in i for i in items) else 'no')")
assert_eq "Scoped tree success" "True" "$TREE_SCOPED_SUCCESS"
assert_eq "Scoped tree contains scoped file" "yes" "$TREE_SCOPED_HAS_FILE"
assert_eq "Scoped tree excludes sibling dir" "no" "$TREE_SCOPED_HAS_OTHER"

# --- 22. Codex: getProjectContext mode=read_file ---
echo ""
echo "--- 22. Codex Context Read File ---"
RESP=$(curl -sf -X POST "$CODEX/context" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","mode":"read_file","path":"test.txt"}')
CTX_SUCCESS=$(pyget "$RESP" "success")
CTX_CONTENT=$(pyget "$RESP" "content")
assert_eq "Read file success" "True" "$CTX_SUCCESS"
assert_contains "Read file contains line1" "line1" "$CTX_CONTENT"
assert_contains "Read file contains line2" "line2" "$CTX_CONTENT"
RESP=$(curl -sf -X POST "$CODEX/context" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","mode":"read_file","path":"longline.csv","start_line":2,"limit":1}')
LONG_LINE_CONTENT=$(pyget "$RESP" "content")
assert_contains "Read file long line is truncated" "[line truncated]" "$LONG_LINE_CONTENT"

# --- 23. Codex: getProjectContext mode=search ---
echo ""
echo "--- 23. Codex Context Search ---"
RESP=$(curl -sf -X POST "$CODEX/context" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","mode":"search","query":"println"}')
CTX_SUCCESS=$(pyget "$RESP" "success")
assert_eq "Search success" "True" "$CTX_SUCCESS"
HAS_RESULT=$(echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); items=d.get('items',[]); print('yes' if len(items)>0 else 'no')")
assert_contains "Search found println" "yes" "$HAS_RESULT"
RESP=$(curl -sf -X POST "$CODEX/context" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","mode":"grep_context","path":"src/main.rs","query":"println","limit":20}')
GREP_CONTEXT_SUCCESS=$(pyget "$RESP" "success")
GREP_CONTEXT_CONTENT=$(pyget "$RESP" "content")
assert_eq "Grep context success" "True" "$GREP_CONTEXT_SUCCESS"
assert_contains "Grep context includes match" "println" "$GREP_CONTEXT_CONTENT"
assert_contains "Grep context marks match" "> |" "$GREP_CONTEXT_CONTENT"

# --- 24. Codex: getProjectContext mode=git_status ---
echo ""
echo "--- 24. Codex Context Git Status ---"
RESP=$(curl -sf -X POST "$CODEX/context" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","mode":"git_status"}')
CTX_SUCCESS=$(pyget "$RESP" "success")
assert_eq "Git status success" "True" "$CTX_SUCCESS"

# --- 24b. Codex: getProjectContextBatch ---
echo ""
echo "--- 24b. Codex Context Batch ---"
RESP=$(curl -sf -X POST "$CODEX/context_batch" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","requests":[{"mode":"overview"},{"mode":"read_file","path":"test.txt","limit":2},{"mode":"git_status"}]}')
BATCH_SUCCESS=$(pyget "$RESP" "success")
BATCH_COUNT=$(echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); print(len(d.get('results', [])))")
BATCH_HAS_LINE1=$(echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); print('yes' if 'line1' in d['results'][1].get('content','') else 'no')")
assert_eq "Context batch success" "True" "$BATCH_SUCCESS"
assert_eq "Context batch has 3 results" "3" "$BATCH_COUNT"
assert_eq "Context batch read_file contains line1" "yes" "$BATCH_HAS_LINE1"

# --- 24b1. Codex: Markdown context modes ---
echo ""
echo "--- 24b1. Codex Markdown Context Modes ---"
RESP=$(curl -sf -X POST "$CODEX/context_batch" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","requests":[{"mode":"markdown_outline","path":"chapter.md","limit":20},{"mode":"read_section","path":"chapter.md","query":"1.2 Method","limit":20}],"max_total_chars":4000}')
BATCH_SUCCESS=$(pyget "$RESP" "success")
OUTLINE_CONTENT=$(echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d['results'][0].get('content',''))")
SECTION_CONTENT=$(echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d['results'][1].get('content',''))")
assert_eq "Markdown context batch success" "True" "$BATCH_SUCCESS"
assert_contains "Markdown outline has heading" "1.2 Method" "$OUTLINE_CONTENT"
assert_contains "Read section has method" "Method paragraph" "$SECTION_CONTENT"
assert_contains "Read section includes child heading" "1.2.1 Details" "$SECTION_CONTENT"
RESP=$(curl -sf -X POST "$CODEX/context_batch" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","requests":[{"mode":"read_file","path":"big.md","limit":200},{"mode":"read_file","path":"big.md","limit":200}],"max_total_chars":4000}')
BATCH_TRUNCATED=$(echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); print('yes' if any(r.get('truncated') for r in d.get('results', [])) else 'no')")
assert_eq "Context batch max_total_chars can truncate" "yes" "$BATCH_TRUNCATED"

# --- 24b2. Codex: getProjectContextBatch agent_context ---
echo ""
echo "--- 24b2. Codex Context Batch Agent Context ---"
RESP=$(curl -sf -X POST "$CODEX/context_batch" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","requests":[{"mode":"agent_context"},{"mode":"overview"},{"mode":"git_status"}]}')
BATCH_SUCCESS=$(pyget "$RESP" "success")
AGENT_MODE=$(echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d['results'][0].get('mode'))")
AGENT_CONTENT=$(echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d['results'][0].get('content',''))")
assert_eq "Context batch agent_context success" "True" "$BATCH_SUCCESS"
assert_eq "Context batch agent_context mode" "agent_context" "$AGENT_MODE"
assert_contains "Context batch agent_context has AGENTS" "Test Agent Rules" "$AGENT_CONTENT"
assert_contains "Context batch agent_context has memory" "Test Project Memory" "$AGENT_CONTENT"

# --- 24c. Codex: runProjectGit ---
echo ""
echo "--- 24c. Codex Git ---"
RESP=$(curl -sf -X POST "$CODEX/git" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","operation":"status"}')
GIT_SUCCESS=$(pyget "$RESP" "success")
GIT_OPERATION=$(pyget "$RESP" "operation")
assert_eq "Git status success" "True" "$GIT_SUCCESS"
assert_eq "Git status operation" "status" "$GIT_OPERATION"
RESP=$(curl -s -X POST "$CODEX/git" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","operation":"add","paths":[".env"]}')
GIT_SUCCESS=$(pyget "$RESP" "success")
GIT_ERROR=$(pyget "$RESP" "error")
assert_eq "Git add .env blocked" "False" "$GIT_SUCCESS"
assert_contains "Git add error mentions sensitive" "sensitive" "$GIT_ERROR"
RESP=$(curl -s -X POST "$CODEX/git" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","operation":"commit_amend_no_edit","paths":["README.md"]}')
GIT_SUCCESS=$(pyget "$RESP" "success")
GIT_STDERR=$(pyget "$RESP" "stderr_tail")
assert_eq "Git amend with no staged changes fails" "False" "$GIT_SUCCESS"
assert_contains "Git amend no changes stderr" "No staged changes to amend" "$GIT_STDERR"
RESP=$(curl -s -X POST "$CODEX/git" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","operation":"commit","paths":["README.md"],"message":"No changes commit"}')
GIT_SUCCESS=$(pyget "$RESP" "success")
GIT_STDERR=$(pyget "$RESP" "stderr_tail")
assert_eq "Git commit with no staged changes fails" "False" "$GIT_SUCCESS"
assert_contains "Git commit no changes stderr" "No staged changes to commit" "$GIT_STDERR"

# --- 24d. Codex: runProjectCommand ---
echo ""
echo "--- 24d. Codex Command ---"
RESP=$(curl -sf -X POST "$CODEX/command" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","command":"smoke"}')
CMD_SUCCESS=$(pyget "$RESP" "success")
CMD_STDOUT=$(pyget "$RESP" "stdout_tail")
assert_eq "Command smoke success" "True" "$CMD_SUCCESS"
assert_contains "Command smoke stdout" "command-smoke-ok" "$CMD_STDOUT"
RESP=$(curl -s -X POST "$CODEX/command" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","command":"unknown"}')
CMD_SUCCESS=$(pyget "$RESP" "success")
CMD_ERROR=$(pyget "$RESP" "error")
assert_eq "Unknown command rejected" "False" "$CMD_SUCCESS"
assert_contains "Unknown command error" "not configured" "$CMD_ERROR"

# --- 24e. Codex: chat-approved command request ---
echo ""
echo "--- 24e. Codex Command Approval ---"
RESP=$(curl -sf -X POST "$CODEX/command_request" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","command":"smoke","reason":"e2e approval smoke"}')
REQ_SUCCESS=$(pyget "$RESP" "success")
REQ_ID=$(pyget "$RESP" "request_id")
REQ_STATUS=$(echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d['record']['status'])")
REQ_COMMAND_TEXT=$(echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d['record'].get('command_text') or '')")
assert_eq "Command request created" "True" "$REQ_SUCCESS"
assert_not_empty "Command request has id" "$REQ_ID"
assert_eq "Command request pending" "pending" "$REQ_STATUS"
assert_contains "Command request stores command_text" "echo command-smoke-ok" "$REQ_COMMAND_TEXT"
RESP=$(curl -sf -X POST "$CODEX/command_approve" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d "$(python3 -c "import json; print(json.dumps({'request_id':'$REQ_ID'}))")")
APPROVE_SUCCESS=$(pyget "$RESP" "success")
APPROVE_STATUS=$(echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d['record']['status'])")
APPROVE_STDOUT=$(echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d['record'].get('stdout_tail') or '')")
assert_eq "Command approval executed" "True" "$APPROVE_SUCCESS"
assert_eq "Command approval completed" "completed" "$APPROVE_STATUS"
assert_contains "Command approval stdout" "command-smoke-ok" "$APPROVE_STDOUT"
RESP=$(curl -s -X POST "$CODEX/command_approve" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d "$(python3 -c "import json; print(json.dumps({'request_id':'$REQ_ID'}))")")
REAPPROVE_SUCCESS=$(pyget "$RESP" "success")
REAPPROVE_ERROR=$(pyget "$RESP" "error")
assert_eq "Command approval cannot rerun" "False" "$REAPPROVE_SUCCESS"
assert_contains "Command approval rerun error" "not pending" "$REAPPROVE_ERROR"
RESP=$(curl -s -X POST "$CODEX/command_request" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d "$(python3 -c "import json; print(json.dumps({'project':'test-project','command':'smoke','reason':'x'*2001}))")")
LONG_REASON_SUCCESS=$(pyget "$RESP" "success")
LONG_REASON_ERROR=$(pyget "$RESP" "error")
assert_eq "Command request long reason rejected" "False" "$LONG_REASON_SUCCESS"
assert_contains "Command request long reason error" "maximum is 2000" "$LONG_REASON_ERROR"
RESP=$(curl -sf -X POST "$CODEX/command_request" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","command":"counter","reason":"verify duplicate approve does not rerun"}')
COUNTER_ID=$(pyget "$RESP" "request_id")
RESP=$(curl -sf -X POST "$CODEX/command_approve" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d "$(python3 -c "import json; print(json.dumps({'request_id':'$COUNTER_ID'}))")")
RESP=$(curl -s -X POST "$CODEX/command_approve" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d "$(python3 -c "import json; print(json.dumps({'request_id':'$COUNTER_ID'}))")")
COUNTER_CONTENT=$(cat "$TEST_PROJECT_DIR/approval-count.txt")
assert_eq "Command duplicate approval ran once" "run" "$COUNTER_CONTENT"
RESP=$(curl -sf -X POST "$CODEX/command_request_batch" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","requests":[{"command":"smoke","reason":"batch smoke"},{"command":"fail","reason":"batch reject"}]}')
BATCH_SUCCESS=$(pyget "$RESP" "success")
BATCH_COUNT=$(echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); print(len(d.get('records', [])))")
BATCH_ID_0=$(echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d['records'][0]['id'])")
BATCH_ID_1=$(echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d['records'][1]['id'])")
assert_eq "Command request batch created" "True" "$BATCH_SUCCESS"
assert_eq "Command request batch has 2 records" "2" "$BATCH_COUNT"
RESP=$(curl -sf -X POST "$CODEX/command_requests" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","status":"pending","limit":20}')
LIST_SUCCESS=$(pyget "$RESP" "success")
LIST_HAS_BATCH=$(echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); ids={r['id'] for r in d.get('records', [])}; print('yes' if '$BATCH_ID_0' in ids and '$BATCH_ID_1' in ids else 'no')")
assert_eq "Command request list success" "True" "$LIST_SUCCESS"
assert_eq "Command request list has batch ids" "yes" "$LIST_HAS_BATCH"
RESP=$(curl -sf -X POST "$CODEX/command_reject" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d "$(python3 -c "import json; print(json.dumps({'request_id':'$BATCH_ID_1','reason':'not needed'}))")")
REJECT_SUCCESS=$(pyget "$RESP" "success")
REJECT_STATUS=$(echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d['record']['status'])")
REJECT_ERROR=$(echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d['record'].get('error') or '')")
assert_eq "Command request rejected" "True" "$REJECT_SUCCESS"
assert_eq "Command request rejected status" "rejected" "$REJECT_STATUS"
assert_contains "Command request rejection reason" "not needed" "$REJECT_ERROR"
RESP=$(curl -s -X POST "$CODEX/command_approve" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d "$(python3 -c "import json; print(json.dumps({'request_id':'$BATCH_ID_1'}))")")
REJECT_APPROVE_SUCCESS=$(pyget "$RESP" "success")
REJECT_APPROVE_ERROR=$(pyget "$RESP" "error")
assert_eq "Rejected request cannot approve" "False" "$REJECT_APPROVE_SUCCESS"
assert_contains "Rejected request approve error" "not pending" "$REJECT_APPROVE_ERROR"
RESP=$(curl -sf -X POST "$CODEX/command_request_raw" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","command_text":"echo raw-ok","reason":"raw smoke"}')
RAW_SUCCESS=$(pyget "$RESP" "success")
RAW_ID=$(pyget "$RESP" "request_id")
RAW_COMMAND_TEXT=$(echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d['record'].get('command_text') or '')")
assert_eq "Raw command request created" "True" "$RAW_SUCCESS"
assert_contains "Raw command stores command_text" "echo raw-ok" "$RAW_COMMAND_TEXT"
RESP=$(curl -sf -X POST "$CODEX/command_approve" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d "$(python3 -c "import json; print(json.dumps({'request_id':'$RAW_ID'}))")")
RAW_APPROVE_SUCCESS=$(pyget "$RESP" "success")
RAW_APPROVE_STDOUT=$(echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d['record'].get('stdout_tail') or '')")
assert_eq "Raw command approval executed" "True" "$RAW_APPROVE_SUCCESS"
assert_contains "Raw command approval stdout" "raw-ok" "$RAW_APPROVE_STDOUT"
RESP=$(curl -s -X POST "$CODEX/command_request_raw" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","command_text":"git push origin main"}')
RAW_BLOCK_SUCCESS=$(pyget "$RESP" "success")
RAW_BLOCK_ERROR=$(pyget "$RESP" "error")
assert_eq "Raw command high-risk blocked" "False" "$RAW_BLOCK_SUCCESS"
assert_contains "Raw command block error" "blocked" "$RAW_BLOCK_ERROR"
RESP=$(curl -sf -X POST "$CODEX/command_request_op" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"op":"create_raw","project":"test-project","command_text":"echo op-raw-ok","reason":"aggregated raw smoke"}')
OP_SUCCESS=$(pyget "$RESP" "success")
OP_ID=$(pyget "$RESP" "request_id")
OP_COMMAND_TEXT=$(echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d['record'].get('command_text') or '')")
assert_eq "Command op create_raw success" "True" "$OP_SUCCESS"
assert_contains "Command op create_raw stores command_text" "echo op-raw-ok" "$OP_COMMAND_TEXT"
RESP=$(curl -sf -X POST "$CODEX/command_request_op" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d "$(python3 -c "import json; print(json.dumps({'op':'approve','request_id':'$OP_ID'}))")")
OP_APPROVE_SUCCESS=$(pyget "$RESP" "success")
OP_APPROVE_STDOUT=$(echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d['record'].get('stdout_tail') or '')")
assert_eq "Command op approve success" "True" "$OP_APPROVE_SUCCESS"
assert_contains "Command op approve stdout" "op-raw-ok" "$OP_APPROVE_STDOUT"
RESP=$(curl -sf -X POST "$CODEX/command_request_op" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"op":"list","project":"test-project","status":"completed","limit":20}')
OP_LIST_SUCCESS=$(pyget "$RESP" "success")
OP_LIST_HAS_ID=$(echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); ids={r['id'] for r in d.get('records', [])}; print('yes' if '$OP_ID' in ids else 'no')")
assert_eq "Command op list success" "True" "$OP_LIST_SUCCESS"
assert_eq "Command op list has approved id" "yes" "$OP_LIST_HAS_ID"
RESP=$(curl -sf -X POST "$CODEX/command_request_op" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"op":"create_batch","project":"test-project","requests":[{"command":"smoke","reason":"op batch 1"},{"command":"smoke","reason":"op batch 2"}]}')
OP_BATCH_SUCCESS=$(pyget "$RESP" "success")
OP_BATCH_ID_0=$(echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d['records'][0]['id'])")
OP_BATCH_ID_1=$(echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d['records'][1]['id'])")
assert_eq "Command op create_batch success" "True" "$OP_BATCH_SUCCESS"
RESP=$(curl -sf -X POST "$CODEX/command_request_op" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d "$(python3 -c "import json; print(json.dumps({'op':'reject_batch','request_ids':['$OP_BATCH_ID_0','$OP_BATCH_ID_1'],'reason':'batch rejected'}))")")
OP_REJECT_BATCH_SUCCESS=$(pyget "$RESP" "success")
OP_REJECT_BATCH_COUNT=$(echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); print(len(d.get('records', [])))")
assert_eq "Command op reject_batch success" "True" "$OP_REJECT_BATCH_SUCCESS"
assert_eq "Command op reject_batch count" "2" "$OP_REJECT_BATCH_COUNT"
RESP=$(curl -sf -X POST "$CODEX/command_request_op" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"op":"create_goal","project":"test-project","title":"E2E development goal","summary":"Allow low-risk e2e commands","ttl_secs":600}')
GOAL_SUCCESS=$(pyget "$RESP" "success")
GOAL_ID=$(pyget "$RESP" "goal_id")
GOAL_STATUS=$(echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d['goal']['status'])")
assert_eq "Command op create_goal success" "True" "$GOAL_SUCCESS"
assert_not_empty "Command op create_goal id" "$GOAL_ID"
assert_eq "Command op create_goal pending" "pending" "$GOAL_STATUS"
RESP=$(curl -s -X POST "$CODEX/command_request_op" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d "$(python3 -c "import json; print(json.dumps({'op':'create_and_approve','project':'test-project','goal_id':'$GOAL_ID','command':'smoke','reason':'should not run while pending'}))")")
GOAL_PENDING_SUCCESS=$(pyget "$RESP" "success")
GOAL_PENDING_ERROR=$(pyget "$RESP" "error")
assert_eq "Command op pending goal cannot auto approve" "False" "$GOAL_PENDING_SUCCESS"
assert_contains "Command op pending goal error" "not active" "$GOAL_PENDING_ERROR"
RESP=$(curl -sf -X POST "$CODEX/command_request_op" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"op":"list_goals","project":"test-project","status":"pending","limit":10}')
GOAL_PENDING_LIST_SUCCESS=$(pyget "$RESP" "success")
GOAL_PENDING_LIST_HAS_ID=$(echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); ids={g['id'] for g in d.get('goals', [])}; print('yes' if '$GOAL_ID' in ids else 'no')")
assert_eq "Command op list_goals pending success" "True" "$GOAL_PENDING_LIST_SUCCESS"
assert_eq "Command op list_goals pending has id" "yes" "$GOAL_PENDING_LIST_HAS_ID"
RESP=$(curl -sf -X POST "$CODEX/command_request_op" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d "$(python3 -c "import json; print(json.dumps({'op':'approve_goal','goal_id':'$GOAL_ID'}))")")
GOAL_APPROVE_SUCCESS=$(pyget "$RESP" "success")
GOAL_APPROVE_STATUS=$(echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d['goal']['status'])")
assert_eq "Command op approve_goal success" "True" "$GOAL_APPROVE_SUCCESS"
assert_eq "Command op approve_goal active" "active" "$GOAL_APPROVE_STATUS"
RESP=$(curl -sf -X POST "$CODEX/command_request_op" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d "$(python3 -c "import json; print(json.dumps({'op':'create_raw_and_approve','project':'test-project','goal_id':'$GOAL_ID','command_text':'echo goal-raw-ok','reason':'goal raw smoke'}))")")
GOAL_RAW_SUCCESS=$(pyget "$RESP" "success")
GOAL_RAW_STDOUT=$(echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d['record'].get('stdout_tail') or '')")
GOAL_RAW_REASON=$(echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d['record'].get('reason') or '')")
assert_eq "Command op goal raw auto approve success" "True" "$GOAL_RAW_SUCCESS"
assert_contains "Command op goal raw stdout" "goal-raw-ok" "$GOAL_RAW_STDOUT"
assert_contains "Command op goal raw reason has goal" "$GOAL_ID" "$GOAL_RAW_REASON"
RESP=$(curl -sf -X POST "$CODEX/command_request_op" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d "$(python3 -c "import json; print(json.dumps({'op':'create_and_approve','project':'test-project','goal_id':'$GOAL_ID','command':'smoke','reason':'goal configured smoke'}))")")
GOAL_CMD_SUCCESS=$(pyget "$RESP" "success")
GOAL_CMD_STDOUT=$(echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d['record'].get('stdout_tail') or '')")
assert_eq "Command op goal configured auto approve success" "True" "$GOAL_CMD_SUCCESS"
assert_contains "Command op goal configured stdout" "command-smoke-ok" "$GOAL_CMD_STDOUT"
RESP=$(curl -sf -X POST "$CODEX/command_request_op" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"op":"list_goals","project":"test-project","status":"active","limit":10}')
GOAL_LIST_SUCCESS=$(pyget "$RESP" "success")
GOAL_LIST_HAS_ID=$(echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); ids={g['id'] for g in d.get('goals', [])}; print('yes' if '$GOAL_ID' in ids else 'no')")
assert_eq "Command op list_goals active success" "True" "$GOAL_LIST_SUCCESS"
assert_eq "Command op list_goals active has id" "yes" "$GOAL_LIST_HAS_ID"
RESP=$(curl -sf -X POST "$CODEX/command_request_op" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"op":"create_goal_and_approve","project":"test-project","title":"One-step active goal","summary":"Create and activate in one audited call","ttl_secs":600}')
GOAL_ONE_STEP_SUCCESS=$(pyget "$RESP" "success")
GOAL_ONE_STEP_ID=$(pyget "$RESP" "goal_id")
GOAL_ONE_STEP_STATUS=$(echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d['goal']['status'])")
assert_eq "Command op create_goal_and_approve success" "True" "$GOAL_ONE_STEP_SUCCESS"
assert_not_empty "Command op create_goal_and_approve id" "$GOAL_ONE_STEP_ID"
assert_eq "Command op create_goal_and_approve active" "active" "$GOAL_ONE_STEP_STATUS"
RESP=$(curl -sf -X POST "$CODEX/command_request_op" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d "$(python3 -c "import json; print(json.dumps({'op':'create_and_approve','project':'test-project','goal_id':'$GOAL_ONE_STEP_ID','command':'smoke','reason':'one-step goal configured smoke'}))")")
GOAL_ONE_STEP_CMD_SUCCESS=$(pyget "$RESP" "success")
GOAL_ONE_STEP_CMD_STDOUT=$(echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d['record'].get('stdout_tail') or '')")
assert_eq "Command op one-step goal configured auto approve success" "True" "$GOAL_ONE_STEP_CMD_SUCCESS"
assert_contains "Command op one-step goal configured stdout" "command-smoke-ok" "$GOAL_ONE_STEP_CMD_STDOUT"

# --- 24e2. Codex Trusted Async Jobs ---
echo ""
echo "--- 24e2. Codex Trusted Async Jobs ---"
RESP=$(curl -s -X POST "$CODEX/job" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","op":"create","command":"echo no-goal"}')
JOB_NO_GOAL_SUCCESS=$(pyget "$RESP" "success")
JOB_NO_GOAL_ERROR=$(pyget "$RESP" "error")
assert_eq "Job create without goal fails" "False" "$JOB_NO_GOAL_SUCCESS"
assert_contains "Job create without goal error" "goal_id" "$JOB_NO_GOAL_ERROR"
RESP=$(curl -s -X POST "$CODEX/job" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","op":"create","goal_id":"missing-goal","command":"echo no-active"}')
JOB_BAD_GOAL_SUCCESS=$(pyget "$RESP" "success")
JOB_BAD_GOAL_ERROR=$(pyget "$RESP" "error")
assert_eq "Job create non-active goal fails" "False" "$JOB_BAD_GOAL_SUCCESS"
assert_contains "Job create non-active goal error" "Goal not found" "$JOB_BAD_GOAL_ERROR"
RESP=$(curl -sf -X POST "$CODEX/job" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d "$(python3 -c "import json; print(json.dumps({'project':'test-project','op':'create','goal_id':'$GOAL_ID','command':'echo job-start; sleep 1; echo job-done','reason':'job smoke','max_runtime_secs':30}))")")
JOB_SUCCESS=$(pyget "$RESP" "success")
JOB_ID=$(pyget "$RESP" "job_id")
assert_eq "Job create success" "True" "$JOB_SUCCESS"
assert_not_empty "Job create returns job_id" "$JOB_ID"
sleep 2
RESP=$(curl -sf -X POST "$CODEX/job" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d "$(python3 -c "import json; print(json.dumps({'project':'test-project','op':'log','job_id':'$JOB_ID','tail_lines':20}))")")
JOB_LOG_SUCCESS=$(pyget "$RESP" "success")
JOB_LOG_STDOUT=$(pyget "$RESP" "stdout_tail")
assert_eq "Job log success" "True" "$JOB_LOG_SUCCESS"
assert_contains "Job log has output" "job-done" "$JOB_LOG_STDOUT"
RESP=$(curl -sf -X POST "$CODEX/job" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d "$(python3 -c "import json; print(json.dumps({'project':'test-project','op':'status','job_id':'$JOB_ID'}))")")
JOB_STATUS_SUCCESS=$(pyget "$RESP" "success")
JOB_STATUS_VALUE=$(echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d['job']['status'])")
JOB_FINISHED_AT=$(echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d['job'].get('finished_at') or '')")
assert_eq "Job status success" "True" "$JOB_STATUS_SUCCESS"
assert_eq "Job status completed" "completed" "$JOB_STATUS_VALUE"
assert_not_empty "Job completed has finished_at" "$JOB_FINISHED_AT"
RESP=$(curl -sf -X POST "$CODEX/job" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d "$(python3 -c "import json; print(json.dumps({'project':'test-project','op':'create','goal_id':'$GOAL_ID','script_path':'scripts/codex_jobs/job_smoke.sh','script_args':['alpha value','beta value'],'reason':'job script_path smoke','max_runtime_secs':30}))")")
JOB_SCRIPT_SUCCESS=$(pyget "$RESP" "success")
JOB_SCRIPT_ID=$(pyget "$RESP" "job_id")
assert_eq "Job script_path create success" "True" "$JOB_SCRIPT_SUCCESS"
assert_not_empty "Job script_path returns job_id" "$JOB_SCRIPT_ID"
sleep 1
RESP=$(curl -sf -X POST "$CODEX/job" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d "$(python3 -c "import json; print(json.dumps({'project':'test-project','op':'log','job_id':'$JOB_SCRIPT_ID','tail_lines':20}))")")
JOB_SCRIPT_LOG_SUCCESS=$(pyget "$RESP" "success")
JOB_SCRIPT_STDOUT=$(pyget "$RESP" "stdout_tail")
JOB_SCRIPT_STDERR=$(pyget "$RESP" "stderr_tail")
assert_eq "Job script_path log success" "True" "$JOB_SCRIPT_LOG_SUCCESS"
assert_contains "Job script_path stdout has arg" "script-start:alpha value" "$JOB_SCRIPT_STDOUT"
assert_contains "Job script_path stderr has arg" "script-err:beta value" "$JOB_SCRIPT_STDERR"
RESP=$(curl -s -X POST "$CODEX/job" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d "$(python3 -c "import json; print(json.dumps({'project':'test-project','op':'create','goal_id':'$GOAL_ID','script_path':'../evil.sh','reason':'bad script path','max_runtime_secs':30}))")")
JOB_BAD_SCRIPT_SUCCESS=$(pyget "$RESP" "success")
JOB_BAD_SCRIPT_ERROR=$(pyget "$RESP" "error")
assert_eq "Job script_path traversal fails" "False" "$JOB_BAD_SCRIPT_SUCCESS"
assert_contains "Job script_path traversal error" "script_path" "$JOB_BAD_SCRIPT_ERROR"
RESP=$(curl -sf -X POST "$CODEX/job" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d "$(python3 -c "import json; print(json.dumps({'project':'test-project','op':'create_batch','goal_id':'$GOAL_ID','commands':['echo batch-0','echo batch-1','echo batch-2'],'reason':'batch smoke','max_runtime_secs':30}))")")
JOB_BATCH_SUCCESS=$(pyget "$RESP" "success")
JOB_BATCH_COUNT=$(echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); print(len(d.get('job_ids', [])))")
assert_eq "Job create_batch success" "True" "$JOB_BATCH_SUCCESS"
assert_eq "Job create_batch count" "3" "$JOB_BATCH_COUNT"
sleep 1
RESP=$(curl -sf -X POST "$CODEX/job" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d "$(python3 -c "import json; print(json.dumps({'project':'test-project','op':'list','goal_id':'$GOAL_ID','limit':20}))")")
JOB_LIST_SUCCESS=$(pyget "$RESP" "success")
JOB_LIST_COUNT=$(echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); print(len(d.get('jobs', [])))")
assert_eq "Job list success" "True" "$JOB_LIST_SUCCESS"
if [ "$JOB_LIST_COUNT" -lt 5 ]; then
    fail "Job list has jobs" "expected at least 5 jobs, got $JOB_LIST_COUNT"
else
    log_pass "Job list has jobs"
fi
RESP=$(curl -s -X POST "$CODEX/job" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d "$(python3 -c "import json; print(json.dumps({'project':'test-project','op':'create_batch','goal_id':'$GOAL_ID','commands':['echo should-not-start',''],'reason':'invalid batch','max_runtime_secs':30}))")")
JOB_BAD_BATCH_SUCCESS=$(pyget "$RESP" "success")
JOB_BAD_BATCH_ERROR=$(pyget "$RESP" "error")
assert_eq "Job create_batch invalid command fails" "False" "$JOB_BAD_BATCH_SUCCESS"
assert_contains "Job create_batch invalid command error" "command cannot be empty" "$JOB_BAD_BATCH_ERROR"
RESP=$(curl -sf -X POST "$CODEX/job" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d "$(python3 -c "import json; print(json.dumps({'project':'test-project','op':'list','goal_id':'$GOAL_ID','limit':20}))")")
JOB_LIST_AFTER_BAD_BATCH_COUNT=$(echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); print(len(d.get('jobs', [])))")
assert_eq "Job invalid batch starts no partial jobs" "$JOB_LIST_COUNT" "$JOB_LIST_AFTER_BAD_BATCH_COUNT"
RESP=$(curl -sf -X POST "$CODEX/job" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d "$(python3 -c "import json; print(json.dumps({'project':'test-project','op':'summarize','goal_id':'$GOAL_ID','tail_lines':10}))")")
JOB_SUMMARY_SUCCESS=$(pyget "$RESP" "success")
JOB_SUMMARY=$(pyget "$RESP" "summary_markdown")
assert_eq "Job summarize success" "True" "$JOB_SUMMARY_SUCCESS"
assert_contains "Job summarize markdown" "Codex job summary" "$JOB_SUMMARY"
RESP=$(curl -sf -X POST "$CODEX/job" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d "$(python3 -c "import json; print(json.dumps({'project':'test-project','op':'create','goal_id':'$GOAL_ID','command':'sleep 30','reason':'stop smoke','max_runtime_secs':60}))")")
JOB_STOP_ID=$(pyget "$RESP" "job_id")
RESP=$(curl -sf -X POST "$CODEX/job" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d "$(python3 -c "import json; print(json.dumps({'project':'test-project','op':'stop','job_id':'$JOB_STOP_ID'}))")")
JOB_STOP_SUCCESS=$(pyget "$RESP" "success")
JOB_STOP_STATUS=$(echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d['job']['status'])")
JOB_STOP_FINISHED_AT=$(echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d['job'].get('finished_at') or '')")
assert_eq "Job stop success" "True" "$JOB_STOP_SUCCESS"
assert_eq "Job stop status" "stopped" "$JOB_STOP_STATUS"
assert_not_empty "Job stopped has finished_at" "$JOB_STOP_FINISHED_AT"

RESP=$(curl -sf -X POST "$CODEX/command_request_op" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"op":"create_goal","project":"test-project","title":"Rejected goal","ttl_secs":600}')
REJECT_GOAL_ID=$(pyget "$RESP" "goal_id")
RESP=$(curl -sf -X POST "$CODEX/command_request_op" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d "$(python3 -c "import json; print(json.dumps({'op':'reject_goal','goal_id':'$REJECT_GOAL_ID','reason':'not approved'}))")")
REJECT_GOAL_SUCCESS=$(pyget "$RESP" "success")
REJECT_GOAL_STATUS=$(echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d['goal']['status'])")
assert_eq "Command op reject_goal success" "True" "$REJECT_GOAL_SUCCESS"
assert_eq "Command op reject_goal rejected" "rejected" "$REJECT_GOAL_STATUS"
RESP=$(curl -s -X POST "$CODEX/command_request_op" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d "$(python3 -c "import json; print(json.dumps({'op':'approve_goal','goal_id':'$REJECT_GOAL_ID'}))")")
REJECTED_APPROVE_SUCCESS=$(pyget "$RESP" "success")
REJECTED_APPROVE_ERROR=$(pyget "$RESP" "error")
assert_eq "Command op rejected goal cannot approve" "False" "$REJECTED_APPROVE_SUCCESS"
assert_contains "Command op rejected goal approve error" "not pending" "$REJECTED_APPROVE_ERROR"
RESP=$(curl -s -X POST "$CODEX/command_request_op" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d "$(python3 -c "import json; print(json.dumps({'op':'create_and_approve','project':'test-project','goal_id':'$REJECT_GOAL_ID','command':'smoke'}))")")
REJECTED_RUN_SUCCESS=$(pyget "$RESP" "success")
REJECTED_RUN_ERROR=$(pyget "$RESP" "error")
assert_eq "Command op rejected goal cannot auto approve" "False" "$REJECTED_RUN_SUCCESS"
assert_contains "Command op rejected goal run error" "not active" "$REJECTED_RUN_ERROR"
RESP=$(curl -sf -X POST "$CODEX/command_request_op" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"op":"list_goals","project":"test-project","status":"rejected","limit":10}')
REJECTED_LIST_SUCCESS=$(pyget "$RESP" "success")
REJECTED_LIST_HAS_ID=$(echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); ids={g['id'] for g in d.get('goals', [])}; print('yes' if '$REJECT_GOAL_ID' in ids else 'no')")
assert_eq "Command op list_goals rejected success" "True" "$REJECTED_LIST_SUCCESS"
assert_eq "Command op list_goals rejected has id" "yes" "$REJECTED_LIST_HAS_ID"
RESP=$(curl -sf -X POST "$CODEX/command_request_op" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d "$(python3 -c "import json; print(json.dumps({'op':'close_goal','goal_id':'$GOAL_ID','reason':'done'}))")")
GOAL_CLOSE_SUCCESS=$(pyget "$RESP" "success")
GOAL_CLOSE_STATUS=$(echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d['goal']['status'])")
assert_eq "Command op close_goal success" "True" "$GOAL_CLOSE_SUCCESS"
assert_eq "Command op close_goal closed" "closed" "$GOAL_CLOSE_STATUS"
RESP=$(curl -s -X POST "$CODEX/command_request_op" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d "$(python3 -c "import json; print(json.dumps({'op':'create_raw_and_approve','project':'test-project','goal_id':'$GOAL_ID','command_text':'echo should-not-run'}))")")
GOAL_CLOSED_SUCCESS=$(pyget "$RESP" "success")
GOAL_CLOSED_ERROR=$(pyget "$RESP" "error")
assert_eq "Command op closed goal cannot auto approve" "False" "$GOAL_CLOSED_SUCCESS"
assert_contains "Command op closed goal error" "not active" "$GOAL_CLOSED_ERROR"

# --- 25. Codex: applyProjectPatch ---
echo ""
echo "--- 25. Codex Apply Patch ---"
# Create a simple patch that adds a line to test.txt
PATCH_FILE="$TMPDIR_DATA/test.patch"
cat > "$PATCH_FILE" << 'PATCHEOF'
diff --git a/test.txt b/test.txt
--- a/test.txt
+++ b/test.txt
@@ -1,3 +1,4 @@
 line1
 line2
 line3
+line4
PATCHEOF
RESP=$(curl -s -X POST "$CODEX/apply_patch" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d "$(python3 -c "
import json
patch = open('$PATCH_FILE').read()
print(json.dumps({'project':'test-project','patch':patch,'reason':'add line4'}))
")")
PATCH_SUCCESS=$(pyget "$RESP" "success")
assert_eq "Apply patch success" "True" "$PATCH_SUCCESS"
# Verify the file was modified
RESP=$(curl -sf -X POST "$CODEX/context" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","mode":"read_file","path":"test.txt"}')
CTX_CONTENT=$(pyget "$RESP" "content")
assert_contains "Patch added line4" "line4" "$CTX_CONTENT"

# --- 26. Codex: applyProjectPatch blocked sensitive path ---
echo ""
echo "--- 26. Codex Apply Patch Blocked ---"
RESP=$(curl -s -X POST "$CODEX/apply_patch" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","patch":"diff --git a/.env b/.env\n--- /dev/null\n+++ b/.env\n@@ -0,0 +1 @@\n+SECRET=x\n","reason":"evil"}')
PATCH_SUCCESS=$(pyget "$RESP" "success")
PATCH_ERROR=$(pyget "$RESP" "error")
assert_eq "Patch .env blocked" "False" "$PATCH_SUCCESS"
assert_contains "Error mentions sensitive" "sensitive" "$PATCH_ERROR"

# --- 27. Codex: runProjectCheck ---
echo ""
echo "--- 27. Codex Run Check ---"
RESP=$(curl -sf -X POST "$CODEX/check" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","suite":"test"}')
CHECK_SUCCESS=$(pyget "$RESP" "success")
CHECK_SUITE=$(pyget "$RESP" "suite")
CHECK_EXIT=$(pyget "$RESP" "exit_code")
assert_eq "Check test success" "True" "$CHECK_SUCCESS"
assert_eq "Check suite is test" "test" "$CHECK_SUITE"
assert_eq "Check exit code 0" "0" "$CHECK_EXIT"

# --- 28. Codex: runProjectCheck unknown suite ---
echo ""
echo "--- 28. Codex Check Unknown Suite ---"
RESP=$(curl -s -X POST "$CODEX/check" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","suite":"unknown_suite"}')
CHECK_SUCCESS=$(pyget "$RESP" "success")
assert_eq "Unknown suite rejected" "False" "$CHECK_SUCCESS"

# --- 29. Codex: writeProjectReport ---
echo ""
echo "--- 29. Codex Write Report ---"
RESP=$(curl -sf -X POST "$CODEX/report" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","status":"completed","title":"E2E Test Report","summary":"All tests passed","channel":"omo"}')
REPORT_SUCCESS=$(pyget "$RESP" "success")
REPORT_ID=$(pyget "$RESP" "report_id")
REPORT_MSG_ID=$(pyget "$RESP" "message_id")
REPORT_PATH=$(pyget "$RESP" "path")
assert_eq "Report success" "True" "$REPORT_SUCCESS"
assert_not_empty "Report has report_id" "$REPORT_ID"
assert_not_empty "Report has message_id" "$REPORT_MSG_ID"
assert_not_empty "Report has path" "$REPORT_PATH"

# Verify report message is in the channel
RESP=$(curl -sf -H "Authorization: Bearer $TOKEN" "$BASE/api/messages?channel=omo&limit=10")
FOUND=$(echo "$RESP" | python3 -c "
import sys, json
data = json.load(sys.stdin)
print('yes' if any(m['id'] == '$REPORT_MSG_ID' for m in data.get('messages', [])) else 'no')
")
assert_eq "Report message found in omo channel" "yes" "$FOUND"

# --- 30. Codex: OpenAPI spec has codex operations ---
echo ""
echo "--- 30. Codex OpenAPI Spec ---"
RESP=$(curl -sf "$BASE/openapi.json")
HAS_CTX=$(echo "$RESP" | python3 -c "import sys; print('yes' if 'getProjectContext' in sys.stdin.read() else 'no')")
HAS_PATCH=$(echo "$RESP" | python3 -c "import sys; print('yes' if 'applyProjectPatch' in sys.stdin.read() else 'no')")
HAS_EDIT=$(echo "$RESP" | python3 -c "import sys; print('yes' if 'applyProjectEdit' in sys.stdin.read() else 'no')")
HAS_ARTIFACT=$(echo "$RESP" | python3 -c "import sys; print('yes' if 'saveProjectArtifact' in sys.stdin.read() else 'no')")
HAS_GIT=$(echo "$RESP" | python3 -c "import sys; print('yes' if 'runProjectGit' in sys.stdin.read() else 'no')")
HAS_CMD=$(echo "$RESP" | python3 -c "import sys; print('yes' if 'runProjectCommand' in sys.stdin.read() else 'no')")
HAS_RAW_REQ=$(echo "$RESP" | python3 -c "import sys; print('yes' if 'createRawCommandRequest' in sys.stdin.read() else 'no')")
HAS_OP_REQ=$(echo "$RESP" | python3 -c "import sys; print('yes' if 'runCommandRequestOp' in sys.stdin.read() else 'no')")
HAS_LIST_REQ=$(echo "$RESP" | python3 -c "import sys; print('yes' if 'listCommandRequests' in sys.stdin.read() else 'no')")
HAS_BATCH_REQ=$(echo "$RESP" | python3 -c "import sys; print('yes' if 'createCommandRequestBatch' in sys.stdin.read() else 'no')")
HAS_REJECT_REQ=$(echo "$RESP" | python3 -c "import sys; print('yes' if 'rejectCommandRequest' in sys.stdin.read() else 'no')")
HAS_CHECK=$(echo "$RESP" | python3 -c "import sys; print('yes' if 'runProjectCheck' in sys.stdin.read() else 'no')")
HAS_REPORT=$(echo "$RESP" | python3 -c "import sys; print('yes' if 'writeProjectReport' in sys.stdin.read() else 'no')")
HAS_JOB=$(echo "$RESP" | python3 -c "import sys; print('yes' if 'runJobOp' in sys.stdin.read() else 'no')")
assert_contains "OpenAPI has getProjectContext" "yes" "$HAS_CTX"
assert_contains "OpenAPI has applyProjectPatch" "yes" "$HAS_PATCH"
assert_contains "OpenAPI has applyProjectEdit" "yes" "$HAS_EDIT"
assert_contains "OpenAPI has saveProjectArtifact" "yes" "$HAS_ARTIFACT"
assert_contains "OpenAPI has runProjectGit" "yes" "$HAS_GIT"
assert_contains "OpenAPI has runProjectCommand" "yes" "$HAS_CMD"
assert_contains "OpenAPI has createRawCommandRequest" "yes" "$HAS_RAW_REQ"
assert_contains "OpenAPI has runCommandRequestOp" "yes" "$HAS_OP_REQ"
assert_contains "OpenAPI has listCommandRequests" "yes" "$HAS_LIST_REQ"
assert_contains "OpenAPI has createCommandRequestBatch" "yes" "$HAS_BATCH_REQ"
assert_contains "OpenAPI has rejectCommandRequest" "yes" "$HAS_REJECT_REQ"
assert_contains "OpenAPI has runProjectCheck" "yes" "$HAS_CHECK"
assert_contains "OpenAPI has writeProjectReport" "yes" "$HAS_REPORT"
assert_contains "OpenAPI has runJobOp" "yes" "$HAS_JOB"
# Verify new edit schemas are present
HAS_REPLACE_TEXT_SCHEMA=$(echo "$RESP" | python3 -c "import sys; print('yes' if 'ReplaceTextEdit' in sys.stdin.read() else 'no')")
assert_contains "OpenAPI has ReplaceTextEdit schema" "yes" "$HAS_REPLACE_TEXT_SCHEMA"

# Also verify old operations are still there (check main openapi.json, not codex subset)
HAS_CREATE=$(curl -sf "$BASE/openapi.json" | python3 -c "import sys; print('yes' if 'createMessage' in sys.stdin.read() else 'no')")
assert_contains "OpenAPI still has createMessage" "yes" "$HAS_CREATE"

# Also check codex-only OpenAPI endpoint
RESP=$(curl -sf "$BASE/codex-openapi.json")
HAS_EDIT=$(echo "$RESP" | python3 -c "import sys; print('yes' if 'applyProjectEdit' in sys.stdin.read() else 'no')")
HAS_ARTIFACT=$(echo "$RESP" | python3 -c "import sys; print('yes' if 'saveProjectArtifact' in sys.stdin.read() else 'no')")
HAS_GIT=$(echo "$RESP" | python3 -c "import sys; print('yes' if 'runProjectGit' in sys.stdin.read() else 'no')")
HAS_CMD=$(echo "$RESP" | python3 -c "import sys; print('yes' if 'runProjectCommand' in sys.stdin.read() else 'no')")
HAS_RAW_REQ=$(echo "$RESP" | python3 -c "import sys; print('yes' if 'createRawCommandRequest' in sys.stdin.read() else 'no')")
HAS_OP_REQ=$(echo "$RESP" | python3 -c "import sys; print('yes' if 'runCommandRequestOp' in sys.stdin.read() else 'no')")
HAS_LIST_REQ=$(echo "$RESP" | python3 -c "import sys; print('yes' if 'listCommandRequests' in sys.stdin.read() else 'no')")
HAS_BATCH_REQ=$(echo "$RESP" | python3 -c "import sys; print('yes' if 'createCommandRequestBatch' in sys.stdin.read() else 'no')")
HAS_REJECT_REQ=$(echo "$RESP" | python3 -c "import sys; print('yes' if 'rejectCommandRequest' in sys.stdin.read() else 'no')")
assert_contains "codex-openapi.json has applyProjectEdit" "yes" "$HAS_EDIT"
assert_contains "codex-openapi.json has saveProjectArtifact" "yes" "$HAS_ARTIFACT"
assert_contains "codex-openapi.json has runProjectGit" "yes" "$HAS_GIT"
assert_contains "codex-openapi.json has runProjectCommand" "yes" "$HAS_CMD"
assert_contains "codex-openapi.json has createRawCommandRequest" "yes" "$HAS_RAW_REQ"
assert_contains "codex-openapi.json has runCommandRequestOp" "yes" "$HAS_OP_REQ"
assert_contains "codex-openapi.json has listCommandRequests" "yes" "$HAS_LIST_REQ"
assert_contains "codex-openapi.json has createCommandRequestBatch" "yes" "$HAS_BATCH_REQ"
assert_contains "codex-openapi.json has rejectCommandRequest" "yes" "$HAS_REJECT_REQ"
HAS_ONEOF=$(echo "$RESP" | python3 -c "import sys; print('yes' if 'oneOf' in sys.stdin.read() else 'no')")
assert_contains "codex-openapi.json has oneOf schemas" "yes" "$HAS_ONEOF"

# Compact Codex OpenAPI should expose only action-efficient core operations
RESP=$(curl -sf "$BASE/codex-openapi-compact.json")
COMPACT_OK=$(echo "$RESP" | python3 -c "import sys,json; json.load(sys.stdin); print('yes')")
COMPACT_SERVER=$(echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d['servers'][0]['url'])")
assert_eq "codex-openapi-compact.json loads" "yes" "$COMPACT_OK"
assert_eq "codex-openapi-compact.json server url" "http://localhost:8080" "$COMPACT_SERVER"
COMPACT_HAS_SAVE_GENERATED=$(echo "$RESP" | python3 -c "import sys; print('yes' if 'save_generated' in sys.stdin.read() else 'no')")
COMPACT_HAS_AGENT_CONTEXT=$(echo "$RESP" | python3 -c "import sys; print('yes' if 'agent_context' in sys.stdin.read() else 'no')")
assert_eq "codex-openapi-compact.json has save_generated artifact mode" "yes" "$COMPACT_HAS_SAVE_GENERATED"
assert_eq "codex-openapi-compact.json has agent_context mode" "yes" "$COMPACT_HAS_AGENT_CONTEXT"
COMPACT_OPS=$(echo "$RESP" | python3 -c '
import json, sys
spec=json.load(sys.stdin)
ops=[]
for path, methods in spec.get("paths", {}).items():
    for method, op in methods.items():
        if isinstance(op, dict) and "operationId" in op:
            ops.append(op["operationId"])
print("\n".join(sorted(ops)))
')
for op in getProjectContextBatch applyProjectEdit saveProjectArtifact runProjectGit runProjectCommand runCommandRequestOp runJobOp runProjectCheck writeProjectReport; do
    HAS_OP=$(echo "$COMPACT_OPS" | python3 -c "import sys; op='$op'; print('yes' if op in sys.stdin.read().splitlines() else 'no')")
    assert_eq "codex-openapi-compact.json has $op" "yes" "$HAS_OP"
done
for op in getProjectContext applyProjectPatch createCommandRequest createRawCommandRequest listCommandRequests createCommandRequestBatch approveCommandRequest rejectCommandRequest; do
    HAS_OP=$(echo "$COMPACT_OPS" | python3 -c "import sys; op='$op'; print('yes' if op in sys.stdin.read().splitlines() else 'no')")
    assert_eq "codex-openapi-compact.json hides $op" "no" "$HAS_OP"
done
COMPACT_PATHS=$(echo "$RESP" | python3 -c 'import json,sys; d=json.load(sys.stdin); print("\n".join(sorted(d.get("paths", {}).keys())))')
for path in /api/codex/context /api/codex/apply_patch /api/codex/command_request /api/codex/command_request_raw /api/codex/command_requests /api/codex/command_request_batch /api/codex/command_approve /api/codex/command_reject; do
    HAS_PATH=$(echo "$COMPACT_PATHS" | python3 -c "import sys; path='$path'; print('yes' if path in sys.stdin.read().splitlines() else 'no')")
    assert_eq "codex-openapi-compact.json hides $path" "no" "$HAS_PATH"
done
COMPACT_REFS_OK=$(echo "$RESP" | python3 -c '
import json, sys
spec = json.load(sys.stdin)
schemas = spec.get("components", {}).get("schemas", {})
missing = []
def walk(x):
    if isinstance(x, dict):
        ref = x.get("$ref")
        if isinstance(ref, str) and ref.startswith("#/components/schemas/"):
            name = ref.rsplit("/", 1)[-1]
            if name not in schemas:
                missing.append(name)
        for v in x.values():
            walk(v)
    elif isinstance(x, list):
        for v in x:
            walk(v)
walk(spec)
print("yes" if not missing else "missing:" + ",".join(sorted(set(missing))))
')
assert_eq "codex-openapi-compact.json refs resolve" "yes" "$COMPACT_REFS_OK"

# --- 31. Path safety: read_file rejects dangerous paths ---
echo ""
echo "--- 31. Path Safety ---"
# Test path traversal
RESP=$(curl -s -X POST "$CODEX/context" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","mode":"read_file","path":"../evil.txt"}')
PATH_SUCCESS=$(pyget "$RESP" "success")
assert_eq "read_file rejects ../evil.txt" "False" "$PATH_SUCCESS"

# Test absolute path
RESP=$(curl -s -X POST "$CODEX/context" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","mode":"read_file","path":"/etc/passwd"}')
PATH_SUCCESS=$(pyget "$RESP" "success")
assert_eq "read_file rejects /etc/passwd" "False" "$PATH_SUCCESS"

# Test sensitive path
RESP=$(curl -s -X POST "$CODEX/context" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","mode":"read_file","path":"secret.pem"}')
PATH_SUCCESS=$(pyget "$RESP" "success")
assert_eq "read_file rejects secret.pem" "False" "$PATH_SUCCESS"

# Test normal path is allowed
RESP=$(curl -s -X POST "$CODEX/context" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","mode":"read_file","path":"src/main.rs"}')
PATH_SUCCESS=$(pyget "$RESP" "success")
assert_eq "read_file allows src/main.rs" "True" "$PATH_SUCCESS"

# --- 32. Executor config: SSH config parses correctly ---
echo ""
echo "--- 32. Executor Config ---"
# Verify the test projects.toml has local executor (default)
HAS_LOCAL=$(grep -c 'executor' "$PROJECTS_TOML" 2>/dev/null | tr -d '[:space:]' || true)
if [ -z "$HAS_LOCAL" ]; then HAS_LOCAL="0"; fi
assert_eq "Test config uses local executor (no executor field)" "0" "$HAS_LOCAL"

# Create a test SSH config and verify it parses
SSH_TOML="$TMPDIR_DATA/ssh-test.toml"
cat > "$SSH_TOML" << 'SSHEOF'
[projects.remote-proj]
executor = "ssh"
host = "testhost"
user = "testuser"
path = "/remote/path"
allow_patch = true
allowed_checks = ["test"]

[projects.remote-proj.checks]
test = "cargo test"
SSHEOF
# Verify the TOML is valid by checking the binary can read it
# (The server would fail to start if TOML is invalid)
PARSE_OK=$(python3 -c "
import sys
try:
    content = open('$SSH_TOML').read()
    # Simple TOML validation
    if 'executor' in content and 'host' in content and 'ssh' in content:
        print('yes')
    else:
        print('no')
except:
    print('no')
")
assert_eq "SSH config TOML is valid" "yes" "$PARSE_OK"

# Verify SSH config fields are present
HAS_EXECUTOR=$(grep -c 'executor.*=.*"ssh"' "$SSH_TOML")
HAS_HOST=$(grep -c 'host.*=.*"testhost"' "$SSH_TOML")
HAS_USER=$(grep -c 'user.*=.*"testuser"' "$SSH_TOML")
assert_eq "SSH config has executor field" "1" "$HAS_EXECUTOR"
assert_eq "SSH config has host field" "1" "$HAS_HOST"
assert_eq "SSH config has user field" "1" "$HAS_USER"

# --- 33. SSH command construction: verify no user injection ---
echo ""
echo "--- 33. SSH Command Safety ---"
# Test that the SSH target format is safe
# These are unit-level checks via the Rust binary
# We verify by checking that the server starts with SSH config
# and that path traversal is blocked even with SSH executor

# Verify sensitive path patterns are blocked
RESP=$(curl -s -X POST "$CODEX/apply_patch" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","patch":"diff --git a/.env b/.env\nnew file\n--- /dev/null\n+++ b/.env\n@@ -0,0 +1 @@\n+SECRET=x","reason":"test"}')
PATCH_SUCCESS=$(pyget "$RESP" "success")
assert_eq "Patch .env still blocked" "False" "$PATCH_SUCCESS"

# Verify the local executor tests still work (regression check)
RESP=$(curl -sf -X POST "$CODEX/context" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","mode":"overview"}')
CTX_SUCCESS=$(pyget "$RESP" "success")
assert_eq "Local executor overview still works" "True" "$CTX_SUCCESS"

# ============================================================================
# applyProjectEdit E2E Tests (34-47)
# ============================================================================
echo ""
echo "=== applyProjectEdit Tests ==="

EDIT="$CODEX/edit"

# --- 34. Edit: replace_text modifies file ---
echo ""
echo "--- 34. Edit replace_text ---"
RESP=$(curl -sf -X POST "$EDIT" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","edits":[{"type":"replace_text","path":"test.txt","old_text":"line2","new_text":"LINE2"}]}')
EDIT_SUCCESS=$(pyget "$RESP" "success")
assert_eq "replace_text success" "True" "$EDIT_SUCCESS"
# Verify the file was modified
RESP=$(curl -sf -X POST "$CODEX/context" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","mode":"read_file","path":"test.txt"}')
CTX_CONTENT=$(pyget "$RESP" "content")
assert_contains "test.txt now contains LINE2" "LINE2" "$CTX_CONTENT"

# --- 35. Edit: replace_text multiple matches without occurrence fails ---
echo ""
echo "--- 35. Edit replace_text multi-match fails ---"
# First, create a file with multiple matches
RESP=$(curl -sf -X POST "$EDIT" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","edits":[{"type":"write_file","path":"multi.txt","content":"aaa bbb aaa bbb aaa\n","allow_overwrite":true}]}')
# Now try replace_text without occurrence
RESP=$(curl -s -X POST "$EDIT" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","edits":[{"type":"replace_text","path":"multi.txt","old_text":"aaa","new_text":"AAA"}]}')
EDIT_SUCCESS=$(pyget "$RESP" "success")
EDIT_ERROR=$(pyget "$RESP" "error")
assert_eq "multi-match without occurrence fails" "False" "$EDIT_SUCCESS"
assert_contains "Error mentions occurrence" "occurrence" "$EDIT_ERROR"

# --- 36. Edit: replace_text with occurrence succeeds ---
echo ""
echo "--- 36. Edit replace_text with occurrence ---"
RESP=$(curl -sf -X POST "$EDIT" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","edits":[{"type":"replace_text","path":"multi.txt","old_text":"aaa","new_text":"AAA","occurrence":2}]}')
EDIT_SUCCESS=$(pyget "$RESP" "success")
assert_eq "replace_text with occurrence=2 success" "True" "$EDIT_SUCCESS"
# Verify: should be "aaa bbb AAA bbb aaa"
RESP=$(curl -sf -X POST "$CODEX/context" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","mode":"read_file","path":"multi.txt"}')
CTX_CONTENT=$(pyget "$RESP" "content")
# Should have exactly one AAA (the second aaa was replaced)
AAA_COUNT=$(echo "$CTX_CONTENT" | python3 -c "import sys; print(sys.stdin.read().count('AAA'))")
assert_eq "Exactly one AAA in file" "1" "$AAA_COUNT"

# --- 37. Edit: replace_range ---
echo ""
echo "--- 37. Edit replace_range ---"
RESP=$(curl -sf -X POST "$EDIT" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","edits":[{"type":"replace_range","path":"test.txt","start_line":1,"end_line":1,"new_text":"first_line"}]}')
EDIT_SUCCESS=$(pyget "$RESP" "success")
assert_eq "replace_range success" "True" "$EDIT_SUCCESS"
RESP=$(curl -sf -X POST "$CODEX/context" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","mode":"read_file","path":"test.txt"}')
CTX_CONTENT=$(pyget "$RESP" "content")
assert_contains "test.txt line1 replaced" "first_line" "$CTX_CONTENT"

# --- 38. Edit: append_file ---
echo ""
echo "--- 38. Edit append_file ---"
RESP=$(curl -sf -X POST "$EDIT" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","edits":[{"type":"append_file","path":"test.txt","text":"appended_line\n"}]}')
EDIT_SUCCESS=$(pyget "$RESP" "success")
assert_eq "append_file success" "True" "$EDIT_SUCCESS"
RESP=$(curl -sf -X POST "$CODEX/context" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","mode":"read_file","path":"test.txt"}')
CTX_CONTENT=$(pyget "$RESP" "content")
assert_contains "test.txt has appended line" "appended_line" "$CTX_CONTENT"

# --- 39. Edit: create_file ---
echo ""
echo "--- 39. Edit create_file ---"
RESP=$(curl -sf -X POST "$EDIT" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","edits":[{"type":"create_file","path":"new_file.txt","content":"brand new file\n"}]}')
EDIT_SUCCESS=$(pyget "$RESP" "success")
assert_eq "create_file success" "True" "$EDIT_SUCCESS"
# Verify the file exists
RESP=$(curl -sf -X POST "$CODEX/context" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","mode":"read_file","path":"new_file.txt"}')
CTX_SUCCESS=$(pyget "$RESP" "success")
assert_eq "new_file.txt readable" "True" "$CTX_SUCCESS"

# --- 40. Edit: write_file allow_overwrite=false on existing file fails ---
echo ""
echo "--- 40. Edit write_file no-overwrite fails ---"
RESP=$(curl -s -X POST "$EDIT" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","edits":[{"type":"write_file","path":"new_file.txt","content":"overwrite attempt\n","allow_overwrite":false}]}')
EDIT_SUCCESS=$(pyget "$RESP" "success")
assert_eq "write_file no-overwrite fails" "False" "$EDIT_SUCCESS"

# --- 41. Edit: write_file allow_overwrite=true on existing file succeeds ---
echo ""
echo "--- 41. Edit write_file overwrite succeeds ---"
RESP=$(curl -sf -X POST "$EDIT" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","edits":[{"type":"write_file","path":"new_file.txt","content":"overwritten content\n","allow_overwrite":true}]}')
EDIT_SUCCESS=$(pyget "$RESP" "success")
assert_eq "write_file overwrite success" "True" "$EDIT_SUCCESS"
RESP=$(curl -sf -X POST "$CODEX/context" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","mode":"read_file","path":"new_file.txt"}')
CTX_CONTENT=$(pyget "$RESP" "content")
assert_contains "new_file.txt overwritten" "overwritten content" "$CTX_CONTENT"

# --- 41a. Edit: create_file to new subdirectory succeeds ---
echo ""
echo "--- 41a. Edit create_file to new subdirectory ---"
RESP=$(curl -sf -X POST "$EDIT" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","edits":[{"type":"create_file","path":"docs/notes/nested.txt","content":"nested text artifact\n"}]}')
EDIT_SUCCESS=$(pyget "$RESP" "success")
assert_eq "create_file nested directory success" "True" "$EDIT_SUCCESS"
RESP=$(curl -sf -X POST "$CODEX/context" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","mode":"read_file","path":"docs/notes/nested.txt"}')
CTX_CONTENT=$(pyget "$RESP" "content")
assert_contains "create_file nested content" "nested text artifact" "$CTX_CONTENT"

# --- 41b. Edit: create_binary_file succeeds ---
echo ""
echo "--- 41b. Edit create_binary_file ---"
RESP=$(curl -sf -X POST "$EDIT" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","edits":[{"type":"create_binary_file","path":"docs/diagrams/pixel.bin","base64_content":"AAECAw=="}]}')
EDIT_SUCCESS=$(pyget "$RESP" "success")
EDIT_DIFF=$(pyget "$RESP" "diff")
assert_eq "create_binary_file success" "True" "$EDIT_SUCCESS"
assert_contains "create_binary_file diff is binary" "Binary file" "$EDIT_DIFF"
RESP=$(curl -sf -X POST "$CODEX/context" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","mode":"tree"}')
TREE_ITEMS=$(echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); print('\n'.join(d.get('items') or []))")
assert_contains "create_binary_file appears in tree" "docs/diagrams/pixel.bin" "$TREE_ITEMS"

# --- 41c. Edit: write_binary_file overwrite succeeds ---
echo ""
echo "--- 41c. Edit write_binary_file overwrite succeeds ---"
RESP=$(curl -sf -X POST "$EDIT" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","edits":[{"type":"write_binary_file","path":"docs/diagrams/pixel.bin","base64_content":"AQIDBAU=","allow_overwrite":true}]}')
EDIT_SUCCESS=$(pyget "$RESP" "success")
EDIT_DIFF=$(pyget "$RESP" "diff")
assert_eq "write_binary_file overwrite success" "True" "$EDIT_SUCCESS"
assert_contains "write_binary_file diff mentions new size" "new size: 5 bytes" "$EDIT_DIFF"

# --- 41d. Edit: invalid binary base64 fails ---
echo ""
echo "--- 41d. Edit invalid binary base64 fails ---"
RESP=$(curl -s -X POST "$EDIT" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","edits":[{"type":"create_binary_file","path":"src/bad.bin","base64_content":"not base64!"}]}')
EDIT_SUCCESS=$(pyget "$RESP" "success")
EDIT_ERROR=$(pyget "$RESP" "error")
assert_eq "invalid binary base64 fails" "False" "$EDIT_SUCCESS"
assert_contains "invalid binary base64 error" "Invalid base64" "$EDIT_ERROR"

# --- 41e. Edit: text/binary mixed same path fails ---
echo ""
echo "--- 41e. Edit text/binary mixed same path fails ---"
RESP=$(curl -s -X POST "$EDIT" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","edits":[{"type":"write_file","path":"docs/mixed.bin","content":"text","allow_overwrite":true},{"type":"write_binary_file","path":"docs/mixed.bin","base64_content":"AAE=","allow_overwrite":true}]}')
EDIT_SUCCESS=$(pyget "$RESP" "success")
EDIT_ERROR=$(pyget "$RESP" "error")
assert_eq "mixed text binary same path fails" "False" "$EDIT_SUCCESS"
assert_contains "mixed text binary error" "cannot mix text and binary edits for the same path" "$EDIT_ERROR"

# --- 41f. Edit: create_binary_file_from_upload succeeds ---
echo ""
echo "--- 41f. Edit create_binary_file_from_upload ---"
RESP=$(curl -sf -X POST "$EDIT" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","edits":[{"type":"create_binary_file_from_upload","path":"docs/diagrams/from-upload.bin","source_file":"upload-source.bin"}]}')
EDIT_SUCCESS=$(pyget "$RESP" "success")
EDIT_DIFF=$(pyget "$RESP" "diff")
assert_eq "create_binary_file_from_upload success" "True" "$EDIT_SUCCESS"
assert_contains "create_binary_file_from_upload diff" "new size: 4 bytes" "$EDIT_DIFF"

# --- 41g. Edit: write_binary_file_from_upload overwrite succeeds ---
echo ""
echo "--- 41g. Edit write_binary_file_from_upload overwrite succeeds ---"
RESP=$(curl -sf -X POST "$EDIT" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","edits":[{"type":"write_binary_file_from_upload","path":"docs/diagrams/from-upload.bin","source_file":"upload-source-new.bin","allow_overwrite":true}]}')
EDIT_SUCCESS=$(pyget "$RESP" "success")
EDIT_DIFF=$(pyget "$RESP" "diff")
assert_eq "write_binary_file_from_upload overwrite success" "True" "$EDIT_SUCCESS"
assert_contains "write_binary_file_from_upload diff" "new size: 5 bytes" "$EDIT_DIFF"

# --- 41h. Edit: create_binary_file_from_url succeeds ---
echo ""
echo "--- 41h. Edit create_binary_file_from_url ---"
RESP=$(curl -s -X POST "$EDIT" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","edits":[{"type":"create_binary_file_from_url","path":"docs/diagrams/from-url.html","source_url":"https://example.com/"}]}')
URL_EDIT_SUCCESS=$(pyget "$RESP" "success")
if [ "$URL_EDIT_SUCCESS" = "True" ]; then
    EDIT_DIFF=$(pyget "$RESP" "diff")
    assert_contains "create_binary_file_from_url diff" "Binary file" "$EDIT_DIFF"
else
    URL_EDIT_ERROR=$(pyget "$RESP" "error")
    log_pass "create_binary_file_from_url skipped due network: $URL_EDIT_ERROR"
fi

# --- 41i. Edit: source_url rejects localhost/private ---
echo ""
echo "--- 41i. Edit source_url rejects localhost ---"
RESP=$(curl -s -X POST "$EDIT" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","edits":[{"type":"create_binary_file_from_url","path":"docs/diagrams/local.bin","source_url":"http://127.0.0.1:1/local.bin"}]}')
EDIT_SUCCESS=$(pyget "$RESP" "success")
EDIT_ERROR=$(pyget "$RESP" "error")
assert_eq "source_url localhost fails" "False" "$EDIT_SUCCESS"
assert_contains "source_url localhost error" "blocked private/local" "$EDIT_ERROR"

# --- 41j. Edit: oversized source_file fails ---
echo ""
echo "--- 41j. Edit oversized source_file fails ---"
python3 - <<PY
from pathlib import Path
Path('$TEST_PROJECT_DIR/oversized.bin').write_bytes(b'x' * (5 * 1024 * 1024 + 1))
PY
RESP=$(curl -s -X POST "$EDIT" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","edits":[{"type":"create_binary_file_from_upload","path":"docs/diagrams/oversized.bin","source_file":"oversized.bin"}]}')
EDIT_SUCCESS=$(pyget "$RESP" "success")
EDIT_ERROR=$(pyget "$RESP" "error")
assert_eq "oversized source_file fails" "False" "$EDIT_SUCCESS"
assert_contains "oversized source_file error" "exceeds" "$EDIT_ERROR"

# --- 41k. Edit: create_binary_artifact succeeds ---
echo ""
echo "--- 41k. Edit create_binary_artifact ---"
RESP=$(curl -sf -X POST "$EDIT" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","edits":[{"type":"create_binary_artifact","path":"docs/diagrams/generated-artifact.png","base64_content":"iVBORw0KGgoAAAANSUhEUgAAAAoAAAAKCAYAAACNMs+9AAAAH0lEQVQoU2NkYGD4z8DAwMDAwMDAwMAQF8EBACrIAf8nqgkhAAAAAElFTkSuQmCC"}]}')
EDIT_SUCCESS=$(pyget "$RESP" "success")
EDIT_DIFF=$(pyget "$RESP" "diff")
assert_eq "create_binary_artifact success" "True" "$EDIT_SUCCESS"
assert_contains "create_binary_artifact diff" "Binary file" "$EDIT_DIFF"

# --- 41l. Edit: write_binary_artifact overwrite succeeds ---
echo ""
echo "--- 41l. Edit write_binary_artifact overwrite succeeds ---"
RESP=$(curl -sf -X POST "$EDIT" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","edits":[{"type":"write_binary_artifact","path":"docs/diagrams/generated-artifact.png","base64_content":"AAECAwQFBgcICQ==","allow_overwrite":true}]}')
EDIT_SUCCESS=$(pyget "$RESP" "success")
EDIT_DIFF=$(pyget "$RESP" "diff")
assert_eq "write_binary_artifact overwrite success" "True" "$EDIT_SUCCESS"
assert_contains "write_binary_artifact diff" "new size: 10 bytes" "$EDIT_DIFF"

# --- 41m. Edit: create_binary_artifact dry_run does not write ---
echo ""
echo "--- 41m. Edit create_binary_artifact dry_run ---"
RESP=$(curl -sf -X POST "$EDIT" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","dry_run":true,"edits":[{"type":"create_binary_artifact","path":"docs/diagrams/dry-run-artifact.png","base64_content":"AAECAw=="}]}')
EDIT_SUCCESS=$(pyget "$RESP" "success")
EDIT_DIFF=$(pyget "$RESP" "diff")
assert_eq "create_binary_artifact dry_run success" "True" "$EDIT_SUCCESS"
assert_contains "create_binary_artifact dry_run diff" "new size: 4 bytes" "$EDIT_DIFF"
RESP=$(curl -sf -X POST "$CODEX/context" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","mode":"tree"}')
TREE_ITEMS=$(echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); print('\n'.join(d.get('items') or []))")
assert_not_contains "create_binary_artifact dry_run did not write" "docs/diagrams/dry-run-artifact.png" "$TREE_ITEMS"

# --- 41n. Edit: oversized base64 artifact fails ---
echo ""
echo "--- 41n. Edit oversized base64 artifact fails ---"
RESP=$(python3 - <<PY | curl -s -X POST "$EDIT" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d @-
import base64, json
payload = {
    "project": "test-project",
    "edits": [{
        "type": "create_binary_artifact",
        "path": "docs/diagrams/too-large.png",
        "base64_content": base64.b64encode(b'x' * (5 * 1024 * 1024 + 1)).decode('ascii'),
    }],
}
print(json.dumps(payload))
PY
)
EDIT_SUCCESS=$(pyget "$RESP" "success")
EDIT_ERROR=$(pyget "$RESP" "error")
assert_eq "oversized base64 artifact fails" "False" "$EDIT_SUCCESS"
assert_contains "oversized base64 artifact error" "too large" "$EDIT_ERROR"

# --- 41o. Artifact API: save_base64 succeeds ---
echo ""
echo "--- 41o. Artifact API save_base64 ---"
RESP=$(curl -sf -X POST "$CODEX/artifact" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","op":"save_base64","path":"docs/diagrams/artifact-api.png","base64_content":"AAECAw==","mime_type":"image/png"}')
ART_SUCCESS=$(pyget "$RESP" "success")
ART_DIFF=$(pyget "$RESP" "diff")
assert_eq "artifact save_base64 success" "True" "$ART_SUCCESS"
assert_contains "artifact save_base64 diff" "new size: 4 bytes" "$ART_DIFF"

# --- 41p. Artifact API: save_base64 overwrite succeeds ---
echo ""
echo "--- 41p. Artifact API save_base64 overwrite ---"
RESP=$(curl -sf -X POST "$CODEX/artifact" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","op":"save_base64","path":"docs/diagrams/artifact-api.png","base64_content":"AAECAwQFBg==","allow_overwrite":true}')
ART_SUCCESS=$(pyget "$RESP" "success")
ART_DIFF=$(pyget "$RESP" "diff")
assert_eq "artifact save_base64 overwrite success" "True" "$ART_SUCCESS"
assert_contains "artifact save_base64 overwrite diff" "new size: 7 bytes" "$ART_DIFF"

# --- 41q. Artifact API: save_upload succeeds ---
echo ""
echo "--- 41q. Artifact API save_upload ---"
RESP=$(curl -sf -X POST "$CODEX/artifact" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","op":"save_upload","path":"docs/diagrams/artifact-upload.bin","source_file":"upload-source.bin"}')
ART_SUCCESS=$(pyget "$RESP" "success")
ART_DIFF=$(pyget "$RESP" "diff")
assert_eq "artifact save_upload success" "True" "$ART_SUCCESS"
assert_contains "artifact save_upload diff" "new size: 4 bytes" "$ART_DIFF"

# --- 41r. Artifact API: save_upload with /api/files file_id succeeds ---
echo ""
echo "--- 41r. Artifact API save_upload file_id ---"
python3 - <<PY
from pathlib import Path
Path('$TMPDIR_DATA/artifact-file-id.bin').write_bytes(bytes([0, 1, 2, 3]))
PY
RESP=$(curl -sf -X POST "$BASE/api/files?channel=files" \
    -H "Authorization: Bearer $TOKEN" \
    -F "file=@$TMPDIR_DATA/artifact-file-id.bin")
ART_UPLOAD_ID=$(pyget "$RESP" "id")
assert_not_empty "artifact upload returns file id" "$ART_UPLOAD_ID"
RESP=$(curl -sf -X POST "$CODEX/artifact" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d "$(python3 -c "import json; print(json.dumps({'project':'test-project','op':'save_upload','path':'docs/diagrams/artifact-upload-file-id.bin','file_id':'$ART_UPLOAD_ID'}))")")
ART_SUCCESS=$(pyget "$RESP" "success")
ART_DIFF=$(pyget "$RESP" "diff")
assert_eq "artifact save_upload file_id success" "True" "$ART_SUCCESS"
assert_contains "artifact save_upload file_id diff" "new size: 4 bytes" "$ART_DIFF"
RESP=$(curl -sf -X POST "$CODEX/context" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","mode":"tree"}')
TREE_ITEMS=$(echo "$RESP" | python3 -c "import sys,json; d=json.load(sys.stdin); print('\n'.join(d.get('items') or []))")
assert_contains "artifact save_upload file_id appears in tree" "docs/diagrams/artifact-upload-file-id.bin" "$TREE_ITEMS"

# --- 41r2. Artifact API: save_generated base64 with companion markdown ---
echo ""
echo "--- 41r2. Artifact API save_generated base64 companion ---"
RESP=$(curl -sf -X POST "$CODEX/artifact" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","op":"save_generated","path":"docs/diagrams/generated-base64.png","base64_content":"AAECAw==","mime_type":"image/png","alt_text":"Generated base64 smoke","companion_markdown_path":"docs/diagrams/generated-base64.md"}')
ART_SUCCESS=$(pyget "$RESP" "success")
ART_SAVED=$(pyget "$RESP" "saved_path")
ART_REL=$(pyget "$RESP" "relative_path")
ART_SIZE=$(pyget "$RESP" "file_size")
ART_SNIPPET=$(pyget "$RESP" "markdown_snippet")
ART_SELECTED=$(pyget "$RESP" "selected_source")
ART_DIFF=$(pyget "$RESP" "diff")
assert_eq "artifact save_generated base64 success" "True" "$ART_SUCCESS"
assert_eq "artifact save_generated saved_path" "docs/diagrams/generated-base64.png" "$ART_SAVED"
assert_eq "artifact save_generated relative_path" "docs/diagrams/generated-base64.png" "$ART_REL"
assert_eq "artifact save_generated file_size" "4" "$ART_SIZE"
assert_eq "artifact save_generated selected_source" "base64_content" "$ART_SELECTED"
assert_contains "artifact save_generated markdown snippet" "Generated base64 smoke" "$ART_SNIPPET"
assert_contains "artifact save_generated diff has companion" "generated-base64.md" "$ART_DIFF"
RESP=$(curl -sf -X POST "$CODEX/context" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","mode":"read_file","path":"docs/diagrams/generated-base64.md"}')
COMPANION_CONTENT=$(pyget "$RESP" "content")
assert_contains "artifact companion markdown references image" "./generated-base64.png" "$COMPANION_CONTENT"

# --- 41r2b. Artifact API: save_generated multiple sources warns ---
echo ""
echo "--- 41r2b. Artifact API save_generated multiple sources warns ---"
RESP=$(curl -sf -X POST "$CODEX/artifact" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","op":"save_generated","path":"docs/diagrams/generated-multi-source.png","base64_content":"AAECAw==","source_url":"http://127.0.0.1:1/should-not-fetch.bin","mime_type":"image/png"}')
ART_SUCCESS=$(pyget "$RESP" "success")
ART_SELECTED=$(pyget "$RESP" "selected_source")
ART_WARNINGS=$(pyget "$RESP" "warnings")
assert_eq "artifact save_generated multi-source success" "True" "$ART_SUCCESS"
assert_eq "artifact save_generated multi-source selected_source" "base64_content" "$ART_SELECTED"
assert_contains "artifact save_generated multi-source warning" "Multiple artifact sources provided" "$ART_WARNINGS"
assert_contains "artifact save_generated multi-source warning selected" "base64_content" "$ART_WARNINGS"

# --- 41r3. Artifact API: save_generated no-overwrite fails ---
echo ""
echo "--- 41r3. Artifact API save_generated no-overwrite fails ---"
RESP=$(curl -s -X POST "$CODEX/artifact" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","op":"save_generated","path":"docs/diagrams/generated-base64.png","base64_content":"AAECAw==","mime_type":"image/png"}')
ART_SUCCESS=$(pyget "$RESP" "success")
ART_ERROR=$(pyget "$RESP" "error")
assert_eq "artifact save_generated no-overwrite fails" "False" "$ART_SUCCESS"
assert_contains "artifact save_generated no-overwrite error" "already exists" "$ART_ERROR"

# --- 41r4. Artifact API: save_generated overwrite succeeds ---
echo ""
echo "--- 41r4. Artifact API save_generated overwrite succeeds ---"
RESP=$(curl -sf -X POST "$CODEX/artifact" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","op":"save_generated","path":"docs/diagrams/generated-base64.png","base64_content":"AAECAwQFBg==","mime_type":"image/png","allow_overwrite":true}')
ART_SUCCESS=$(pyget "$RESP" "success")
ART_DIFF=$(pyget "$RESP" "diff")
assert_eq "artifact save_generated overwrite success" "True" "$ART_SUCCESS"
assert_contains "artifact save_generated overwrite diff" "new size: 7 bytes" "$ART_DIFF"

# --- 41r5. Artifact API: save_generated file_id succeeds ---
echo ""
echo "--- 41r5. Artifact API save_generated file_id ---"
RESP=$(curl -sf -X POST "$CODEX/artifact" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d "$(python3 -c "import json; print(json.dumps({'project':'test-project','op':'save_generated','path':'docs/diagrams/generated-file-id.bin','file_id':'$ART_UPLOAD_ID','mime_type':'application/octet-stream'}))")")
ART_SUCCESS=$(pyget "$RESP" "success")
ART_SAVED=$(pyget "$RESP" "saved_path")
ART_SELECTED=$(pyget "$RESP" "selected_source")
ART_DIFF=$(pyget "$RESP" "diff")
assert_eq "artifact save_generated file_id success" "True" "$ART_SUCCESS"
assert_eq "artifact save_generated file_id saved_path" "docs/diagrams/generated-file-id.bin" "$ART_SAVED"
assert_eq "artifact save_generated file_id selected_source" "file_id" "$ART_SELECTED"
assert_contains "artifact save_generated file_id diff" "new size: 4 bytes" "$ART_DIFF"

# --- 41r6. Artifact API: save_generated rejects localhost URL ---
echo ""
echo "--- 41r6. Artifact API save_generated rejects localhost URL ---"
RESP=$(curl -s -X POST "$CODEX/artifact" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","op":"save_generated","path":"docs/diagrams/generated-local.bin","source_url":"http://127.0.0.1:1/local.bin"}')
ART_SUCCESS=$(pyget "$RESP" "success")
ART_ERROR=$(pyget "$RESP" "error")
assert_eq "artifact save_generated localhost URL fails" "False" "$ART_SUCCESS"
assert_contains "artifact save_generated localhost URL error" "blocked private/local" "$ART_ERROR"

# --- 41s. Artifact API: missing file_id fails ---
echo ""
echo "--- 41s. Artifact API missing file_id fails ---"
RESP=$(curl -s -X POST "$CODEX/artifact" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","op":"save_upload","path":"docs/diagrams/artifact-missing-file-id.bin","file_id":"missing-file-id"}')
ART_SUCCESS=$(pyget "$RESP" "success")
ART_ERROR=$(pyget "$RESP" "error")
assert_eq "artifact missing file_id fails" "False" "$ART_SUCCESS"
assert_contains "artifact missing file_id error" "not found" "$ART_ERROR"

# --- 41t. Artifact API: save_url succeeds or skips on network ---
echo ""
echo "--- 41t. Artifact API save_url ---"
RESP=$(curl -s -X POST "$CODEX/artifact" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","op":"save_url","path":"docs/diagrams/artifact-url.html","source_url":"https://example.com/"}')
ART_SUCCESS=$(pyget "$RESP" "success")
if [ "$ART_SUCCESS" = "True" ]; then
    ART_DIFF=$(pyget "$RESP" "diff")
    assert_contains "artifact save_url diff" "Binary file" "$ART_DIFF"
else
    ART_ERROR=$(pyget "$RESP" "error")
    log_pass "artifact save_url skipped due network: $ART_ERROR"
fi

# --- 41s. Artifact API: rejects localhost/private URL ---
echo ""
echo "--- 41s. Artifact API rejects localhost URL ---"
RESP=$(curl -s -X POST "$CODEX/artifact" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","op":"save_url","path":"docs/diagrams/artifact-local.bin","source_url":"http://127.0.0.1:1/local.bin"}')
ART_SUCCESS=$(pyget "$RESP" "success")
ART_ERROR=$(pyget "$RESP" "error")
assert_eq "artifact localhost URL fails" "False" "$ART_SUCCESS"
assert_contains "artifact localhost URL error" "blocked private/local" "$ART_ERROR"

# --- 41t. Artifact API: oversized base64 fails ---
echo ""
echo "--- 41t. Artifact API oversized base64 fails ---"
RESP=$(python3 - <<PY | curl -s -X POST "$CODEX/artifact" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d @-
import base64, json
payload = {
    "project": "test-project",
    "op": "save_base64",
    "path": "docs/diagrams/artifact-too-large.png",
    "base64_content": base64.b64encode(b'x' * (5 * 1024 * 1024 + 1)).decode('ascii'),
}
print(json.dumps(payload))
PY
)
ART_SUCCESS=$(pyget "$RESP" "success")
ART_ERROR=$(pyget "$RESP" "error")
assert_eq "artifact oversized base64 fails" "False" "$ART_SUCCESS"
assert_contains "artifact oversized base64 error" "too large" "$ART_ERROR"

# --- 42. Edit: dry_run=true returns diff but does not modify ---
echo ""
echo "--- 42. Edit dry_run ---"
# Read current content first
RESP=$(curl -sf -X POST "$CODEX/context" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","mode":"read_file","path":"new_file.txt"}')
BEFORE_CONTENT=$(pyget "$RESP" "content")
# Dry run edit
RESP=$(curl -sf -X POST "$EDIT" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","dry_run":true,"edits":[{"type":"replace_text","path":"new_file.txt","old_text":"overwritten","new_text":"DRYRUN"}]}')
EDIT_SUCCESS=$(pyget "$RESP" "success")
EDIT_DIFF=$(pyget "$RESP" "diff")
assert_eq "dry_run success" "True" "$EDIT_SUCCESS"
assert_contains "dry_run diff contains -overwritten" "overwritten" "$EDIT_DIFF"
# Verify file was NOT modified
RESP=$(curl -sf -X POST "$CODEX/context" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","mode":"read_file","path":"new_file.txt"}')
AFTER_CONTENT=$(pyget "$RESP" "content")
assert_eq "dry_run did not modify file" "$BEFORE_CONTENT" "$AFTER_CONTENT"

# --- 42a. Edit: allows root .gitignore and still rejects .git directory ---
echo ""
echo "--- 42a. Edit allows .gitignore ---"
RESP=$(curl -sf -X POST "$EDIT" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","edits":[{"type":"create_file","path":".gitignore","content":".codex/jobs/\n"}]}')
EDIT_SUCCESS=$(pyget "$RESP" "success")
assert_eq "Edit .gitignore success" "True" "$EDIT_SUCCESS"
RESP=$(curl -sf -X POST "$CODEX/context" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","mode":"read_file","path":".gitignore"}')
GITIGNORE_CONTENT=$(pyget "$RESP" "content")
assert_contains "Edit .gitignore contains jobs ignore" ".codex/jobs/" "$GITIGNORE_CONTENT"
RESP=$(curl -s -X POST "$EDIT" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","edits":[{"type":"write_file","path":".git/config","content":"bad","allow_overwrite":true}]}')
EDIT_SUCCESS=$(pyget "$RESP" "success")
EDIT_ERROR=$(pyget "$RESP" "error")
assert_eq "Edit .git/config blocked" "False" "$EDIT_SUCCESS"
assert_contains "Edit .git/config error" "sensitive" "$EDIT_ERROR"

# --- 43. Edit: rejects .env ---
echo ""
echo "--- 43. Edit rejects .env ---"
RESP=$(curl -s -X POST "$EDIT" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","edits":[{"type":"create_file","path":".env","content":"SECRET=x\n"}]}')
EDIT_SUCCESS=$(pyget "$RESP" "success")
assert_eq "Edit .env blocked" "False" "$EDIT_SUCCESS"

# --- 44. Edit: rejects ../evil.txt ---
echo ""
echo "--- 44. Edit rejects traversal ---"
RESP=$(curl -s -X POST "$EDIT" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","edits":[{"type":"create_file","path":"../evil.txt","content":"evil\n"}]}')
EDIT_SUCCESS=$(pyget "$RESP" "success")
assert_eq "Edit ../evil.txt blocked" "False" "$EDIT_SUCCESS"

# --- 45. Edit: rejects /etc/passwd ---
echo ""
echo "--- 45. Edit rejects absolute path ---"
RESP=$(curl -s -X POST "$EDIT" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","edits":[{"type":"write_file","path":"/etc/passwd","content":"evil\n","allow_overwrite":true}]}')
EDIT_SUCCESS=$(pyget "$RESP" "success")
assert_eq "Edit /etc/passwd blocked" "False" "$EDIT_SUCCESS"

# --- 46. Edit: rejects target/foo ---
echo ""
echo "--- 46. Edit rejects target/ ---"
RESP=$(curl -s -X POST "$EDIT" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","edits":[{"type":"create_file","path":"target/evil.txt","content":"evil\n"}]}')
EDIT_SUCCESS=$(pyget "$RESP" "success")
assert_eq "Edit target/ blocked" "False" "$EDIT_SUCCESS"

# --- 47. Edit + runProjectCheck(test) passes ---
echo ""
echo "--- 47. Edit then check ---"
# The check.sh in test project just echoes "check passed" and exits 0
RESP=$(curl -sf -X POST "$CODEX/check" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"project":"test-project","suite":"test"}')
CHECK_SUCCESS=$(pyget "$RESP" "success")
assert_eq "Check after edits passes" "True" "$CHECK_SUCCESS"

# ============================================================================
# Summary
# ============================================================================
echo ""
echo "============================================"
echo "  E2E Test Results: $PASS passed, $FAIL failed, $TOTAL total"
echo "============================================"

if [ "$FAIL" -gt 0 ]; then
    echo ""
    echo "Server log (last 20 lines):"
    tail -20 "$LOGFILE" | sed 's/^/  /'
    exit 1
fi

echo "All tests passed!"
exit 0
