#!/bin/sh
# Runs INSIDE the virtme-ng guest (root, host rootfs read-only).
# $VIGIL_E2E_DIR must be a --rwdir for logs; repo root is read from $VIGIL_REPO.
REPO=${VIGIL_REPO:-/home/mason/repos/vigil}
DIR=${VIGIL_E2E_DIR:-/tmp}
LOG=$DIR/vigil.log
: > "$LOG"
python3 "$REPO/tests/e2e/fake_greetd.py" /tmp/greetd.sock hunter2 &
sleep 0.5
for i in $(seq 100); do [ -e /dev/dri/card0 ] && break; sleep 0.2; done
command -v udevadm >/dev/null && { udevadm trigger --action=add 2>/dev/null; udevadm settle 2>/dev/null; }
if command -v seatd >/dev/null; then
    seatd 2>>"$LOG" &
    for i in $(seq 50); do [ -e /run/seatd.sock ] && break; sleep 0.1; done
    export LIBSEAT_BACKEND=seatd
else
    export LIBSEAT_BACKEND=builtin
fi
"$REPO/target/debug/vigil" --user demo --socket /tmp/greetd.sock --cmd /bin/true 2>>"$LOG"
echo "VIGIL-EXIT:$?" | tee -a "$LOG"
