#!/bin/sh
# GL with a rotated output in the VM (issue #26). virtio-gpu has no plane
# rotation property, so the rotated output must FALL BACK to software (with
# the rotation test as the logged reason) while the untouched output stays
# on GL — and the full login must still pass. This proves the fallback gate;
# proving the rotation itself renders upright needs metal (the dock).
set -eu
REPO=$(cd "$(dirname "$0")/../.." && pwd)
WORK=${1:-$(mktemp -d -p "${XDG_CACHE_HOME:-$HOME/.cache}" vigil-e2e-rot.XXXXXX)}
case $WORK in /tmp/*) echo "workdir must not be under /tmp (guest tmpfs)"; exit 2;; esac
mkdir -p "$WORK/profiles"
# Both VM outputs describe as "RHT QEMU Monitor"; connectors pin the entries.
cat > "$WORK/profiles/rotated.toml" <<EOF
description="e2e rotated secondary"
match=["RHT QEMU Monitor"]
[[monitor]]
output="Virtual-1"
position=[0,0]
[[monitor]]
output="Virtual-2"
position=[1280,0]
transform=3
EOF
cat > "$WORK/vigil-rotated.toml" <<EOF
[render]
backend = "gl"
[profiles]
dir = "$WORK/profiles"
EOF
# The rotated Virtual-2 has scene dims 800x1280, so the pointer row's
# bounding box is 2080x1280 — the driver's click math must use it.
VIGIL_E2E_CONFIG=$WORK/vigil-rotated.toml VIGIL_E2E_ROW_W=2080 VIGIL_E2E_ROW_H=1280 \
    "$REPO/tests/e2e/run.sh" "$WORK"
grep -q "rendering with GL (hardware cursor)" "$WORK/vigil.log" || {
    echo "ROTATED FAIL: no output on GL ($WORK)"; exit 1; }
grep -q "GL unavailable (rotation test:" "$WORK/vigil.log" || {
    echo "ROTATED FAIL: rotated output did not fall back via the rotation test ($WORK)"; exit 1; }
echo "ROTATED PASS ($WORK)"
