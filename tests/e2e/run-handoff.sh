#!/bin/sh
set -eu
REPO=$(cd "$(dirname "$0")/../.." && pwd)
WORK=${1:-$(mktemp -d -p "${XDG_CACHE_HOME:-$HOME/.cache}" vigil-e2e-handoff.XXXXXX)}
case $WORK in /tmp/*) echo "workdir must not be under /tmp (guest tmpfs)"; exit 2;; esac
mkdir -p "$WORK/profiles"
cp "$REPO"/tests/fixtures/handoff/*.toml "$WORK/profiles/"
cat > "$WORK/vigil-handoff.toml" <<EOF
[profiles]
dir = "$WORK/profiles"
EOF
VIGIL_E2E_CONFIG=$WORK/vigil-handoff.toml "$REPO/tests/e2e/run.sh" "$WORK"
grep -q "^vigil: monitor profile desk$" "$WORK/vigil.log" || {
    echo "HANDOFF FAIL: greeter did not pick the session's profile: $(grep '^vigil: monitor profile' "$WORK/vigil.log" || echo none) ($WORK)"
    exit 1; }
echo "HANDOFF PASS ($WORK)"
