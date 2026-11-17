#!/bin/sh
REPO=${VIGIL_REPO:-/home/mason/repos/vigil}
DIR=${VIGIL_E2E_DIR:-/tmp}
BIN=${VIGIL_E2E_BIN:-$REPO/target/debug/vigil}
LOG=$DIR/vigil.log
: > "$LOG"
mkdir -p "$DIR/sessions"
printf '[Desktop Entry]\nName=Test DE\nExec=/bin/true\n' > "$DIR/sessions/test-de.desktop"
printf '[Desktop Entry]\nName=Other DE\nExec=/bin/false\n' > "$DIR/sessions/other-de.desktop"
export VIGIL_SESSION_DIRS=$DIR/sessions
export VIGIL_POWER_INHIBIT=1
python3 "$REPO/tests/e2e/fake_greetd.py" /tmp/greetd.sock hunter2 >>"$LOG" 2>&1 &
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
if [ -n "${VIGIL_E2E_CONFIG:-}" ]; then
    "$BIN" --socket /tmp/greetd.sock --config "$VIGIL_E2E_CONFIG" 2>>"$LOG"
else
    "$BIN" --socket /tmp/greetd.sock 2>>"$LOG"
fi
echo "VIGIL-EXIT:$?" | tee -a "$LOG"
