#!/bin/sh
set -eu
REPO=$(cd "$(dirname "$0")/../.." && pwd)
WORK=${1:-$(mktemp -d -p "${XDG_CACHE_HOME:-$HOME/.cache}" vigil-scene.XXXXXX)}
case $WORK in /tmp/*) echo "workdir must not be under /tmp (guest tmpfs)"; exit 2;; esac
mkdir -p "$WORK"
SCENE=${VIGIL_E2E_SCENE:-$REPO/tests/fixtures/scene.json}
LOGO=${VIGIL_E2E_LOGO:-$REPO/tests/fixtures/mark.svg}
mkdir -p "$WORK/profiles"
python3 - "$SCENE" "$WORK/scene.json" "${VIGIL_E2E_REF:-}" <<'PY'
import json, sys
d = json.load(open(sys.argv[1]))
if sys.argv[3]:
    d["ref_short_side"] = int(sys.argv[3])
json.dump(d, open(sys.argv[2], "w"), indent=2)
PY
SCENE=$WORK/scene.json
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
    [ "${VIGIL_E2E_TAGS:-key}" = env ] || echo "debug_tags = true"
    echo "[render]"
    echo "backend = \"${VIGIL_E2E_BACKEND:-software}\""
    [ -n "${VIGIL_E2E_KEEP_STATE:-}" ] && echo "keep_display_state = true"
    [ -n "${VIGIL_E2E_HOLD:-}" ] && echo "hold_last_frame_on_exit = true"
    [ -n "${VIGIL_E2E_HOLD_MS:-}" ] && echo "hold_timeout_ms = $VIGIL_E2E_HOLD_MS"
} > "$WORK/vigil.toml"
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
VIGIL_REPO=$REPO VIGIL_E2E_DIR=$WORK vng --run ${VIGIL_VM_KERNEL:-} ${VIGIL_E2E_KARGS:+--append "$VIGIL_E2E_KARGS"} --disable-microvm --user root -m 3G --cpus 4 \
    --rwdir="$WORK" \
    --qemu-opts="-device virtio-gpu-pci,id=gpu0,edid=on,xres=2560,yres=1080 -device virtio-gpu-pci,id=gpu1,edid=on,xres=1280,yres=720 -device virtio-gpu-pci,id=gpu2,edid=on,xres=3840,yres=2160 -device usb-ehci -device usb-tablet -qmp unix:$QMP,server=on,wait=off" \
    -e "VIGIL_REPO=$REPO VIGIL_E2E_DIR=$WORK VIGIL_E2E_CONFIG=$WORK/vigil.toml VIGIL_E2E_BIN=${VIGIL_E2E_BIN:-} VIGIL_E2E_TAGS=${VIGIL_E2E_TAGS:-key} VIGIL_E2E_NEXT=${VIGIL_E2E_NEXT:-} VIGIL_E2E_NEXT_DELAY=${VIGIL_E2E_NEXT_DELAY:-3} VIGIL_E2E_NOFBCON=${VIGIL_E2E_NOFBCON:-} $REPO/tests/e2e/guest-scene.sh" &
VM=$!
python3 "$REPO/tests/e2e/scene-driver.py" "$QMP" "$WORK" || {
    echo "SCENE FAIL: driver error ($WORK)"
    kill "$VM" 2>/dev/null; wait "$VM" 2>/dev/null || true
    exit 1
}
wait $VM || true
grep -q "VIGIL-EXIT:0" "$WORK/vigil.log" || { echo "SCENE FAIL: no clean exit ($WORK)"; exit 1; }
cleanup() { [ -n "${VIGIL_E2E_KEEP_FRAMES:-}" ] || rm -f "$WORK"/f[0-9]*.ppm; }
trap cleanup EXIT
if [ -n "${VIGIL_E2E_NEXT:-}" ]; then
    python3 "$REPO/tests/e2e/hold-check.py" "$WORK" "${VIGIL_E2E_HOLD:+hold}"
else
    python3 "$REPO/tests/e2e/scene-check.py" "$WORK"
fi
