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

if [ $# -eq 0 ]; then
    targets=$($cargo fuzz list) || { echo "fuzz: cargo fuzz list failed" >&2; exit 1; }
    [ -n "$targets" ] || { echo "fuzz: cargo fuzz list found no targets" >&2; exit 1; }
    # shellcheck disable=SC2086
    set -- $targets
fi
for target; do
    echo "fuzz: $target for ${seconds}s"
    mkdir -p "corpus/$target"
    $cargo fuzz run "$target" "corpus/$target" "seeds/$target" -- \
        -max_total_time="$seconds" -timeout=10 -verbosity=0 -print_final_stats=1
done
