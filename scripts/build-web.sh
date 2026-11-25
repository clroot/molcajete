#!/usr/bin/env bash
# Builds the wasm client and generates JS bindings into web/pkg.
#   PROFILE=dev scripts/build-web.sh      # fast, unoptimized
#   scripts/build-web.sh                  # optimized (default)
set -euo pipefail
cd "$(dirname "$0")/.."

PROFILE="${PROFILE:-wasm-release}"
case "$PROFILE" in
  dev) TARGET_DIR=debug ;;
  release) TARGET_DIR=release ;;
  *) TARGET_DIR="$PROFILE" ;;
esac

cargo build -p molcajete-client --target wasm32-unknown-unknown --profile "$PROFILE"
wasm-bindgen --target web --no-typescript --out-dir web/pkg \
  "target/wasm32-unknown-unknown/$TARGET_DIR/molcajete_client.wasm"

if command -v wasm-opt >/dev/null 2>&1 && [ "$PROFILE" != dev ]; then
  wasm-opt -Os --enable-bulk-memory --enable-nontrapping-float-to-int \
    -o web/pkg/molcajete_client_bg.wasm web/pkg/molcajete_client_bg.wasm
fi

ls -l web/pkg
