#!/bin/sh
set -eu

cd -- "$(dirname -- "$0")/.."
set -x
cargo fmt --all --check
python3 -I tests/gates/test_gate.py
python3 -I tests/gates/gate.py all
./tests/check-sim-safety.sh
./tests/check-warning-safety.sh
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
./tests/check-sim-scenario.sh
