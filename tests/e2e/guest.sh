#!/bin/sh
# Runs INSIDE the virtme-ng guest (root, host rootfs read-only).
# $VIGIL_E2E_DIR must be a --rwdir for logs; repo root is read from $VIGIL_REPO.
REPO=${VIGIL_REPO:-/home/mason/repos/vigil}
DIR=${VIGIL_E2E_DIR:-/tmp}
LOG=$DIR/vigil.log
: > "$LOG"
# Controlled session list so the picker renders and start_session is
# deterministic regardless of what the host has installed.
mkdir -p "$DIR/sessions"
printf '[Desktop Entry]\nName=Test DE\nExec=/bin/true\n' > "$DIR/sessions/test-de.desktop"
printf '[Desktop Entry]\nName=Other DE\nExec=/bin/false\n' > "$DIR/sessions/other-de.desktop"
export VIGIL_SESSION_DIRS=$DIR/sessions
# A stray click on a power button must log, not kill the VM mid-test.
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
# No --user, no --cmd: the full greeter-spec flow (username stage + session
# list). "Other DE" sorts first, so the driver cycling once should land on
# "Test DE" -> /bin/true; without cycling, start cmd is /bin/false, which the
# fake greetd still accepts (it only logs) — run.sh asserts the logged cmd.
# $VIGIL_E2E_CONFIG lets a run pick a non-default config -- the GL path is
# selected that way, so the same flow can be driven on either renderer.
if [ -n "${VIGIL_E2E_CONFIG:-}" ]; then
    "$REPO/target/debug/vigil" --socket /tmp/greetd.sock --config "$VIGIL_E2E_CONFIG" 2>>"$LOG"
else
    "$REPO/target/debug/vigil" --socket /tmp/greetd.sock 2>>"$LOG"
fi
echo "VIGIL-EXIT:$?" | tee -a "$LOG"
