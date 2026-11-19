#!/bin/sh
set -eu
REPO=$(cd "$(dirname "$0")/../.." && pwd)

command -v sway >/dev/null 2>&1 || { echo "SKIP: sway not installed"; exit 0; }
command -v python3 >/dev/null 2>&1 || { echo "SKIP: python3 not installed"; exit 0; }

TARGET=$(cargo metadata --format-version 1 --no-deps --manifest-path "$REPO/Cargo.toml" |
    python3 -I -c 'import json, sys; print(json.load(sys.stdin)["target_directory"])')
VLOCK=$TARGET/debug/vigil-lock
PROBE=$TARGET/debug/examples/lock_probe
[ -x "$VLOCK" ] && [ -x "$PROBE" ] || {
    echo "build first: cargo build -p vigil-lock && cargo build -p vigil-wayland --example lock_probe"
    exit 2
}

WORK=$(mktemp -d)
SWAY_PID=
stop_session() {
    pkill -f "^$VLOCK " 2>/dev/null || true
    [ -n "$SWAY_PID" ] && kill "$SWAY_PID" 2>/dev/null && wait "$SWAY_PID" 2>/dev/null || true
    SWAY_PID=
}
cleanup() {
    stop_session
    rm -rf "$WORK"
}
trap cleanup EXIT INT TERM

export XDG_CONFIG_HOME=$WORK/config
mkdir -p "$XDG_CONFIG_HOME"
CFG=$WORK/vigil.toml
: >"$CFG"
export WLR_BACKENDS=headless WLR_LIBINPUT_NO_DEVICES=1 WLR_RENDERER=pixman

fail() { echo "FAIL: $*"; exit 1; }
pass() { echo "  ok: $*"; }

start_session() {
    export XDG_RUNTIME_DIR=$WORK/run-$1
    mkdir -p "$XDG_RUNTIME_DIR"
    chmod 700 "$XDG_RUNTIME_DIR"
    sway -c "$REPO/tests/nested/sway.cfg" >"$WORK/sway-$1.log" 2>&1 &
    SWAY_PID=$!
    i=0
    while :; do
        for sock in "$XDG_RUNTIME_DIR"/wayland-*; do
            [ -S "$sock" ] && break 2
        done
        i=$((i + 1))
        [ $i -gt 100 ] && fail "$1: sway socket never appeared"
        sleep 0.1
    done
    WAYLAND_DISPLAY=$(basename "$sock")
    export WAYLAND_DISPLAY
}
sm() { swaymsg -s "$(echo "$XDG_RUNTIME_DIR"/sway-ipc.*.sock)" "$@"; }
probe_state() { "$PROBE" >/dev/null 2>&1 && echo 0 || echo $?; }
locker_pid() { pgrep -nf "^$VLOCK " || true; }
parked_in_pipe() { cat /proc/"$1"/task/*/wchan 2>/dev/null | grep -q 'pipe_write$'; }

READERLESS='import os, subprocess, sys
r, w = os.pipe()
os.close(r)
sys.exit(subprocess.call(sys.argv[1:], stderr=w))'

lock_with_stderr() {
    case $1 in
        file) timeout 30 "$VLOCK" --config "$CFG" --wait --no-warn --grace 300 2>"$WORK/control.log" ;;
        full) timeout 30 "$VLOCK" --config "$CFG" --wait --no-warn --grace 300 2>/dev/full ;;
        readerless) timeout 30 python3 -I -c "$READERLESS" "$VLOCK" --config "$CFG" --wait --no-warn --grace 300 ;;
        stalled) timeout 30 "$VLOCK" --config "$CFG" --wait --no-warn --grace 300 2>"$WORK/stalled.fifo" ;;
    esac
}

scenario() {
    echo "$1: stderr $2"
    start_session "$1"
    [ "$(probe_state)" = 0 ] || fail "$1: fresh session reports locked"
    lock_with_stderr "$2" && rc=0 || rc=$?
    [ "$rc" = 0 ] || fail "$1: --wait exited $rc, the session was never locked"
    [ "$(probe_state)" = 10 ] || fail "$1: --wait returned 0 but the probe says unlocked"
    pid=$(locker_pid)
    [ -n "$pid" ] || fail "$1: no locker left holding the lock"
    before=$(wc -l <"$WORK/control.log" 2>/dev/null || echo 0)
    sm create_output >/dev/null
    sleep 1
    kill -0 "$pid" 2>/dev/null || fail "$1: the locker died after a post-lock log line; session stranded"
    [ "$(probe_state)" = 10 ] || fail "$1: session no longer locked"
    ! parked_in_pipe "$pid" || fail "$1: the locker is parked writing stderr"
    if [ "$2" = file ]; then
        after=$(wc -l <"$WORK/control.log")
        [ "$after" -gt "$before" ] || fail "$1: create_output logged nothing, so the other scenarios prove nothing"
    fi
    stop_session
    pass "$1"
}

scenario L1 file
scenario L2 full
scenario L3 readerless

mkfifo "$WORK/stalled.fifo"
exec 7<>"$WORK/stalled.fifo"
python3 -I -c '
import os, sys
fd = os.open(sys.argv[1], os.O_WRONLY | os.O_NONBLOCK)
for chunk in (b"x" * 4096, b"x"):
    try:
        while True:
            os.write(fd, chunk)
    except BlockingIOError:
        pass
' "$WORK/stalled.fifo"
scenario L4 stalled
exec 7<&-

echo "PASS"
