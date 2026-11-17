#!/bin/sh
set -eu
REPO=$(cd "$(dirname "$0")/../.." && pwd)

command -v sway >/dev/null 2>&1 || { echo "SKIP: sway not installed"; exit 0; }
command -v wtype >/dev/null 2>&1 || { echo "SKIP: wtype not installed"; exit 0; }
command -v systemd-run >/dev/null 2>&1 || { echo "SKIP: systemd-run not installed"; exit 0; }
command -v python3 >/dev/null 2>&1 || { echo "SKIP: python3 not installed"; exit 0; }
systemctl --user show-environment >/dev/null 2>&1 || {
    echo "SKIP: no systemd user manager (needs a logged-in session, not a container)"
    exit 0
}
VLOCK=${VLOCK:-$REPO/target/debug/vigil-lock}
[ -x "$VLOCK" ] || { echo "build first: cargo build -p vigil-lock"; exit 2; }

HOST_RUNTIME=$XDG_RUNTIME_DIR
UNIT="vigil-journald-$$"
WORK=$(mktemp -d)
SWAY_PID=
cleanup() {
    [ -n "$SWAY_PID" ] && kill "$SWAY_PID" 2>/dev/null || true
    XDG_RUNTIME_DIR=$HOST_RUNTIME systemctl --user stop "$UNIT.service" 2>/dev/null || true
    pkill -f "$VLOCK" 2>/dev/null || true
    [ -n "${VIGIL_JOURNALD_KEEP:-}" ] && echo "workdir kept: $WORK" || rm -rf "$WORK"
}
trap cleanup EXIT INT TERM

export XDG_RUNTIME_DIR=$WORK/run
mkdir -p "$XDG_RUNTIME_DIR"
chmod 700 "$XDG_RUNTIME_DIR"
export WLR_BACKENDS=headless WLR_LIBINPUT_NO_DEVICES=1 WLR_RENDERER=pixman
sway -c "$REPO/tests/nested/sway.cfg" >"$WORK/sway.log" 2>&1 &
SWAY_PID=$!
i=0
while :; do
    for sock in "$XDG_RUNTIME_DIR"/wayland-*; do [ -S "$sock" ] && break 2; done
    i=$((i + 1))
    [ $i -gt 100 ] && { echo "FAIL: sway socket never appeared"; cat "$WORK/sway.log"; exit 1; }
    sleep 0.1
done
NESTED_WD=$(basename "$sock")
NESTED_RUNTIME=$XDG_RUNTIME_DIR

hex() { head -c "$1" /dev/urandom | od -An -tx1 | tr -d ' \n'; }
TP="00-$(hex 16)-$(hex 8)-01"
TRACE_ID=$(printf '%s' "$TP" | cut -d- -f2)
echo "minted TRACEPARENT=$TP"

XDG_RUNTIME_DIR=$HOST_RUNTIME systemd-run --user --collect --quiet --unit="$UNIT" \
    -- /bin/sh -c "exec env XDG_RUNTIME_DIR='$HOST_RUNTIME' systemd-run --user --scope --quiet \
        --setenv=XDG_RUNTIME_DIR='$NESTED_RUNTIME' \
        --setenv=WAYLAND_DISPLAY='$NESTED_WD' \
        --setenv=SPAN_LINES='${SPAN_LINES:-frames}' \
        --setenv=TRACEPARENT='$TP' \
        -- '$VLOCK' --warn 4000" >/dev/null 2>&1

sleep 1.5
WAYLAND_DISPLAY=$NESTED_WD wtype -s 800 x 2>/dev/null || true
sleep 3

XDG_RUNTIME_DIR=$HOST_RUNTIME journalctl --user --since "-2 min" -o json \
    _COMM=vigil-lock > "$WORK/journal.json" 2>/dev/null || true

python3 "$REPO/tests/journald/check_records.py" "$WORK/journal.json" "$TRACE_ID"
