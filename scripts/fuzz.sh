#!/usr/bin/env bash
# Runs the fuzz targets in fuzz/ for a while each (nightly toolchain and
# cargo-fuzz: `rustup toolchain install nightly`, `cargo install cargo-fuzz`).
#
#   scripts/fuzz.sh [seconds per target] [target ...]
#
# Without targets, runs them all. The committed seeds (fuzz/seeds/<target>)
# are read, and what the fuzzer finds goes to fuzz/corpus/<target>, which is
# not committed. A crash leaves its input in fuzz/artifacts/<target>/ and
# makes the script fail once every target has run.
set -euo pipefail

cd "$(dirname "$0")/.."
seconds="${1:-60}"
shift || true
if [ $# -eq 0 ]; then
    set -- $(cargo +nightly fuzz list --fuzz-dir fuzz)
fi

failed=()
for target in "$@"; do
    echo "== $target (${seconds}s)"
    mkdir -p "fuzz/corpus/$target"
    # Allocations above 512 MiB count as crashes: no input this short should
    # make a parser allocate that much.
    if ! cargo +nightly fuzz run --fuzz-dir fuzz "$target" \
        "fuzz/corpus/$target" "fuzz/seeds/$target" -- \
        -max_total_time="$seconds" -max_len=16384 \
        -malloc_limit_mb=512 -rss_limit_mb=2048 -print_final_stats=1; then
        failed+=("$target")
    fi
done

if [ ${#failed[@]} -gt 0 ]; then
    echo "crashed: ${failed[*]} (inputs in fuzz/artifacts/)" >&2
    exit 1
fi
