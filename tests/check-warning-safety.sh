#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)

if rg -n 'screencopy|image_copy_capture|image-copy-capture|portal.*capture' \
    "$root/crates/vigil-wayland" "$root/crates/vigil-lock"; then
    echo 'production warning path must not bind a capture protocol' >&2
    exit 1
fi

rg -q 'BackgroundEffectState' "$root/crates/vigil-wayland/src/lib.rs"
rg -q 'Format::Argb8888' "$root/crates/vigil-wayland/src/lib.rs"
rg -q 'Layer::Overlay' "$root/crates/vigil-wayland/src/lib.rs"
echo 'vigil warning protocol boundary is capture-free'
