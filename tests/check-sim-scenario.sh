#!/bin/sh
set -eu

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

for fixture in warning-commit warning-cancel warning-hotplug tint-only; do
    first="$work/$fixture.first.json"
    second="$work/$fixture.second.json"
    cargo run --quiet --manifest-path "$root/Cargo.toml" -p vigil-sim -- \
        scenario "$root/tests/fixtures/sim/$fixture.toml" >"$first"
    cargo run --quiet --manifest-path "$root/Cargo.toml" -p vigil-sim -- \
        scenario "$root/tests/fixtures/sim/$fixture.toml" >"$second"
    cmp "$first" "$second"
    jq -e '.frame_hash | length == 16' "$first" >/dev/null
done

jq -e '.mode == "Lock" and .warning_phase == null' \
    "$work/warning-commit.first.json" >/dev/null
jq -e '.mode == "Login"' "$work/warning-cancel.first.json" >/dev/null
jq -e '.mode == "Login"' "$work/warning-hotplug.first.json" >/dev/null
jq -e '.mode == "Warning" and .warning_phase == "PreLock"' \
    "$work/tint-only.first.json" >/dev/null
echo "vigil-sim scenario is deterministic"
