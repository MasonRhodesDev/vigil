#!/bin/sh
# Full end-to-end login test in QEMU via virtme-ng (host kernel, no image).
# Requires: qemu-system-x86-core, virtme-ng, seatd on the host rootfs, and a
# debug build (cargo build -p vigil).
#
#   tests/e2e/run.sh [workdir]
#
# PASSES when the guest log ends with VIGIL-EXIT:0 after drive.py walks the
# full greeter-spec flow (username stage, error path, Escape, mouse-driven
# session picker, correct password). Cannot run inside a desktop session's
# DRM devices — it doesn't need to; the guest has its own virtio-gpu.
#
# If QEMU fails with "Failed to initialize io_uring: Cannot allocate
# memory", the memlock rlimit is exhausted (per-user io_uring accounting);
# raise it for your shell first:  sudo prlimit --memlock=unlimited:unlimited --pid $$
set -eu
REPO=$(cd "$(dirname "$0")/../.." && pwd)
# The workdir must NOT live under /tmp: virtme overlays a fresh tmpfs on the
# guest's /tmp, which hides host paths there — the guest could never write
# its log back. ~/.cache is shared between host and guest.
WORK=${1:-$(mktemp -d -p "${XDG_CACHE_HOME:-$HOME/.cache}" vigil-e2e.XXXXXX)}
case $WORK in /tmp/*) echo "workdir must not be under /tmp (guest tmpfs)"; exit 2;; esac
mkdir -p "$WORK"
QMP=$WORK/qmp.sock
rm -f "$QMP"
VIGIL_REPO=$REPO VIGIL_E2E_DIR=$WORK vng --run --disable-microvm --user root -m 1G --cpus 2 \
    --rwdir="$WORK" \
    --qemu-opts="-device virtio-gpu-pci -device virtio-gpu-pci -device usb-ehci -device usb-tablet -qmp unix:$QMP,server=on,wait=off" \
    -e "VIGIL_REPO=$REPO VIGIL_E2E_DIR=$WORK $REPO/tests/e2e/guest.sh" &
VM=$!
for i in $(seq 120); do grep -q "vigil: output" "$WORK/vigil.log" 2>/dev/null && break; sleep 1; done
grep -q "vigil: output" "$WORK/vigil.log" 2>/dev/null || { echo "vigil never came up; see $WORK/vigil.log"; exit 1; }
sleep 2
# Two virtio-gpu devices -> two DRM cards -> the tablet's absolute axes
# span both outputs side by side (2560 global width).
python3 "$REPO/tests/e2e/drive.py" "$QMP" "$WORK" 2560
wait $VM || true
# Full-spec pass: clean exit, both GPUs' outputs lit, AND the session
# picked by mouse (Test DE -> /bin/true, not the default-sorted Other DE ->
# /bin/false) was started.
grep -q "VIGIL-EXIT:0" "$WORK/vigil.log" || { echo "E2E FAIL: no clean exit ($WORK)"; exit 1; }
[ "$(grep -c '^vigil: output' "$WORK/vigil.log")" -ge 2 ] || {
    echo "E2E FAIL: expected outputs on both GPUs ($WORK)"; exit 1; }
grep -q "START_SESSION cmd=\['/bin/true'\]" "$WORK/vigil.log" || {
    echo "E2E FAIL: picked session was not started ($WORK)"; exit 1; }
echo "E2E PASS ($WORK)"
