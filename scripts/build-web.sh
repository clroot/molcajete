#!/usr/bin/env bash
# Builds the web client into web/dist: the wasm client and its JS bindings
# (web/public/pkg), then the React UI (bun).
#   PROFILE=dev scripts/build-web.sh      # fast, unoptimized wasm
#   scripts/build-web.sh                  # optimized (default)
#   SKIP_UI=1 scripts/build-web.sh        # wasm only (e.g. for `bun run dev`)
set -euo pipefail
cd "$(dirname "$0")/.."

PROFILE="${PROFILE:-wasm-release}"
case "$PROFILE" in
  dev) TARGET_DIR=debug ;;
  release) TARGET_DIR=release ;;
  *) TARGET_DIR="$PROFILE" ;;
esac

PKG=web/public/pkg
cargo build -p molcajete-client --target wasm32-unknown-unknown --profile "$PROFILE"
wasm-bindgen --target web --no-typescript --out-dir "$PKG" \
  "target/wasm32-unknown-unknown/$TARGET_DIR/molcajete_client.wasm"

if command -v wasm-opt >/dev/null 2>&1 && [ "$PROFILE" != dev ]; then
  wasm-opt -Os --enable-bulk-memory --enable-nontrapping-float-to-int \
    -o "$PKG/molcajete_client_bg.wasm" "$PKG/molcajete_client_bg.wasm"
fi
ls -l "$PKG"

if [ -z "${SKIP_UI:-}" ]; then
  (cd web && bun install --frozen-lockfile && bun run build)
fi
