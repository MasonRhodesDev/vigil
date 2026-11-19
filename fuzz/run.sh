#!/bin/sh
# usage: fuzz/run.sh [seconds per target] [target...]
set -eu
cd "$(dirname "$0")"

seconds=${1:-300}
[ $# -eq 0 ] || shift

command -v cargo-fuzz >/dev/null 2>&1 || {
    echo "cargo-fuzz is missing: cargo install cargo-fuzz --locked" >&2
    exit 2
}
if cargo +nightly --version >/dev/null 2>&1; then
    cargo="cargo +nightly"
else
    # A stable toolchain that ships the sanitizer runtimes builds the targets too.
    export RUSTC_BOOTSTRAP=1
    cargo=cargo
fi

# shellcheck disable=SC2046
[ $# -gt 0 ] || set -- $($cargo fuzz list)
for target; do
    echo "fuzz: $target for ${seconds}s"
    mkdir -p "corpus/$target"
    $cargo fuzz run "$target" "corpus/$target" "seeds/$target" -- \
        -max_total_time="$seconds" -timeout=10 -print_final_stats=1
done
