#!/usr/bin/env bash
set -uo pipefail

DIR=$(cd "$(dirname "$0")/../.." && pwd)
BIN=${VIGIL_VT_BIN:-$DIR/target/release/vigil}
[ -x "$BIN" ] || BIN=$DIR/target/debug/vigil
SOCK=${XDG_RUNTIME_DIR:-/tmp}/vigil-vt-test.sock
PASSWORD=${VIGIL_TEST_PASSWORD:-vigil-test}
LOG=${VIGIL_VT_LOG:-$HOME/.cache/vigil-vt-test}
LIMIT=${VIGIL_VT_TIMEOUT:-180}

CTTY=$(ps -o tty= -p $$ | tr -d ' ')
if [[ ${CTTY} != tty[0-9]* && ${XDG_SESSION_TYPE:-} != tty && -z ${XDG_VTNR:-} ]]; then
    echo "warning: this does not look like a virtual console login" >&2
    echo "  (ctty=${CTTY:-?} XDG_SESSION_TYPE=${XDG_SESSION_TYPE:-unset} XDG_VTNR=${XDG_VTNR:-unset})" >&2
    echo "  continuing anyway — vigil will fail cleanly if the seat is unavailable" >&2
fi
[ -x "$BIN" ] || { echo "build first: cargo build --release -p vigil" >&2; exit 1; }
echo "vigil-vt: using $BIN"

mkdir -p "$(dirname "$LOG")"
python3 "$DIR/tests/e2e/fake_greetd.py" "$SOCK" "$PASSWORD" >"$LOG.greetd.log" 2>&1 &
GREETD=$!
trap 'kill "$GREETD" 2>/dev/null' EXIT

echo "vigil-vt: starting (auto-kill after ${LIMIT}s; password: $PASSWORD)"
export VIGIL_POWER_INHIBIT=1
CONFIG_ARGS=()
[ -n "${VIGIL_VT_CONFIG:-}" ] && CONFIG_ARGS=(--config "$VIGIL_VT_CONFIG")
timeout --foreground "$LIMIT" \
    "$BIN" --socket "$SOCK" "${CONFIG_ARGS[@]}" >"$LOG.log" 2>&1
RC=$?

echo "vigil-vt: vigil exited $RC (124 = timeout kill)"
echo "--- vigil ---"
tail -n 20 "$LOG.log"
echo "--- fake greetd ---"
tail -n 10 "$LOG.greetd.log"
echo "--- renderer ---"
grep -E "rendering with GL|GL unavailable" "$LOG.log" || echo "software (no GL lines)"
if [ "$RC" -eq 0 ] && grep -q START_SESSION "$LOG.greetd.log"; then
    echo "vigil-vt: PASS — full login completed on metal"
else
    echo "vigil-vt: FAIL — see $LOG.log"
fi
