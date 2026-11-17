#!/bin/sh
set -eu
REPO=$(cd "$(dirname "$0")/../.." && pwd)

command -v sway >/dev/null 2>&1 || { echo "SKIP: sway not installed"; exit 0; }
command -v python3 >/dev/null 2>&1 || { echo "SKIP: python3 not installed"; exit 0; }
command -v flock >/dev/null 2>&1 || { echo "SKIP: flock (util-linux) not installed"; exit 0; }
command -v wtype >/dev/null 2>&1 || { echo "SKIP: wtype not installed (virtual keyboard — the headless seat has no real inputs)"; exit 0; }

TARGET=${CARGO_TARGET_DIR:-$REPO/target}
VLOCK=$TARGET/debug/vigil-lock
PROBE=$TARGET/debug/examples/lock_probe
[ -x "$VLOCK" ] && [ -x "$PROBE" ] || {
    echo "build first: cargo build -p vigil-lock && cargo build -p vigil-wayland --example lock_probe"
    exit 2
}

WORK=$(mktemp -d)
SWAY_PID=
cleanup() {
    [ -n "$SWAY_PID" ] && kill "$SWAY_PID" 2>/dev/null || true
    pkill -f "$VLOCK" 2>/dev/null || true
    command -v faillock >/dev/null 2>&1 &&
        faillock --user "$(id -un)" --reset >/dev/null 2>&1 || true
    if [ -n "${VIGIL_NESTED_KEEP:-}" ]; then
        echo "workdir kept: $WORK"
    else
        rm -rf "$WORK"
    fi
}
trap cleanup EXIT INT TERM

export XDG_RUNTIME_DIR=$WORK/run
mkdir -p "$XDG_RUNTIME_DIR"
chmod 700 "$XDG_RUNTIME_DIR"

export XDG_CONFIG_HOME=$WORK/config
mkdir -p "$XDG_CONFIG_HOME"
CFG=$WORK/vigil.toml
: >"$CFG"

export WLR_BACKENDS=headless WLR_LIBINPUT_NO_DEVICES=1 WLR_RENDERER=pixman

sway -c "$REPO/tests/nested/sway.cfg" >"$WORK/sway.log" 2>&1 &
SWAY_PID=$!
i=0
while :; do
    for sock in "$XDG_RUNTIME_DIR"/wayland-*; do
        [ -S "$sock" ] && break 2
    done
    i=$((i + 1))
    [ $i -gt 100 ] && { echo "FAIL: sway socket never appeared"; cat "$WORK/sway.log"; exit 1; }
    sleep 0.1
done
WAYLAND_DISPLAY=$(basename "$sock")
export WAYLAND_DISPLAY

sm() { swaymsg -s "$(echo "$XDG_RUNTIME_DIR"/sway-ipc.*.sock)" "$@"; }
LOCKFILE=$XDG_RUNTIME_DIR/vigil-lock-$WAYLAND_DISPLAY.lock
SOCK=$XDG_RUNTIME_DIR/vigil-lock-$WAYLAND_DISPLAY.sock

fail() { echo "FAIL: $*"; exit 1; }
pass() { echo "  ok: $*"; }

wait_free() {
    i=0
    while ! flock -n "$LOCKFILE" true 2>/dev/null; do
        i=$((i + 1))
        [ $i -gt 100 ] && fail "$1: locker never released the singleton (issue #49 regression)"
        sleep 0.1
    done
}
wait_held() {
    i=0
    while flock -n "$LOCKFILE" true 2>/dev/null; do
        i=$((i + 1))
        [ $i -gt 100 ] && fail "$1: locker never took the singleton"
        sleep 0.1
    done
}
probe_state() {
    "$PROBE" >/dev/null 2>&1 && echo 0 || echo $?
}
tap() { wtype -s 800 x; }
teardown_check() {
    wait_free "$1"
    sm -t get_version >/dev/null || fail "$1: sway died"
    [ "$(probe_state)" = 0 ] || fail "$1: session still locked after scenario"
}

echo "S1: readiness — locked event precedes --wait success"
[ "$(probe_state)" = 0 ] || fail "S1: fresh session reports locked"
WAYLAND_DEBUG=1 timeout 30 "$VLOCK" --config "$CFG" --wait --no-warn --grace 300 2>"$WORK/s1.trace" && rc=0 || rc=$?
[ "$rc" = 0 ] || { grep -v '^\[' "$WORK/s1.trace" | tail -5; fail "S1: --wait exited $rc"; }
s1_probe=$(probe_state)
[ "$s1_probe" = 10 ] || {
    echo "S1 diag: probe exit=$s1_probe (0=unlocked, 10=locked, 2=probe error)"
    echo "S1 diag: locker holds singleton: $(flock -n "$LOCKFILE" true 2>/dev/null && echo no || echo yes)"
    echo "S1 diag: non-protocol lines from the locker:"
    grep -v '^\[' "$WORK/s1.trace" | tail -20
    echo "S1 diag: last protocol lines:"
    grep '^\[' "$WORK/s1.trace" | tail -12
    echo "S1 diag: sway log tail:"
    tail -15 "$WORK/sway.log"
    fail "S1: probe says unlocked after --wait returned 0"
}
tap
teardown_check S1
python3 "$REPO/tests/nested/check_trace.py" "$WORK/s1.trace" --expect locked --handoff --locked-to-commit-ms 100 --no-reveal || fail "S1: trace check"
pass "S1"

echo "S2: second locker joins the in-flight warning"
timeout 60 env WAYLAND_DEBUG=1 "$VLOCK" --config "$CFG" --wait --warn 30 --grace 300 2>"$WORK/s2a.trace" &
A=$!
wait_held S2
start=$(date +%s)
timeout 25 "$VLOCK" --config "$CFG" --wait 2>"$WORK/s2b.log" && rcb=0 || rcb=$?
end=$(date +%s)
[ "$rcb" = 0 ] || { tail -3 "$WORK/s2b.log"; fail "S2: joiner exited $rcb"; }
[ $((end - start)) -lt 20 ] || fail "S2: join took $((end - start))s — commit request ignored"
wait $A && rca=0 || rca=$?
[ "$rca" = 0 ] || fail "S2: warning owner exited $rca"
[ "$(probe_state)" = 10 ] || fail "S2: session not locked after join"
python3 "$REPO/tests/nested/check_trace.py" "$WORK/s2a.trace" --expect locked --handoff || fail "S2: trace check"
tap
teardown_check S2
pass "S2"

echo "S3: cancel before commitment never acquires the lock"
timeout 60 env WAYLAND_DEBUG=1 "$VLOCK" --config "$CFG" --wait --warn 30 2>"$WORK/s3.trace" &
A=$!
wait_held S3
sleep 1
tap
wait $A && rc=0 || rc=$?
[ "$rc" = 3 ] || fail "S3: expected exit 3 (cancelled), got $rc"
python3 "$REPO/tests/nested/check_trace.py" "$WORK/s3.trace" --expect cancelled || fail "S3: trace check"
teardown_check S3
pass "S3"

echo "S4: two-output warning handoff never exposes the desktop"
sm create_output >/dev/null
timeout 60 env WAYLAND_DEBUG=1 VIGIL_FRAME_HASH=1 "$VLOCK" --config "$CFG" --wait --warn 2 --grace 300 2>"$WORK/s4.trace" && rc=0 || rc=$?
[ "$rc" = 0 ] || fail "S4: exited $rc"
python3 "$REPO/tests/nested/check_trace.py" "$WORK/s4.trace" --expect locked --handoff --first-frame-not-black --outputs 2 || fail "S4: trace check"
tap
wait_free S4
sm output HEADLESS-2 unplug >/dev/null 2>&1 || echo "  note: sway cannot unplug outputs; leaving HEADLESS-2"
teardown_check S4
pass "S4"

echo "S7a: hotplug during the warning cancels before commitment"
timeout 60 env WAYLAND_DEBUG=1 "$VLOCK" --config "$CFG" --wait --warn 30 2>"$WORK/s7a.trace" &
A=$!
wait_held S7a
sleep 1
sm create_output >/dev/null
wait $A && rc=0 || rc=$?
[ "$rc" = 3 ] || fail "S7a: expected exit 3 on topology change, got $rc"
python3 "$REPO/tests/nested/check_trace.py" "$WORK/s7a.trace" --expect cancelled || fail "S7a: trace check"
sm output HEADLESS-2 unplug >/dev/null 2>&1 || true
teardown_check S7a
pass "S7a"

echo "S7b: an output hotplugged while locked gets a lock surface"
timeout 30 env WAYLAND_DEBUG=1 "$VLOCK" --config "$CFG" --wait --no-warn --grace 300 2>"$WORK/s7b.trace" || fail "S7b: lock failed"
sm create_output >/dev/null
sleep 2
python3 "$REPO/tests/nested/check_trace.py" "$WORK/s7b.trace" --expect locked --outputs 2 || fail "S7b: trace check"
sm output HEADLESS-2 unplug >/dev/null 2>&1 || true
sleep 1
tap
teardown_check S7b
pass "S7b"

echo "S8: --immediate is the pre-0.4 path: no overlay on lock or unlock"
timeout 30 env WAYLAND_DEBUG=1 "$VLOCK" --config "$CFG" --wait --no-warn --immediate --grace 300 2>"$WORK/s8.trace" || fail "S8: lock failed"
[ "$(probe_state)" = 10 ] || fail "S8: session not locked"
tap
teardown_check S8
python3 "$REPO/tests/nested/check_trace.py" "$WORK/s8.trace" --expect locked --no-layer || fail "S8: trace check"
pass "S8"

echo "S9: hotplug during the transition commits instead of cancelling"
timeout 30 env WAYLAND_DEBUG=1 "$VLOCK" --config "$CFG" --wait --no-warn --grace 300 2>"$WORK/s9.trace" &
A=$!
sleep 0.15
sm create_output >/dev/null
wait $A && rc=0 || rc=$?
[ "$rc" = 0 ] || fail "S9: expected a lock (exit 0) despite hotplug, got $rc"
[ "$(probe_state)" = 10 ] || fail "S9: session not locked"
sleep 1
python3 "$REPO/tests/nested/check_trace.py" "$WORK/s9.trace" --expect locked --outputs 2 || fail "S9: trace check"
sm output HEADLESS-2 unplug >/dev/null 2>&1 || true
sleep 1
tap
teardown_check S9
pass "S9"

echo "S10: SIGTERM closes the trace and leaves the lock to the compositor (issue #79)"
timeout 30 env WAYLAND_DEBUG=1 SPAN_LINES=frames "$VLOCK" --config "$CFG" --no-warn --grace 300 2>"$WORK/s10.trace" &
A=$!
i=0
while ! grep -q 'ext_session_lock_v1.*\.locked' "$WORK/s10.trace" 2>/dev/null; do
    i=$((i + 1))
    [ $i -gt 100 ] && fail "S10: the locker never got locked"
    sleep 0.1
done
sleep 1
LOCKER=$(pgrep -P "$A") || fail "S10: no locker under timeout"
kill -TERM "$LOCKER"
wait $A && rc=0 || rc=$?
[ "$rc" = 143 ] || fail "S10: expected exit 143 (128 + SIGTERM), got $rc"
python3 "$REPO/tests/nested/check_trace.py" "$WORK/s10.trace" --expect locked --session-outcome Terminated:SIGTERM --no-unlock || fail "S10: trace check"
teardown_check S10
pass "S10"

echo "S5: no capture global was bound in any scenario"
for t in "$WORK"/s*.trace; do
    python3 "$REPO/tests/nested/check_trace.py" "$t" --no-capture-only || fail "S5: capture bind in $t"
done
pass "S5 (and S6: sway offers no background-effect global — tint tier exercised throughout)"

echo "ALL NESTED SCENARIOS PASSED"
