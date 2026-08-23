#!/bin/sh
# Nested-compositor integration suite for lock readiness (issue #44).
#
# Runs vigil-lock against a headless sway (ext-session-lock-v1, layer-shell,
# runtime output hotplug, input injection — and deliberately NO
# ext-background-effect, so this is the tint-fallback tier) inside a private
# XDG_RUNTIME_DIR. Every vigil-lock run is traced with WAYLAND_DEBUG=1 and
# check_trace.py asserts protocol ordering: locked precedes readiness, no
# capture global is ever bound, the warning-to-lock handoff never exposes
# the desktop, and a cancel never requests the lock.
#
# Opt-in locally (skips cleanly without sway); gates release in CI.
# Every scenario runs under timeout: a hang IS the failure (issue #49).
set -eu
REPO=$(cd "$(dirname "$0")/../.." && pwd)

command -v sway >/dev/null 2>&1 || { echo "SKIP: sway not installed"; exit 0; }
command -v python3 >/dev/null 2>&1 || { echo "SKIP: python3 not installed"; exit 0; }
command -v flock >/dev/null 2>&1 || { echo "SKIP: flock (util-linux) not installed"; exit 0; }
command -v wtype >/dev/null 2>&1 || { echo "SKIP: wtype not installed (virtual keyboard — the headless seat has no real inputs)"; exit 0; }

VLOCK=$REPO/target/debug/vigil-lock
PROBE=$REPO/target/debug/examples/lock_probe
[ -x "$VLOCK" ] && [ -x "$PROBE" ] || {
    echo "build first: cargo build -p vigil-lock && cargo build -p vigil-wayland --example lock_probe"
    exit 2
}

WORK=$(mktemp -d)
SWAY_PID=
cleanup() {
    [ -n "$SWAY_PID" ] && kill "$SWAY_PID" 2>/dev/null || true
    # Reap any harness locker that survived a failed scenario. Matching the
    # target/debug path never touches a system /usr/bin/vigil-lock.
    pkill -f "$VLOCK" 2>/dev/null || true
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
export WLR_BACKENDS=headless WLR_LIBINPUT_NO_DEVICES=1 WLR_RENDERER=pixman

sway -c "$REPO/tests/nested/sway.cfg" >"$WORK/sway.log" 2>&1 &
SWAY_PID=$!
# sway names its own socket inside our private runtime dir; adopt it.
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

# Wait until no live locker owns the singleton (kernel releases the flock on
# death, so this is exact — leftover files do not matter).
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
# errexit-safe: the probe exits 10 on purpose; || keeps set -e from aborting.
probe_state() {
    "$PROBE" >/dev/null 2>&1 && echo 0 || echo $?
}
# The headless seat reports capabilities(0): swaymsg cursor injection goes
# nowhere. wtype attaches a zwp_virtual_keyboard, which gives the seat a
# keyboard and delivers a real key press to the focused (lock or warning)
# surface — a key press dismisses the grace window and cancels a pending
# warning ("activity before commitment").
# -s keeps the transient virtual keyboard alive long enough for the seat
# capability, keymap, and focus enter to reach the client before the key.
tap() { wtype -s 800 x; }
# Teardown invariants after every scenario (issue #44 checklist).
teardown_check() {
    wait_free "$1"
    sm -t get_version >/dev/null || fail "$1: sway died"
    [ "$(probe_state)" = 0 ] || fail "$1: session still locked after scenario"
}

echo "S1: readiness — locked event precedes --wait success"
[ "$(probe_state)" = 0 ] || fail "S1: fresh session reports locked"
WAYLAND_DEBUG=1 timeout 30 "$VLOCK" --wait --no-warn --grace 300 2>"$WORK/s1.trace" && rc=0 || rc=$?
[ "$rc" = 0 ] || { grep -v '^\[' "$WORK/s1.trace" | tail -5; fail "S1: --wait exited $rc"; }
s1_probe=$(probe_state)
[ "$s1_probe" = 10 ] || {
    # Diagnostics: is the locker still alive, did it panic, what did sway say.
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
python3 "$REPO/tests/nested/check_trace.py" "$WORK/s1.trace" --expect locked || fail "S1: trace check"
tap
teardown_check S1
pass "S1"

echo "S2: second locker joins the in-flight warning"
timeout 60 env WAYLAND_DEBUG=1 "$VLOCK" --wait --warn 30 --grace 300 2>"$WORK/s2a.trace" &
A=$!
wait_held S2
start=$(date +%s)
timeout 25 "$VLOCK" --wait 2>"$WORK/s2b.log" && rcb=0 || rcb=$?
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
timeout 60 env WAYLAND_DEBUG=1 "$VLOCK" --wait --warn 30 2>"$WORK/s3.trace" &
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
timeout 60 env WAYLAND_DEBUG=1 "$VLOCK" --wait --warn 2 --grace 300 2>"$WORK/s4.trace" && rc=0 || rc=$?
[ "$rc" = 0 ] || fail "S4: exited $rc"
python3 "$REPO/tests/nested/check_trace.py" "$WORK/s4.trace" --expect locked --handoff --outputs 2 || fail "S4: trace check"
tap
wait_free S4
sm output HEADLESS-2 unplug >/dev/null 2>&1 || echo "  note: sway cannot unplug outputs; leaving HEADLESS-2"
teardown_check S4
pass "S4"

echo "S7a: hotplug during the warning cancels before commitment"
timeout 60 env WAYLAND_DEBUG=1 "$VLOCK" --wait --warn 30 2>"$WORK/s7a.trace" &
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
timeout 30 env WAYLAND_DEBUG=1 "$VLOCK" --wait --no-warn --grace 300 2>"$WORK/s7b.trace" || fail "S7b: lock failed"
sm create_output >/dev/null
sleep 2
# The detached child appends to the same trace fd; assert the new output's
# lock surface was created and committed after the hotplug.
python3 "$REPO/tests/nested/check_trace.py" "$WORK/s7b.trace" --expect locked --outputs 2 || fail "S7b: trace check"
sm output HEADLESS-2 unplug >/dev/null 2>&1 || true
sleep 1
tap
teardown_check S7b
pass "S7b"

echo "S5: no capture global was bound in any scenario"
for t in "$WORK"/s*.trace; do
    python3 "$REPO/tests/nested/check_trace.py" "$t" --no-capture-only || fail "S5: capture bind in $t"
done
pass "S5 (and S6: sway offers no background-effect global — tint tier exercised throughout)"

echo "ALL NESTED SCENARIOS PASSED"
