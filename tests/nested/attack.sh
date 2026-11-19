#!/bin/sh
# Attacks on a locked session under headless sway. Each scenario ends with
# the session still locked, observed from outside the lock client.
set -eu
REPO=$(cd "$(dirname "$0")/../.." && pwd)

skip() {
    echo "SKIP: $*"
    [ -z "${CI:-}" ] || exit 1
    exit 0
}
for tool in sway swaymsg flock wtype prlimit pgrep; do
    command -v "$tool" >/dev/null 2>&1 || skip "$tool is not installed"
done

# cargo knows where its builds went, whatever wrapper or variable moved them.
TARGET=$(cd "$REPO" && cargo metadata --format-version 1 --no-deps 2>/dev/null |
    sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p') || true
TARGET=${TARGET:-${CARGO_TARGET_DIR:-$REPO/target}}
VLOCK=$TARGET/debug/vigil-lock
PROBE=$TARGET/debug/examples/lock_probe
[ -x "$VLOCK" ] && [ -x "$PROBE" ] || {
    echo "build first: cargo build -p vigil-lock && cargo build -p vigil-wayland --example lock_probe"
    exit 2
}

# No account by this name exists, so no keystroke here counts against one.
GHOST=vigil-attack-ghost
WORK=$(mktemp -d)
# `--wait` re-runs the locker under setsid, so neither its process group nor
# its path tells this run's lockers from one the user runs from the same
# build. The environment they inherit does.
export VIGIL_ATTACK_RUN="$WORK"
ours() {
    for pid in $(pgrep -x vigil-lock); do
        tr '\0' '\n' <"/proc/$pid/environ" 2>/dev/null |
            grep -qxF "VIGIL_ATTACK_RUN=$WORK" && echo "$pid"
    done
    return 0
}
SWAY_PID=
cleanup() {
    for pid in $(ours); do kill -KILL "$pid" 2>/dev/null || true; done
    [ -n "$SWAY_PID" ] && kill "$SWAY_PID" 2>/dev/null || true
    if [ -n "${VIGIL_NESTED_KEEP:-}" ]; then
        echo "workdir kept: $WORK"
    else
        rm -rf "$WORK"
    fi
}
trap cleanup EXIT INT TERM

# A crashed lock client must not leave a core with typed text on disk.
ulimit -c 0

# The lock client and the host's PAM modules talk D-Bus. Neither may reach
# the host's D-Bus, so both addresses name a socket nobody listens on. The
# run is not sealed off the host otherwise: NSS looks the ghost user up
# through /run/systemd/userdb, and PAM modules write to /dev/log.
export DBUS_SYSTEM_BUS_ADDRESS=unix:path=$WORK/no-system-bus
export DBUS_SESSION_BUS_ADDRESS=unix:path=$WORK/no-session-bus
export XDG_RUNTIME_DIR=$WORK/run
mkdir -p "$XDG_RUNTIME_DIR" "$WORK/canary"
chmod 700 "$XDG_RUNTIME_DIR"
export XDG_CONFIG_HOME=$WORK/config XDG_STATE_HOME=$WORK/state XDG_DATA_HOME=$WORK/data
export XDG_CACHE_HOME=$WORK/cache
mkdir -p "$XDG_CONFIG_HOME"
unset WALLPAPER_PATH
CFG=$WORK/vigil.toml
: >"$CFG"

# sway runs a plain binding only while the session is unlocked and a
# --locked binding always. F9 is the canary, F10 proves the key arrived.
cat >"$WORK/sway.cfg" <<EOF
xwayland disable
default_border none
output HEADLESS-1 resolution 1280x800
bindsym F9 exec touch $WORK/canary/desktop
bindsym --locked F10 exec touch $WORK/canary/arrived
EOF

export WLR_BACKENDS=headless WLR_LIBINPUT_NO_DEVICES=1 WLR_RENDERER=pixman
sway -c "$WORK/sway.cfg" >"$WORK/sway.log" 2>&1 &
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
LOCKFILE=$XDG_RUNTIME_DIR/vigil-lock-$WAYLAND_DISPLAY.lock

sm() { swaymsg -s "$(echo "$XDG_RUNTIME_DIR"/sway-ipc.*.sock)" "$@"; }
fail() {
    echo "FAIL: $*"
    echo "--- lock client log"
    tail -n 15 "$WORK/lock.log" 2>/dev/null || true
    exit 1
}
pass() { echo "  ok: $*"; }

# 0 when the compositor treats the session as locked.
desktop_is_hidden() {
    rm -f "$WORK/canary/desktop" "$WORK/canary/arrived"
    wtype -k F9
    wtype -k F10
    i=0
    while [ ! -e "$WORK/canary/arrived" ]; do
        i=$((i + 1))
        [ $i -gt 50 ] && fail "$1: the canary key never reached sway"
        sleep 0.1
    done
    sleep 0.3
    [ ! -e "$WORK/canary/desktop" ]
}
still_locked() {
    desktop_is_hidden "$1" || fail "$1: the desktop took a key, the session is unlocked"
    sleep 1
    desktop_is_hidden "$1" || fail "$1: the session unlocked a second later"
}
locker() { ours | head -n 1; }
gone() {
    i=0
    while kill -0 "$1" 2>/dev/null; do
        i=$((i + 1))
        [ $i -gt "${2:-100}" ] && return 1
        sleep 0.1
    done
}
lock() {
    timeout 40 "$VLOCK" --config "$CFG" --user "$GHOST" --wait --no-warn "$@" 2>>"$WORK/lock.log"
}
held_by_a_live_client() {
    "$PROBE" >/dev/null 2>&1 && rc=0 || rc=$?
    [ "$rc" = 10 ]
}
release() {
    pid=$(locker) || true
    if [ -n "${pid:-}" ]; then
        kill -KILL "$pid" 2>/dev/null || true
        gone "$pid" || fail "$1: the lock client survived SIGKILL"
    fi
    i=0
    while ! flock -n "$LOCKFILE" true 2>/dev/null; do
        i=$((i + 1))
        [ $i -gt 100 ] && fail "$1: the singleton was never released"
        sleep 0.1
    done
    # The probe takes an abandoned lock over and releases it.
    "$PROBE" >/dev/null 2>&1 || fail "$1: the probe could not release the session"
    desktop_is_hidden "$1" && fail "$1: the session is still locked after release"
    return 0
}
dies_locked() {
    lock --grace 0 || fail "$1: lock failed"
    pid=$(locker)
    kill "-$2" "$pid"
    sleep 0.5
    # std answers a SIGSEGV that is no guard page fault by restoring the
    # default action and returning, so a sent one is fatal the second time.
    kill "-$2" "$pid" 2>/dev/null || true
    gone "$pid" || fail "$1: the lock client survived SIG$2"
    still_locked "$1"
    lock --grace 0 || fail "$1: a new lock client could not take the abandoned lock"
    held_by_a_live_client || fail "$1: no live client holds the lock after the takeover"
    still_locked "$1"
    release "$1"
    pass "$1"
}

echo "A0: the canary tells an unlocked session from a locked one"
desktop_is_hidden A0 && fail "A0: the canary reports a fresh session as locked"
lock --grace 0 || fail "A0: lock failed"
still_locked A0
release A0
pass "A0"

echo "A1: SIGKILL on the lock client (what the kernel OOM killer sends)"
dies_locked A1 KILL

echo "A2: SIGSEGV on the lock client"
dies_locked A2 SEGV

echo "A3: SIGABRT on the lock client"
dies_locked A3 ABRT

echo "A4: allocations fail inside the lock client"
lock --grace 0 || fail "A4: lock failed"
pid=$(locker)
size=$(awk '/^VmSize:/ { print $2 * 1024 }' "/proc/$pid/status")
prlimit --pid "$pid" --as="$size" || fail "A4: could not cap the address space"
sm create_output >/dev/null
sleep 2
still_locked A4
if kill -0 "$pid" 2>/dev/null; then
    echo "  note: the client survived a new output with no memory to grow into"
else
    echo "  note: the client died when its allocation failed"
fi
sm output HEADLESS-2 unplug >/dev/null 2>&1 || true
release A4
pass "A4"

echo "A5: SIGSTOP then SIGCONT on the lock client"
lock --grace 0 || fail "A5: lock failed"
pid=$(locker)
kill -STOP "$pid"
still_locked "A5 (stopped)"
kill -CONT "$pid"
sleep 0.5
kill -0 "$pid" 2>/dev/null || fail "A5: the lock client died on SIGCONT"
held_by_a_live_client || fail "A5: the client lost the lock while stopped"
still_locked "A5 (continued)"
release A5
pass "A5"

echo "A6: the compositor reloads its configuration"
lock --grace 0 || fail "A6: lock failed"
pid=$(locker)
sm reload >/dev/null
sleep 1
kill -0 "$pid" 2>/dev/null || fail "A6: the lock client died on reload"
held_by_a_live_client || fail "A6: the client lost the lock on reload"
still_locked A6
release A6
pass "A6"

echo "A7: an input flood while a sign-in is in flight"
lock --grace 0 || fail "A7: lock failed"
pid=$(locker)
before=$(awk '/^VmRSS:/ { print $2 }' "/proc/$pid/status")
burst=$(head -c 300 /dev/zero | tr '\0' 'k')
round=0
while [ $round -lt 4 ]; do
    wtype "$burst" -k Return -k Escape -k BackSpace -k Tab -k Left -k Home -k Delete
    wtype -M ctrl -k a -m ctrl -M ctrl -k v -m ctrl -k Return
    round=$((round + 1))
done
kill -0 "$pid" 2>/dev/null || fail "A7: the lock client died under the flood"
after=$(awk '/^VmRSS:/ { print $2 }' "/proc/$pid/status")
[ $((after - before)) -lt 65536 ] || fail "A7: the flood grew the client by $((after - before)) kB"
held_by_a_live_client || fail "A7: the client lost the lock under the flood"
still_locked A7
# The client draws once per key, so SIGTERM is answered only after the backlog.
sent=$(date +%s)
kill -TERM "$pid"
gone "$pid" 900 || fail "A7: the client never answered SIGTERM, its loop is stuck"
still_locked "A7 (after SIGTERM)"
release A7
pass "A7 (grew $((after - before)) kB, answered SIGTERM after $(($(date +%s) - sent))s)"

echo "A8: an unlock racing a new lock"
# A reveal keeps the unlocking client alive long enough to race it.
printf '[lock.transition]\nwallpaper_out_ms = 1500\n' >"$WORK/reveal.toml"
for after in 0 0.1 0.3 0.6 1.0 1.4 1.8; do
    CFG=$WORK/reveal.toml lock --grace 300 || fail "A8: lock failed"
    first=$(locker)
    unlocks=$(grep -ac 'from=Locked to=RevealPending' "$WORK/lock.log" || true)
    # wtype's keymap needs a moment to reach the client before the key counts.
    wtype -s 800 x
    i=0
    while [ "$(grep -ac 'from=Locked to=RevealPending' "$WORK/lock.log" || true)" = "$unlocks" ]; do
        i=$((i + 1))
        [ $i -gt 100 ] && fail "A8: the key never dismissed the grace window"
        sleep 0.02
    done
    sleep "$after"
    lock --grace 0 && second=0 || second=$?
    gone "$first" || fail "A8 (${after}s): the unlocking client never exited"
    sleep 0.5
    [ "$second" = 0 ] || fail "A8 (${after}s): the second lock exited $second"
    held_by_a_live_client ||
        fail "A8 (${after}s): the second lock reported success and the session is unlocked"
    still_locked "A8 (${after}s)"
    echo "  ${after}s after the key: locked again"
    release "A8 (${after}s)"
done
pass "A8"

echo "A9: the compositor dies"
lock --grace 0 || fail "A9: lock failed"
pid=$(locker)
kill -KILL "$SWAY_PID"
wait "$SWAY_PID" 2>/dev/null || true
SWAY_PID=
gone "$pid" || fail "A9: the lock client outlived its compositor"
flock -n "$LOCKFILE" true 2>/dev/null || fail "A9: the singleton is still held"
pass "A9"

echo "SKIP: VT switch. Headless sway has no seat session; see README, Security tests."
echo "ALL LOCK ATTACKS HELD"
