#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TAURI_TARGET="$ROOT/target/desktop-local-tauri"
OUTPUT_DIR="$ROOT/target/desktop-local-dist"

fail() {
    printf 'macOS Desktop local build failed: %s\n' "$*" >&2
    exit 1
}

usage() {
    cat <<'EOF'
Usage: scripts/build_desktop_macos_local.sh

Build an installable, ad-hoc signed macOS WebCodex Desktop DMG for local testing.
The bundled WebCodex runtime uses the dogfood Cargo profile; this is not a
formal release or publication workflow.

Output:
  target/desktop-local-dist/
EOF
}

if [ "${1:-}" = "--help" ] || [ "${1:-}" = "-h" ]; then
    usage
    exit 0
fi
if [ "$#" -ne 0 ]; then
    usage >&2
    fail "unexpected arguments"
fi
if [ "$(uname -s)" != "Darwin" ]; then
    fail "this helper is macOS-only"
fi

for command in git node npm cargo python3; do
    command -v "$command" >/dev/null 2>&1 || fail "required command not found: $command"
done
[ -x /usr/bin/lipo ] || fail "required command not found: /usr/bin/lipo"

cd "$ROOT"
if [ -n "$(git status --porcelain --untracked-files=all)" ]; then
    fail "worktree must be clean so bundled runtime identity is exact; commit or stash local changes first"
fi

SOURCE_SHA="$(git rev-parse HEAD)"
SHORT_SOURCE="$(git rev-parse --short=12 HEAD)"
VERSION="$(node -p "require('./npm/webcodex/package.json').version")"
BUILT_AT="$(git show -s --format=%ct HEAD)"
export WEBCODEX_BUILT_AT="$BUILT_AT"

case "$(uname -m)" in
    arm64)
        PLATFORM="darwin-arm64"
        ;;
    x86_64)
        PLATFORM="darwin-x64"
        ;;
    *)
        fail "unsupported Mac architecture: $(uname -m)"
        ;;
esac

printf 'Building WebCodex Desktop local DMG\n'
printf '  source:   %s\n' "$SOURCE_SHA"
printf '  version:  %s\n' "$VERSION"
printf '  platform: %s\n' "$PLATFORM"

npm ci --prefix apps/desktop

cargo build --locked --profile dogfood \
    -p webcodex \
    -p webcodex-cli \
    -p webcodex-runner

mkdir -p "$ROOT/target" "$OUTPUT_DIR" "$TAURI_TARGET"
WORK_DIR="$(mktemp -d "$ROOT/target/desktop-local-stage.XXXXXX")"
cleanup() { rm -rf "$WORK_DIR"; }
trap cleanup EXIT
STAGE_DIR="$WORK_DIR/bundle"

python3 scripts/prepare_desktop_bundle_macos.py \
    --bin-dir target/dogfood \
    --version "$VERSION" \
    --source-sha "$SOURCE_SHA" \
    --built-at "$BUILT_AT" \
    --platform "$PLATFORM" \
    --signing-mode adhoc \
    --output-dir "$STAGE_DIR"

# Keep Cargo/Tauri compilation caches between local builds, but remove stale
# bundle output so the candidate selection below is unambiguous.
rm -rf "$TAURI_TARGET/release/bundle/dmg"
export APPLE_SIGNING_IDENTITY="-"
export CARGO_TARGET_DIR="$TAURI_TARGET"

(
    cd apps/desktop
    npm exec tauri -- build \
        --bundles dmg \
        --config "$STAGE_DIR/tauri.bundle.conf.json" \
        --ci -- --locked
)

BUNDLE_DIR="$TAURI_TARGET/release/bundle/dmg"
shopt -s nullglob
candidates=("$BUNDLE_DIR"/*.dmg)
shopt -u nullglob
if [ "${#candidates[@]}" -ne 1 ]; then
    fail "expected exactly one generated DMG, found ${#candidates[@]} in $BUNDLE_DIR"
fi

OUTPUT_DMG="$OUTPUT_DIR/webcodex-desktop-local-$SHORT_SOURCE-v$VERSION-$PLATFORM.dmg"
cp "${candidates[0]}" "$OUTPUT_DMG"

bash scripts/desktop_install_macos_smoke.sh \
    --dmg "$OUTPUT_DMG" \
    --version "$VERSION" \
    --source-sha "$SOURCE_SHA" \
    --built-at "$BUILT_AT" \
    --platform "$PLATFORM" \
    --stage-metadata "$STAGE_DIR/desktop-bundle.json" \
    --signing-mode adhoc

DIGEST="$(shasum -a 256 "$OUTPUT_DMG" | awk '{print $1}')"
printf '\nLocal Desktop DMG ready:\n  %s\n' "$OUTPUT_DMG"
printf 'SHA-256: %s\n' "$DIGEST"
printf 'Open the output directory with:\n  open %q\n' "$OUTPUT_DIR"
