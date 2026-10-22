#!/bin/sh
# Three-head scene run in QEMU via virtme-ng: the splash lights every screen
# together, the greeter takes over in one crossfade, and the screens settle
# on the rest frame after login. Needs the same things as run.sh plus a
# release build (cargo build --release -p vigil), because the splash is drawn
# by the CPU and a debug build misstates its timing.
#
#   tests/e2e/run-scene.sh [workdir]
#
# VIGIL_E2E_SCENE sets the scene file the guest reads (default: the repo's
# tests/fixtures/scene.json, so no desktop-boot install is needed).
# VIGIL_E2E_LOGO sets the mark (default: tests/fixtures/mark.svg).
# VIGIL_E2E_KEEP_STATE=1 turns on [render] keep_display_state.
# VIGIL_E2E_TAGS=env asks for the stage tags through DEBUG_STAGE_TAGS.
# VIGIL_E2E_KARGS adds guest kernel arguments (drm.debug=0x14 makes the
# kernel say, in dmesg, whether vigil's first commit was a full modeset).
# VIGIL_E2E_SECONDS bounds the watch (default 40).
# VIGIL_E2E_BG sets the greeter wallpaper (default: a generated noisy
# gradient that takes a while to decode, so the glow has time to breathe).
set -eu
REPO=$(cd "$(dirname "$0")/../.." && pwd)
WORK=${1:-$(mktemp -d -p "${XDG_CACHE_HOME:-$HOME/.cache}" vigil-scene.XXXXXX)}
case $WORK in /tmp/*) echo "workdir must not be under /tmp (guest tmpfs)"; exit 2;; esac
mkdir -p "$WORK"
SCENE=${VIGIL_E2E_SCENE:-$REPO/tests/fixtures/scene.json}
LOGO=${VIGIL_E2E_LOGO:-$REPO/tests/fixtures/mark.svg}
mkdir -p "$WORK/profiles"
cat > "$WORK/profiles/scene.toml" <<TOML
description="scene run: three heads, the third turned to portrait"
match=["Red Hat Inc. QEMU Monitor"]
[[monitor]]
output="Virtual-1"
position=[0,0]
[[monitor]]
output="Virtual-2"
position=[2560,0]
[[monitor]]
output="Virtual-3"
position=[3840,0]
transform=3
TOML
{
    echo "[profiles]"
    echo "dir = \"$WORK/profiles\""
    echo "[splash]"
    echo "scene = \"$SCENE\""
    echo "logo = \"$LOGO\""
    echo "on_first_frame = \"touch $WORK/first-frame-hook\""
    # With VIGIL_E2E_TAGS=env the tags are asked for through the scene's
    # environment variable (the guest exports it) instead of the config key.
    [ "${VIGIL_E2E_TAGS:-key}" = env ] || echo "debug_tags = true"
    echo "[render]"
    echo "backend = \"${VIGIL_E2E_BACKEND:-software}\""
    [ -n "${VIGIL_E2E_KEEP_STATE:-}" ] && echo "keep_display_state = true"
} > "$WORK/vigil.toml"
# A wallpaper the greeter has to decode, so the splash has something to wait
# for and the cross-fade has a picture to fade to.
BG=${VIGIL_E2E_BG:-$WORK/wallpaper.png}
[ -f "$BG" ] || python3 - "$BG" <<'PY'
import sys
import numpy as np
from PIL import Image
w, h = 2560, 1440
y, x = np.mgrid[0:h, 0:w]
img = np.stack([40 + 90 * x / w, 70 + 100 * y / h, 130 + 60 * (x + y) / (w + h)], axis=-1)
img += np.random.default_rng(1).normal(0, 6, img.shape)
Image.fromarray(np.clip(img, 0, 255).astype("uint8")).save(sys.argv[1])
PY
printf '[look]\nbackground = "%s"\n' "$BG" >> "$WORK/vigil.toml"
QMP=$WORK/qmp.sock
rm -f "$QMP"
# Three virtio-gpu cards of one head each, sized by EDID: an ultrawide, a
# 16:9 and a 4K panel that the profile turns to portrait (transform 3), so
# min(W,H) differs from H on a screen and one screen is rotated.
# (QEMU only enables a second head on a card when a UI that supports resizing
# is attached, which a headless run has not got.)
VIGIL_REPO=$REPO VIGIL_E2E_DIR=$WORK vng --run ${VIGIL_VM_KERNEL:-} ${VIGIL_E2E_KARGS:+--append "$VIGIL_E2E_KARGS"} --disable-microvm --user root -m 3G --cpus 4 \
    --rwdir="$WORK" \
    --qemu-opts="-device virtio-gpu-pci,id=gpu0,edid=on,xres=2560,yres=1080 -device virtio-gpu-pci,id=gpu1,edid=on,xres=1280,yres=720 -device virtio-gpu-pci,id=gpu2,edid=on,xres=3840,yres=2160 -device usb-ehci -device usb-tablet -qmp unix:$QMP,server=on,wait=off" \
    -e "VIGIL_REPO=$REPO VIGIL_E2E_DIR=$WORK VIGIL_E2E_CONFIG=$WORK/vigil.toml VIGIL_E2E_BIN=${VIGIL_E2E_BIN:-} VIGIL_E2E_TAGS=${VIGIL_E2E_TAGS:-key} $REPO/tests/e2e/guest-scene.sh" &
VM=$!
python3 "$REPO/tests/e2e/scene-driver.py" "$QMP" "$WORK" || {
    echo "SCENE FAIL: driver error ($WORK)"
    kill "$VM" 2>/dev/null; wait "$VM" 2>/dev/null || true
    exit 1
}
wait $VM || true
grep -q "VIGIL-EXIT:0" "$WORK/vigil.log" || { echo "SCENE FAIL: no clean exit ($WORK)"; exit 1; }
python3 "$REPO/tests/e2e/scene-check.py" "$WORK"
