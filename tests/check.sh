#!/bin/sh
set -eu

cd -- "$(dirname -- "$0")/.."
set -x
cargo fmt --all --check
./tests/check-unsafe-fence.sh
./tests/check-advisory-expiry.sh
./tests/check-sim-safety.sh
./tests/check-warning-safety.sh
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
./tests/check-sim-scenario.sh
