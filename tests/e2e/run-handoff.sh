#!/bin/sh
# The session's layout reaches the greeter. tests/fixtures/handoff mirrors
# what /etc/monitor-profiles held in the hypr-qa guest after dials saved a
# rotation change there, laid out for this VM's two outputs (connectors pin
# the entries, as in run-rotated.sh):
#
#   desk.toml       the session's profile: matched by the name Hyprland gives
#                   QEMU's monitor ("Red Hat Inc. QEMU Monitor", read from
#                   hyprctl in the guest), second output back to landscape
#   desk-twin.toml  a stale hand-made copy in the old vigil spelling
#                   ("RHT QEMU Monitor"), second output still rotated: the
#                   shape of mason-work's dell-...-hp-e243.toml
#
# The greeter must pick desk and draw both outputs upright, so the full
# login flow passes with the unrotated 2560x800 pointer row. A greeter that
# names the monitor its own way matches only the twin and rotates the
# second output, which is the bug Mason photographed.
set -eu
REPO=$(cd "$(dirname "$0")/../.." && pwd)
WORK=${1:-$(mktemp -d -p "${XDG_CACHE_HOME:-$HOME/.cache}" vigil-e2e-handoff.XXXXXX)}
case $WORK in /tmp/*) echo "workdir must not be under /tmp (guest tmpfs)"; exit 2;; esac
mkdir -p "$WORK/profiles"
cp "$REPO"/tests/fixtures/handoff/*.toml "$WORK/profiles/"
# Software rendering: the GL path loses the card after the VT round trip
# (#108), which would fail the login flow for an unrelated reason.
cat > "$WORK/vigil-handoff.toml" <<EOF
[profiles]
dir = "$WORK/profiles"
EOF
VIGIL_E2E_CONFIG=$WORK/vigil-handoff.toml "$REPO/tests/e2e/run.sh" "$WORK"
grep -q "^vigil: monitor profile desk$" "$WORK/vigil.log" || {
    echo "HANDOFF FAIL: greeter did not pick the session's profile: $(grep '^vigil: monitor profile' "$WORK/vigil.log" || echo none) ($WORK)"
    exit 1; }
echo "HANDOFF PASS ($WORK)"
