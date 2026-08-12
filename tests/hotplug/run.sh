#!/bin/sh
# GPU unplug/replug under the greeter (issue #6), in QEMU via virtme-ng.
#
#   tests/hotplug/run.sh [workdir]
#
# Boots a two-GPU greeter (virtio-gpu + bochs-display), then PCI-unplugs
# the bochs card mid-session (QMP device_del — the guest kernel sees a real
# surprise removal, with udev events), checks the greeter survives and
# keeps rendering, hot-adds another display (device_add — a card born
# mid-session must be ignored politely, not crash the login path), and
# finishes a full login.
#
# Why bochs for the second card: QEMU refuses hotplug for virtio-gpu-pci
# ("does not support hotplugging"), but bochs-display both cold-plugs and
# hot-removes cleanly (verified empirically; DEVICE_DELETED fires).
#
# What this deliberately does NOT cover: connector-level hotplug on a card
# that stays present (the metal dock case). virtio-gpu cannot change a
# connector's state at runtime — no debugfs force, detect always says
# connected — so that half of #6 stays a metal checklist item.
set -eu
REPO=$(cd "$(dirname "$0")/../.." && pwd)
WORK=${1:-$(mktemp -d -p "${XDG_CACHE_HOME:-$HOME/.cache}" vigil-hotplug.XXXXXX)}
case $WORK in /tmp/*) echo "workdir must not be under /tmp (guest tmpfs)"; exit 2;; esac
mkdir -p "$WORK"
QMP=$WORK/qmp.sock
rm -f "$QMP"
# Device ids (gpu0/gpu1) are what device_del addresses.
VIGIL_REPO=$REPO VIGIL_E2E_DIR=$WORK vng --run --disable-microvm --user root -m 1G --cpus 2 \
    --rwdir="$WORK" \
    --qemu-opts="-device virtio-gpu-pci,id=gpu0 -device bochs-display,id=gpu1 -device usb-ehci -device usb-tablet -qmp unix:$QMP,server=on,wait=off" \
    -e "VIGIL_REPO=$REPO VIGIL_E2E_DIR=$WORK VIGIL_E2E_CONFIG=${VIGIL_E2E_CONFIG:-} $REPO/tests/e2e/guest.sh" &
VM=$!
for i in $(seq 120); do grep -q "vigil: output" "$WORK/vigil.log" 2>/dev/null && break; sleep 1; done
grep -q "vigil: output" "$WORK/vigil.log" 2>/dev/null || { echo "vigil never came up; see $WORK/vigil.log"; exit 1; }
sleep 2
python3 "$REPO/tests/hotplug/drive.py" "$QMP" "$WORK" 2560 || {
    echo "HOTPLUG FAIL: driver error ($WORK)"
    kill "$VM" 2>/dev/null; wait "$VM" 2>/dev/null || true
    exit 1
}
wait $VM || true
grep -q "VIGIL-EXIT:0" "$WORK/vigil.log" || { echo "HOTPLUG FAIL: no clean exit ($WORK)"; exit 1; }
# Keyboard-only login (no session picking): any started session proves the
# auth path survived the unplug/replug cycle.
grep -q "START_SESSION cmd=" "$WORK/vigil.log" || {
    echo "HOTPLUG FAIL: login after unplug did not complete ($WORK)"; exit 1; }
echo "HOTPLUG PASS ($WORK)"
