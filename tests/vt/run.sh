#!/usr/bin/env bash
# On-metal VT harness: run vigil against a fake greetd from a spare virtual
# console. Real GPU modeset, real libinput — no VM, no changes to the real
# greetd config. Log in on a free VT (Ctrl+Alt+F3) and run this script there;
# it must NOT run under a compositor (libseat only grants the seat to the
# active VT's session).
#
# The whole run is wrapped in `timeout` so a wedged greeter can never hold
# the display/input hostage: worst case the screen comes back by itself.
# Fake credentials: any username works, the accepted password is "vigil-test"
# (override with VIGIL_TEST_PASSWORD). Try a wrong one first — you should get
# the red error and a fresh prompt. A successful login just logs
# START_SESSION and exits; it starts nothing.
set -uo pipefail

DIR=$(cd "$(dirname "$0")/../.." && pwd)
BIN=$DIR/target/debug/vigil
SOCK=${XDG_RUNTIME_DIR:-/tmp}/vigil-vt-test.sock
PASSWORD=${VIGIL_TEST_PASSWORD:-vigil-test}
LOG=${VIGIL_VT_LOG:-$HOME/.cache/vigil-vt-test}
LIMIT=${VIGIL_VT_TIMEOUT:-180}

# Best-effort VT detection: any one signal is enough. libseat is the real
# gatekeeper — if this session can't take the seat, vigil exits with a clear
# error and the script reports FAIL — so a misread here only costs a warning.
CTTY=$(ps -o tty= -p $$ | tr -d ' ')
if [[ ${CTTY} != tty[0-9]* && ${XDG_SESSION_TYPE:-} != tty && -z ${XDG_VTNR:-} ]]; then
    echo "warning: this does not look like a virtual console login" >&2
    echo "  (ctty=${CTTY:-?} XDG_SESSION_TYPE=${XDG_SESSION_TYPE:-unset} XDG_VTNR=${XDG_VTNR:-unset})" >&2
    echo "  continuing anyway — vigil will fail cleanly if the seat is unavailable" >&2
fi
[ -x "$BIN" ] || { echo "build first: cargo build -p vigil" >&2; exit 1; }

mkdir -p "$(dirname "$LOG")"
python3 "$DIR/tests/e2e/fake_greetd.py" "$SOCK" "$PASSWORD" >"$LOG.greetd.log" 2>&1 &
GREETD=$!
trap 'kill "$GREETD" 2>/dev/null' EXIT

echo "vigil-vt: starting (auto-kill after ${LIMIT}s; password: $PASSWORD)"
# No --user and no --cmd: the full greeter-spec flow. Type any username at
# the first prompt (the fake greetd accepts all of them), pick a session
# with the mouse, and use Ctrl+Alt+Fn to switch VTs at any time. Power
# buttons are inhibited — this is a test, not a real login.
export VIGIL_POWER_INHIBIT=1
timeout --foreground "$LIMIT" \
    "$BIN" --socket "$SOCK" >"$LOG.log" 2>&1
RC=$?

echo "vigil-vt: vigil exited $RC (124 = timeout kill)"
echo "--- vigil ---"
tail -n 20 "$LOG.log"
echo "--- fake greetd ---"
tail -n 10 "$LOG.greetd.log"
if [ "$RC" -eq 0 ] && grep -q START_SESSION "$LOG.greetd.log"; then
    echo "vigil-vt: PASS — full login completed on metal"
else
    echo "vigil-vt: FAIL — see $LOG.log"
fi
