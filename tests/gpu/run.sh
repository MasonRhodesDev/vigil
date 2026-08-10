#!/bin/sh
# Run a command inside a GPU-capable virtme-ng guest and propagate its exit
# code. This is what makes the GL path (issue #17) testable at all.
#
#   tests/gpu/run.sh [--accel] -- <command...>
#   tests/gpu/run.sh -- target/debug/examples/gbm_probe /dev/dri/card0
#
# Why a VM: EGL rendering on a *card* node needs DRM master, and on a desktop
# the compositor holds it -- eglCreateWindowSurface fails with EGL_BAD_MATCH
# and amdgpu reports EACCES. In the guest nothing else owns the device.
#
# Two modes, both headless:
#
#   default   -device virtio-gpu-pci
#             Mesa falls back to kms_swrast/llvmpipe: real GL, real GBM, no
#             host GPU required. This is the mode for CI -- it works on a
#             stock runner with no graphics hardware at all.
#
#   --accel   -device virtio-gpu-gl-pci -display egl-headless
#             virglrenderer forwards to the host GPU (GL_RENDERER reports
#             virgl over the real card). For fidelity and performance checks
#             where llvmpipe's speed would mislead.
#
# Requires: qemu-system-x86-core, virtme-ng; --accel also needs
# virglrenderer on the host.
set -eu

ACCEL=0
while [ $# -gt 0 ]; do
    case $1 in
        --accel) ACCEL=1; shift ;;
        --) shift; break ;;
        *) echo "usage: $0 [--accel] -- <command...>" >&2; exit 2 ;;
    esac
done
[ $# -gt 0 ] || { echo "usage: $0 [--accel] -- <command...>" >&2; exit 2; }

REPO=$(cd "$(dirname "$0")/../.." && pwd)
# Not under /tmp: virtme overlays a fresh tmpfs there, so the guest could
# never write its log back to a path the host can read.
WORK=$(mktemp -d -p "${XDG_CACHE_HOME:-$HOME/.cache}" vigil-gpu.XXXXXX)
LOG=$WORK/gpu.log
: > "$LOG"

if [ "$ACCEL" -eq 1 ]; then
    NODE=${VIGIL_GPU_RENDERNODE:-/dev/dri/renderD128}
    GPU="-device virtio-gpu-gl-pci -display egl-headless,rendernode=$NODE"
else
    GPU="-device virtio-gpu-pci"
fi

# The command runs as root in the guest, with the repo visible from the host
# rootfs and $WORK writable.
cat > "$WORK/guest.sh" <<EOF
#!/bin/sh
cd "$REPO" || exit 1
for i in \$(seq 50); do [ -e /dev/dri/card0 ] && break; sleep 0.2; done
$* >> "$LOG" 2>&1
echo "GPU-EXIT:\$?" >> "$LOG"
EOF
chmod +x "$WORK/guest.sh"

timeout "${VIGIL_GPU_TIMEOUT:-300}" vng --run --disable-microvm --user root \
    -m 1G --cpus 2 --rwdir="$WORK" --qemu-opts="$GPU" \
    -e "$WORK/guest.sh" >/dev/null 2>&1 || true

cat "$LOG"
code=$(sed -n 's/^GPU-EXIT:\([0-9]*\)$/\1/p' "$LOG" | tail -1)
if [ -z "$code" ]; then
    echo "GPU HARNESS FAIL: guest never reported an exit code ($WORK)" >&2
    exit 1
fi
rm -rf "$WORK"
exit "$code"
