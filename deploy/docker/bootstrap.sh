#!/bin/sh
set -eu

ENV_FILE=.env
RECEIPT_FILE=.webcodex-bootstrap.receipt
RECEIPT_VERSION=1
BUILD_OVERLAY=compose.build.yaml
HOST_IP=127.0.0.1
HOST_PORT=8080
ZERO_TOKEN=0000000000000000000000000000000000000000000000000000000000000000
HEALTH_WAIT_SECS=${WEBCODEX_BOOTSTRAP_HEALTH_WAIT_SECS:-90}
TEMP_FILES=

cleanup_temps() {
    for path in $TEMP_FILES; do
        rm -f "$path"
    done
}
trap cleanup_temps EXIT
trap 'cleanup_temps; exit 130' HUP INT TERM

fail() {
    echo "$*" >&2
    exit 1
}

usage() {
    cat >&2 <<EOF_USAGE
Usage:
  $0 <public-https-origin> [--build-from-source]
  $0 status
  $0 resume
  $0 rollback

Examples:
  $0 https://webcodex.example.com
  $0 https://webcodex.example.com --build-from-source
  $0 status
  $0 resume
  $0 rollback
EOF_USAGE
    exit 2
}

validate_port_number() {
    value=$1
    case "$value" in
        ""|*[!0-9]*) return 1 ;;
    esac
    [ "$value" -ge 1 ] 2>/dev/null && [ "$value" -le 65535 ] 2>/dev/null
}

validate_ipv4() {
    value=$1
    old_ifs=$IFS
    IFS=.
    set -- $value
    IFS=$old_ifs
    [ "$#" -eq 4 ] || return 1
    for octet in "$@"; do
        case "$octet" in
            ""|*[!0-9]*) return 1 ;;
        esac
        [ "$octet" -le 255 ] 2>/dev/null || return 1
    done
}

validate_dns_name() {
    value=$1
    [ -n "$value" ] || return 1
    [ "${#value}" -le 253 ] || return 1
    case "$value" in
        .*|*.|*..*) return 1 ;;
    esac
    old_ifs=$IFS
    IFS=.
    set -- $value
    IFS=$old_ifs
    for label in "$@"; do
        [ -n "$label" ] && [ "${#label}" -le 63 ] || return 1
        case "$label" in
            -*|*-|*[!A-Za-z0-9-]*) return 1 ;;
        esac
    done
}

count_ipv6_groups() {
    value=$1
    if [ -z "$value" ]; then
        printf '0\n'
        return 0
    fi
    old_ifs=$IFS
    IFS=:
    set -- $value
    IFS=$old_ifs
    raw_count=$#
    effective_count=0
    index=0
    for group in "$@"; do
        index=$((index + 1))
        case "$group" in
            *.*)
                [ "$index" -eq "$raw_count" ] || return 1
                validate_ipv4 "$group" || return 1
                effective_count=$((effective_count + 2))
                ;;
            ""|?????*|*[!0-9A-Fa-f]*) return 1 ;;
            *) effective_count=$((effective_count + 1)) ;;
        esac
    done
    printf '%s\n' "$effective_count"
}

validate_ipv6() {
    value=$1
    [ -n "$value" ] || return 1
    case "$value" in
        *:::*) return 1 ;;
    esac
    case "$value" in
        *::*)
            left=${value%%::*}
            right=${value#*::}
            case "$right" in
                *::*) return 1 ;;
            esac
            left_count=$(count_ipv6_groups "$left") || return 1
            right_count=$(count_ipv6_groups "$right") || return 1
            [ $((left_count + right_count)) -lt 8 ]
            ;;
        *)
            count=$(count_ipv6_groups "$value") || return 1
            [ "$count" -eq 8 ]
            ;;
    esac
}

validate_public_url() {
    value=$1
    case "$value" in
        *[[:space:][:cntrl:]]*)
            echo "public URL must not contain whitespace or control characters" >&2
            return 1
            ;;
    esac
    case "$value" in
        https://*) ;;
        *)
            echo "public URL must be an HTTPS origin" >&2
            return 1
            ;;
    esac
    authority=${value#https://}
    [ -n "$authority" ] || {
        echo "public URL must contain a host" >&2
        return 1
    }
    case "$authority" in
        */*|*\?*|*\#*|*@*)
            echo "public URL must be an HTTPS origin without userinfo, path, query, or fragment" >&2
            return 1
            ;;
    esac

    port=
    case "$authority" in
        \[*\]*)
            bracket_host=${authority%%]*}
            host=${bracket_host#\[}
            rest=${authority#"$bracket_host"}
            rest=${rest#]}
            case "$rest" in
                "") ;;
                :*) port=${rest#:} ;;
                *)
                    echo "public URL has invalid bracketed IPv6 authority" >&2
                    return 1
                    ;;
            esac
            validate_ipv6 "$host" || {
                echo "public URL has invalid bracketed IPv6 host" >&2
                return 1
            }
            ;;
        *:*)
            host=${authority%%:*}
            port=${authority#*:}
            case "$port" in
                *:*)
                    echo "IPv6 public URL hosts must be bracketed" >&2
                    return 1
                    ;;
            esac
            ;;
        *) host=$authority ;;
    esac

    if [ "${authority#\[}" = "$authority" ]; then
        case "$host" in
            "")
                echo "public URL must contain a host" >&2
                return 1
                ;;
            *[!0-9.]* )
                validate_dns_name "$host" || {
                    echo "public URL has invalid DNS host" >&2
                    return 1
                }
                ;;
            *)
                validate_ipv4 "$host" || {
                    echo "public URL has invalid IPv4 host" >&2
                    return 1
                }
                ;;
        esac
    fi

    if [ -n "$port" ]; then
        validate_port_number "$port" || {
            echo "public URL port must be in 1..65535" >&2
            return 1
        }
    fi
}

safe_compose_file() {
    value=$1
    case "$value" in
        ""|/*|-*|../*|*/../*|*/..|*[!A-Za-z0-9._/-]*) return 1 ;;
        *) return 0 ;;
    esac
}

validate_server_image() {
    value=$1
    case "$value" in
        ""|*[!A-Za-z0-9._/:@-]*) return 1 ;;
        *) return 0 ;;
    esac
}

sha256_file() {
    path=$1
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$path" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$path" | awk '{print $1}'
    else
        return 127
    fi
}

valid_sha256_or_dash() {
    value=$1
    [ "$value" = - ] && return 0
    [ "${#value}" -eq 64 ] || return 1
    case "$value" in
        *[!0-9a-f]*) return 1 ;;
        *) return 0 ;;
    esac
}

atomic_commit() {
    target=$1
    tmp=$2
    chmod 600 "$tmp"
    if ! sync "$tmp"; then
        echo "failed to fsync temporary bootstrap state: $tmp" >&2
        return 1
    fi
    if ! mv "$tmp" "$target"; then
        echo "failed to atomically commit bootstrap state: $target" >&2
        return 1
    fi
    TEMP_FILES=
}

write_receipt() {
    next_phase=$1
    next_env_sha=$2
    tmp="$RECEIPT_FILE.$$.tmp"
    TEMP_FILES=$tmp
    umask 077
    cat > "$tmp" <<EOF_RECEIPT
version=$RECEIPT_VERSION
phase=$next_phase
mode=$MODE
public_url=$PUBLIC_URL
compose_file=$COMPOSE_FILE
compose_sha256=$COMPOSE_DIGEST
overlay_sha256=$OVERLAY_DIGEST
server_image=$SERVER_IMAGE
env_sha256=$next_env_sha
EOF_RECEIPT
    atomic_commit "$RECEIPT_FILE" "$tmp"
    PHASE=$next_phase
    ENV_DIGEST=$next_env_sha
}

receipt_field() {
    key=$1
    count=$(grep -c "^${key}=" "$RECEIPT_FILE" || true)
    [ "$count" -eq 1 ] || fail "invalid installation receipt: expected exactly one $key field"
    sed -n "s/^${key}=//p" "$RECEIPT_FILE"
}

load_receipt() {
    [ -f "$RECEIPT_FILE" ] && [ ! -L "$RECEIPT_FILE" ] || fail "installation receipt not found: $RECEIPT_FILE"
    lines=$(wc -l < "$RECEIPT_FILE" | tr -d ' ')
    [ "$lines" -eq 9 ] || fail "invalid installation receipt: unexpected field count"
    version=$(receipt_field version)
    [ "$version" = "$RECEIPT_VERSION" ] || fail "unsupported installation receipt version: $version"
    PHASE=$(receipt_field phase)
    MODE=$(receipt_field mode)
    PUBLIC_URL=$(receipt_field public_url)
    COMPOSE_FILE=$(receipt_field compose_file)
    COMPOSE_DIGEST=$(receipt_field compose_sha256)
    OVERLAY_DIGEST=$(receipt_field overlay_sha256)
    SERVER_IMAGE=$(receipt_field server_image)
    ENV_DIGEST=$(receipt_field env_sha256)

    case "$PHASE" in
        AssetsPrepared|SecretCommitted|ContainerStarted|ServerHealthy|PairingReady) ;;
        *) fail "invalid installation receipt phase: $PHASE" ;;
    esac
    case "$MODE" in
        image|source) ;;
        *) fail "invalid installation receipt mode: $MODE" ;;
    esac
    validate_public_url "$PUBLIC_URL" || fail "installation receipt contains an invalid public URL"
    safe_compose_file "$COMPOSE_FILE" || fail "installation receipt contains an invalid compose path"
    valid_sha256_or_dash "$COMPOSE_DIGEST" && [ "$COMPOSE_DIGEST" != - ] || fail "installation receipt contains an invalid compose digest"
    valid_sha256_or_dash "$OVERLAY_DIGEST" || fail "installation receipt contains an invalid overlay digest"
    valid_sha256_or_dash "$ENV_DIGEST" || fail "installation receipt contains an invalid env fingerprint"
    if [ "$MODE" = image ]; then
        validate_server_image "$SERVER_IMAGE" || fail "installation receipt contains an invalid Server image"
        [ "$OVERLAY_DIGEST" = - ] || fail "image installation receipt unexpectedly contains a source overlay digest"
    else
        [ -z "$SERVER_IMAGE" ] || fail "source installation receipt unexpectedly contains a Server image"
        [ "$OVERLAY_DIGEST" != - ] || fail "source installation receipt is missing its build overlay digest"
    fi
    if [ "$PHASE" = AssetsPrepared ]; then
        [ "$ENV_DIGEST" = - ] || fail "AssetsPrepared receipt unexpectedly contains an env fingerprint"
    else
        [ "$ENV_DIGEST" != - ] || fail "$PHASE receipt is missing its env fingerprint"
    fi
}

compose_base() {
    docker compose -f "$COMPOSE_FILE" "$@"
}

compose_full() {
    if [ "$MODE" = source ]; then
        docker compose -f "$COMPOSE_FILE" -f "$BUILD_OVERLAY" "$@"
    else
        docker compose -f "$COMPOSE_FILE" "$@"
    fi
}

require_runtime_dependencies() {
    command -v docker >/dev/null 2>&1 || fail "docker is required"
    docker compose version >/dev/null 2>&1 || fail "docker compose v2 is required"
    command -v sync >/dev/null 2>&1 || fail "sync is required for durable bootstrap state commits"
    if ! command -v sha256sum >/dev/null 2>&1 && ! command -v shasum >/dev/null 2>&1; then
        fail "sha256sum or shasum is required"
    fi
}

verify_files_against_receipt() {
    [ -f "$COMPOSE_FILE" ] || fail "compose file recorded by receipt is missing: $COMPOSE_FILE"
    actual=$(sha256_file "$COMPOSE_FILE") || fail "could not hash compose file"
    [ "$actual" = "$COMPOSE_DIGEST" ] || fail "compose file digest does not match installation receipt"
    if [ "$MODE" = source ]; then
        [ -f "$BUILD_OVERLAY" ] || fail "$BUILD_OVERLAY recorded by receipt is missing"
        actual=$(sha256_file "$BUILD_OVERLAY") || fail "could not hash source build overlay"
        [ "$actual" = "$OVERLAY_DIGEST" ] || fail "source build overlay digest does not match installation receipt"
    fi
    if [ "$PHASE" != AssetsPrepared ]; then
        [ -f "$ENV_FILE" ] && [ ! -L "$ENV_FILE" ] || fail "$ENV_FILE recorded by receipt is missing or unsafe"
        actual=$(sha256_file "$ENV_FILE") || fail "could not hash $ENV_FILE"
        [ "$actual" = "$ENV_DIGEST" ] || fail "$ENV_FILE fingerprint does not match installation receipt"
    fi
}

validate_committed_env() {
    [ -f "$ENV_FILE" ] && [ ! -L "$ENV_FILE" ] || fail "$ENV_FILE is missing or unsafe"
    if [ "$MODE" = image ]; then
        expected_lines=8
    else
        expected_lines=7
    fi
    lines=$(wc -l < "$ENV_FILE" | tr -d ' ')
    [ "$lines" -eq "$expected_lines" ] || fail "$ENV_FILE does not match the canonical bootstrap layout"
    for expected in \
        "WEBCODEX_PUBLIC_URL=$PUBLIC_URL" \
        "WEBCODEX_HOST_IP=$HOST_IP" \
        "WEBCODEX_HOST_PORT=$HOST_PORT" \
        "RUST_LOG=info" \
        "WEBCODEX_MCP_MODEL_SURFACE=adaptive-runtime-v1" \
        "COMPOSE_FILE=$COMPOSE_FILE"; do
        [ "$(grep -Fxc "$expected" "$ENV_FILE" || true)" -eq 1 ] \
            || fail "$ENV_FILE does not match the installation receipt"
    done
    if [ "$MODE" = image ]; then
        [ "$(grep -Fxc "WEBCODEX_SERVER_IMAGE=$SERVER_IMAGE" "$ENV_FILE" || true)" -eq 1 ] \
            || fail "$ENV_FILE does not match the recorded Server image"
    elif grep -q '^WEBCODEX_SERVER_IMAGE=' "$ENV_FILE"; then
        fail "$ENV_FILE unexpectedly contains a Server image for source mode"
    fi
    [ "$(grep -c '^WEBCODEX_TOKEN=' "$ENV_FILE" || true)" -eq 1 ] \
        || fail "$ENV_FILE does not contain exactly one administrator token"
    token=$(sed -n 's/^WEBCODEX_TOKEN=//p' "$ENV_FILE")
    [ "${#token}" -eq 64 ] || fail "$ENV_FILE administrator token has an invalid length"
    case "$token" in
        *[!0-9a-f]*) fail "$ENV_FILE administrator token is not lowercase hex" ;;
    esac
}

reconcile_secret_commit_if_needed() {
    [ "$PHASE" = AssetsPrepared ] || return 0
    [ -e "$ENV_FILE" ] || return 0
    validate_committed_env
    ENV_DIGEST=$(sha256_file "$ENV_FILE") || fail "could not fingerprint committed $ENV_FILE"
    write_receipt SecretCommitted "$ENV_DIGEST"
}

port_preflight() {
    if command -v ss >/dev/null 2>&1; then
        if ss -H -ltn "sport = :$HOST_PORT" 2>/dev/null | grep -q .; then
            fail "$HOST_IP:$HOST_PORT is already listening; free the port before retrying"
        fi
        return 0
    fi
    if command -v netstat >/dev/null 2>&1; then
        if netstat -ltn 2>/dev/null | grep -E "[.:]${HOST_PORT}[[:space:]]" >/dev/null; then
            fail "$HOST_IP:$HOST_PORT is already listening; free the port before retrying"
        fi
        return 0
    fi
    fail "ss or netstat is required for host port preflight"
}

preflight_fresh_install() {
    validate_public_url "$PUBLIC_URL" || exit 2
    safe_compose_file "$COMPOSE_FILE" || {
        echo "invalid COMPOSE_FILE: expected a safe relative path" >&2
        exit 2
    }
    [ -f "$COMPOSE_FILE" ] || fail "compose file not found: $COMPOSE_FILE"
    [ ! -e "$RECEIPT_FILE" ] || fail "installation receipt already exists; use '$0 status', '$0 resume', or '$0 rollback'"
    [ ! -e "$ENV_FILE" ] || fail "$ENV_FILE exists without an installation receipt; refusing to guess whether its administrator token is safe to replace"
    require_runtime_dependencies

    if [ "$MODE" = source ]; then
        [ "${WEBCODEX_RELEASE_BOOTSTRAP:-false}" != true ] || fail "release bootstrap assets do not support --build-from-source"
        [ -f "$BUILD_OVERLAY" ] || fail "$BUILD_OVERLAY is required for --build-from-source"
        [ -f Dockerfile ] || fail "Dockerfile is required for --build-from-source"
    fi

    if [ "$MODE" = source ]; then
        WEBCODEX_TOKEN=$ZERO_TOKEN WEBCODEX_PUBLIC_URL="$PUBLIC_URL" \
            compose_full config >/dev/null || fail "source Compose configuration is invalid"
    else
        if [ -z "$SERVER_IMAGE" ]; then
            SERVER_IMAGE=$(WEBCODEX_TOKEN=$ZERO_TOKEN WEBCODEX_PUBLIC_URL="$PUBLIC_URL" compose_base config --images)
        fi
        validate_server_image "$SERVER_IMAGE" || {
            echo "invalid WEBCODEX_SERVER_IMAGE: expected a Docker image reference" >&2
            exit 2
        }
        WEBCODEX_TOKEN=$ZERO_TOKEN WEBCODEX_PUBLIC_URL="$PUBLIC_URL" WEBCODEX_SERVER_IMAGE="$SERVER_IMAGE" \
            compose_base config >/dev/null || fail "Compose configuration is invalid"
    fi

    if [ "$MODE" = image ]; then
        existing=$(WEBCODEX_TOKEN=$ZERO_TOKEN WEBCODEX_PUBLIC_URL="$PUBLIC_URL" WEBCODEX_SERVER_IMAGE="$SERVER_IMAGE" \
            compose_full ps -aq webcodex 2>/dev/null || true)
    else
        existing=$(WEBCODEX_TOKEN=$ZERO_TOKEN WEBCODEX_PUBLIC_URL="$PUBLIC_URL" \
            compose_full ps -aq webcodex 2>/dev/null || true)
    fi
    [ -z "$existing" ] || fail "an existing WebCodex Compose container was found without an installation receipt; refusing to adopt it implicitly"
    port_preflight

    if [ "$MODE" = image ]; then
        WEBCODEX_TOKEN=$ZERO_TOKEN WEBCODEX_PUBLIC_URL="$PUBLIC_URL" WEBCODEX_SERVER_IMAGE="$SERVER_IMAGE" \
            compose_base pull webcodex || {
                echo "could not pull the published WebCodex Server image: $SERVER_IMAGE" >&2
                echo "If the official image is not published/public yet, retry with --build-from-source." >&2
                exit 1
            }
    fi

    COMPOSE_DIGEST=$(sha256_file "$COMPOSE_FILE") || fail "could not hash compose file"
    if [ "$MODE" = source ]; then
        OVERLAY_DIGEST=$(sha256_file "$BUILD_OVERLAY") || fail "could not hash source build overlay"
    else
        OVERLAY_DIGEST=-
    fi
    ENV_DIGEST=-
    write_receipt AssetsPrepared -
}

generate_token() {
    if command -v openssl >/dev/null 2>&1; then
        TOKEN=$(openssl rand -hex 32)
    else
        TOKEN=$(od -An -N32 -tx1 /dev/urandom | tr -d ' \n')
    fi
    case "$TOKEN" in
        ????????????????????????????????????????????????????????????????) ;;
        *) fail "failed to generate a 32-byte administrator token" ;;
    esac
    case "$TOKEN" in
        *[!0-9a-f]*) fail "administrator token generator returned non-hex data" ;;
    esac
}

commit_secret_env() {
    generate_token
    tmp="$ENV_FILE.$$.tmp"
    TEMP_FILES=$tmp
    umask 077
    {
        printf 'WEBCODEX_PUBLIC_URL=%s\n' "$PUBLIC_URL"
        printf 'WEBCODEX_TOKEN=%s\n' "$TOKEN"
        printf 'WEBCODEX_HOST_IP=%s\n' "$HOST_IP"
        printf 'WEBCODEX_HOST_PORT=%s\n' "$HOST_PORT"
        printf 'RUST_LOG=info\n'
        printf 'WEBCODEX_MCP_MODEL_SURFACE=adaptive-runtime-v1\n'
        printf 'COMPOSE_FILE=%s\n' "$COMPOSE_FILE"
        if [ "$MODE" = image ]; then
            printf 'WEBCODEX_SERVER_IMAGE=%s\n' "$SERVER_IMAGE"
        fi
    } > "$tmp"
    atomic_commit "$ENV_FILE" "$tmp" || return 1
    ENV_DIGEST=$(sha256_file "$ENV_FILE") || fail "could not fingerprint committed $ENV_FILE"
    write_receipt SecretCommitted "$ENV_DIGEST"
}

prepare_source_build_identity() {
    WEBCODEX_GIT_COMMIT=
    WEBCODEX_GIT_DIRTY=
    if command -v git >/dev/null 2>&1 && git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
        WEBCODEX_GIT_COMMIT=$(git rev-parse --short=12 HEAD)
        if [ -n "$(git status --porcelain --untracked-files=normal)" ]; then
            WEBCODEX_GIT_DIRTY=true
        else
            WEBCODEX_GIT_DIRTY=false
        fi
    fi
    WEBCODEX_BUILT_AT=$(date +%s)
    export WEBCODEX_GIT_COMMIT WEBCODEX_GIT_DIRTY WEBCODEX_BUILT_AT
}

container_id() {
    compose_full ps -q webcodex 2>/dev/null || true
}

start_container_if_needed() {
    existing=$(container_id)
    if [ -n "$existing" ]; then
        write_receipt ContainerStarted "$ENV_DIGEST"
        return 0
    fi

    port_preflight
    if [ "$MODE" = source ]; then
        prepare_source_build_identity
        compose_full up -d --build || return 1
    else
        # Fresh preflight already pulled the exact image. After SecretCommitted,
        # retries must not acquire a new registry dependency merely to recover
        # from a port/daemon/startup failure.
        compose_base up -d --no-build --pull never || return 1
    fi
    existing=$(container_id)
    [ -n "$existing" ] || fail "docker compose up returned success but no webcodex container exists"
    write_receipt ContainerStarted "$ENV_DIGEST"
}

wait_for_server_health() {
    cid=$(container_id)
    [ -n "$cid" ] || fail "WebCodex container is missing; run '$0 rollback' and retry resume"
    waited=0
    while [ "$waited" -le "$HEALTH_WAIT_SECS" ]; do
        health=$(docker inspect --format '{{if .State.Health}}{{.State.Health.Status}}{{else}}{{.State.Status}}{{end}}' "$cid" 2>/dev/null || true)
        case "$health" in
            healthy)
                compose_full exec -T webcodex curl -fsS http://127.0.0.1:8080/openapi.json >/dev/null \
                    || fail "WebCodex healthcheck is healthy but /openapi.json verification failed"
                write_receipt ServerHealthy "$ENV_DIGEST"
                return 0
                ;;
            unhealthy|exited|dead)
                fail "WebCodex container became $health before the Server was ready; fix the cause and run '$0 resume'"
                ;;
        esac
        sleep 2
        waited=$((waited + 2))
    done
    fail "timed out waiting for WebCodex Server health; run '$0 status' and '$0 resume' after fixing the cause"
}

create_pairing_code() {
    PAIRING_OUTPUT=$(compose_full exec -T webcodex sh -lc \
        'webcodex pairing create --server-url "$WEBCODEX_PUBLIC_URL" --username admin --ttl-secs 600') \
        || fail "Server is healthy but pairing-code creation failed; run '$0 resume' to retry only this final stage"
    printf '%s\n' "$PAIRING_OUTPUT"
    write_receipt PairingReady "$ENV_DIGEST"
}

print_success() {
    if [ "$MODE" = source ]; then
        DEPLOYMENT_SOURCE="local source build"
    else
        DEPLOYMENT_SOURCE=$SERVER_IMAGE
    fi
    cat <<EOF_DONE

WebCodex server is healthy.
Bootstrap phase:          PairingReady
Deployment source:       $DEPLOYMENT_SOURCE
Installation receipt:    $RECEIPT_FILE

Reverse-proxy upstream: http://127.0.0.1:$HOST_PORT
Public URL:            $PUBLIC_URL
Console:               $PUBLIC_URL/console
OpenAPI:               $PUBLIC_URL/openapi.json
MCP:                   $PUBLIC_URL/mcp

This Compose stack runs webcodex-server only. It does not run webcodex-runner
and does not mount any source repository.

A short-lived pairing code was created only after the Server became healthy.
If that code expires, create another with:
  docker compose -f "$COMPOSE_FILE" exec webcodex sh -lc 'webcodex pairing create --server-url "\$WEBCODEX_PUBLIC_URL" --username admin --ttl-secs 600'

On each repository machine, redeem only the short-lived pairing code as the
ordinary user who will run project commands:
  webcodex login "$PUBLIC_URL" --code <wc_pair_...> --allowed-root "\$HOME/git"
  webcodex runner install --scope user --config <login-reported-agent-config>

Keep $ENV_FILE private. It contains the bootstrap administrator token. Do not copy
that token to a repository machine or pass it to webcodex connect; connect is
the separate hosted shared-key path.
EOF_DONE
}

resume_install() {
    # Receipt/file integrity is fail-closed and checked before invoking Docker so
    # a tampered env or deployment asset cannot cause any runtime side effect.
    verify_files_against_receipt
    require_runtime_dependencies

    if [ "$PHASE" = AssetsPrepared ]; then
        reconcile_secret_commit_if_needed
    fi

    if [ "$PHASE" = AssetsPrepared ]; then
        commit_secret_env || fail "failed to durably commit $ENV_FILE; no partial $ENV_FILE was installed"
    fi

    if [ "$PHASE" = SecretCommitted ]; then
        start_container_if_needed || fail "docker compose up failed; the administrator token and receipt were preserved for '$0 resume'"
    fi

    if [ "$PHASE" = ContainerStarted ]; then
        wait_for_server_health
    fi

    if [ "$PHASE" = ServerHealthy ]; then
        create_pairing_code
    fi

    [ "$PHASE" = PairingReady ] || fail "bootstrap stopped at unexpected phase: $PHASE"
    print_success
}

show_status() {
    if [ ! -e "$RECEIPT_FILE" ]; then
        if [ -e "$ENV_FILE" ]; then
            fail "$ENV_FILE exists without an installation receipt; this deployment predates recoverable bootstrap state"
        fi
        echo "WebCodex bootstrap status: not started"
        return 0
    fi
    load_receipt
    verify_files_against_receipt
    if [ "$PHASE" = AssetsPrepared ] && [ -e "$ENV_FILE" ]; then
        validate_committed_env
        env_status="yes (atomic commit complete; receipt reconciliation pending)"
    elif [ "$ENV_DIGEST" = - ]; then
        env_status=no
    else
        env_status=yes
    fi
    cat <<EOF_STATUS
WebCodex bootstrap status
  phase:       $PHASE
  mode:        $MODE
  public URL:  $PUBLIC_URL
  compose:     $COMPOSE_FILE
  env present: $env_status
EOF_STATUS
}

rollback_install() {
    if [ ! -e "$RECEIPT_FILE" ]; then
        if [ -e "$ENV_FILE" ]; then
            fail "$ENV_FILE exists without an installation receipt; refusing to delete an untracked administrator token"
        fi
        echo "WebCodex bootstrap rollback: nothing to do"
        return 0
    fi
    load_receipt
    verify_files_against_receipt

    if [ "$PHASE" = AssetsPrepared ] && [ -e "$ENV_FILE" ]; then
        # A crash may have happened after the atomic env rename but before the
        # receipt phase update. Never orphan or regenerate that committed token.
        require_runtime_dependencies
        reconcile_secret_commit_if_needed
    fi
    if [ "$PHASE" = AssetsPrepared ]; then
        rm -f "$RECEIPT_FILE"
        echo "WebCodex bootstrap rollback: returned to Preflight; no administrator token existed"
        return 0
    fi

    require_runtime_dependencies
    existing=$(compose_full ps -aq webcodex 2>/dev/null || true)
    if [ -n "$existing" ]; then
        compose_full down || fail "rollback could not stop/remove the Compose container; receipt and administrator token were preserved"
    fi
    # Preserve the administrator token and named volume. Regenerating the token
    # after the Server may have initialized durable state can create a real lockout.
    write_receipt SecretCommitted "$ENV_DIGEST"
    cat <<EOF_ROLLBACK
WebCodex bootstrap rollback stopped runtime effects and returned to SecretCommitted.
$ENV_FILE and the named data volume were preserved intentionally.
After fixing the failure, run:
  $0 resume
EOF_ROLLBACK
}

ACTION=install
BUILD_FROM_SOURCE=false
PUBLIC_URL=
COMPOSE_FILE=${COMPOSE_FILE:-compose.yaml}
SERVER_IMAGE=${WEBCODEX_SERVER_IMAGE:-}
MODE=image
PHASE=
COMPOSE_DIGEST=-
OVERLAY_DIGEST=-
ENV_DIGEST=-

case "$#" in
    1)
        case "$1" in
            status|resume|rollback) ACTION=$1 ;;
            *) PUBLIC_URL=${1%/} ;;
        esac
        ;;
    2)
        PUBLIC_URL=${1%/}
        [ "$2" = "--build-from-source" ] || usage
        BUILD_FROM_SOURCE=true
        MODE=source
        SERVER_IMAGE=
        ;;
    *) usage ;;
esac

case "$ACTION" in
    status)
        show_status
        ;;
    rollback)
        rollback_install
        ;;
    resume)
        load_receipt
        resume_install
        ;;
    install)
        preflight_fresh_install
        resume_install
        ;;
    *) usage ;;
esac
