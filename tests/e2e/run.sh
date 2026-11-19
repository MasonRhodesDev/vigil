#!/bin/sh
set -eu
REPO=$(cd "$(dirname "$0")/../.." && pwd)
WORK=${1:-$(mktemp -d -p "${XDG_CACHE_HOME:-$HOME/.cache}" vigil-e2e.XXXXXX)}
case $WORK in /tmp/*) echo "workdir must not be under /tmp (guest tmpfs)"; exit 2;; esac
mkdir -p "$WORK"
QMP=$WORK/qmp.sock
rm -f "$QMP"
VIGIL_REPO=$REPO VIGIL_E2E_DIR=$WORK vng --run ${VIGIL_VM_KERNEL:-} --disable-microvm --user root -m 1G --cpus 2 \
    --rwdir="$WORK" \
    --qemu-opts="-device virtio-gpu-pci,id=gpu0 -device virtio-gpu-pci,id=gpu1 -device usb-ehci -device usb-tablet -qmp unix:$QMP,server=on,wait=off" \
    -e "VIGIL_REPO=$REPO VIGIL_E2E_DIR=$WORK VIGIL_E2E_CONFIG=${VIGIL_E2E_CONFIG:-} VIGIL_E2E_BIN=${VIGIL_E2E_BIN:-} $REPO/tests/e2e/guest.sh" &
VM=$!
for i in $(seq 120); do grep -q "vigil: output" "$WORK/vigil.log" 2>/dev/null && break; sleep 1; done
grep -q "vigil: output" "$WORK/vigil.log" 2>/dev/null || { echo "vigil never came up; see $WORK/vigil.log"; exit 1; }
sleep 2
python3 "$REPO/tests/e2e/drive.py" "$QMP" "$WORK" || {
    echo "E2E FAIL: driver error ($WORK)"
    kill "$VM" 2>/dev/null; wait "$VM" 2>/dev/null || true
    exit 1
}
wait $VM || true
grep -q "VIGIL-EXIT:0" "$WORK/vigil.log" || { echo "E2E FAIL: no clean exit ($WORK)"; exit 1; }
[ "$(grep -c '^vigil: output' "$WORK/vigil.log")" -ge 2 ] || {
    echo "E2E FAIL: expected outputs on both GPUs ($WORK)"; exit 1; }
grep -q "START_SESSION cmd=\['/bin/true'\]" "$WORK/vigil.log" || {
    echo "E2E FAIL: picked session was not started ($WORK)"; exit 1; }
python3 "$REPO/tests/e2e/check_frames.py" "$WORK" || {
    echo "E2E FAIL: frame assertions ($WORK)"; exit 1; }
echo "E2E PASS ($WORK)"
