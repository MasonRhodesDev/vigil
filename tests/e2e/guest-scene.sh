#!/bin/sh
# Runs INSIDE the virtme-ng guest (root, host rootfs read-only). The scene
# run's counterpart of guest.sh: a fixed fake greetd, the splash and scene
# settings from $VIGIL_E2E_DIR, and DEBUG_STAGE_TAGS on so the frames say
# which stage owns each screen.
REPO=${VIGIL_REPO:?}
DIR=${VIGIL_E2E_DIR:?}
BIN=${VIGIL_E2E_BIN:-$REPO/target/release/vigil}
LOG=$DIR/vigil.log
: > "$LOG"
mkdir -p "$DIR/sessions"
printf '[Desktop Entry]\nName=Test DE\nExec=/bin/true\n' > "$DIR/sessions/test-de.desktop"
export VIGIL_SESSION_DIRS=$DIR/sessions
export VIGIL_POWER_INHIBIT=1
python3 "$REPO/tests/e2e/fake_greetd.py" /tmp/greetd.sock hunter2 >>"$LOG" 2>&1 &
sleep 0.5
for i in $(seq 100); do [ -e /dev/dri/card0 ] && break; sleep 0.2; done
command -v udevadm >/dev/null && { udevadm trigger --action=add 2>/dev/null; udevadm settle 2>/dev/null; }
seatd 2>>"$LOG" &
for i in $(seq 50); do [ -e /run/seatd.sock ] && break; sleep 0.1; done
export LIBSEAT_BACKEND=seatd
[ "${VIGIL_E2E_TAGS:-key}" = env ] && export DEBUG_STAGE_TAGS=1
# The cue the host driver waits for; it then watches the screens from t=0.
echo "VIGIL-START" >> "$LOG"
"$BIN" --socket /tmp/greetd.sock --config "$VIGIL_E2E_CONFIG" 2>>"$LOG"
RC=$?
dmesg > "$DIR/dmesg.txt" 2>/dev/null
echo "VIGIL-EXIT:$RC" | tee -a "$LOG"
