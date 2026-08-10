#!/bin/sh
# Run a command inside a GPU-capable virtme-ng guest and propagate its exit
# code. This is what makes the GL path (issue #17) testable at all.
#
#   tests/gpu/run.sh [--accel] [--screenshot FILE.ppm] -- <command...>
#   tests/gpu/run.sh -- target/debug/examples/gbm_probe /dev/dri/card0
#   tests/gpu/run.sh --screenshot /tmp/gl.ppm -- target/debug/examples/gl_modeset
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
#             (no --screenshot: see below)
#             virglrenderer forwards to the host GPU (GL_RENDERER reports
#             virgl over the real card). For fidelity and performance checks
#             where llvmpipe's speed would mislead.
#
# Requires: qemu-system-x86-core, virtme-ng; --accel also needs
# virglrenderer on the host.
set -eu

ACCEL=0
SHOT=
while [ $# -gt 0 ]; do
    case $1 in
        --accel) ACCEL=1; shift ;;
        --screenshot) SHOT=$2; shift 2 ;;
        --) shift; break ;;
        *) echo "usage: $0 [--accel] [--screenshot FILE] -- <command...>" >&2; exit 2 ;;
    esac
done
[ $# -gt 0 ] || { echo "usage: $0 [--accel] [--screenshot FILE] -- <command...>" >&2; exit 2; }

REPO=$(cd "$(dirname "$0")/../.." && pwd)
# Not under /tmp: virtme overlays a fresh tmpfs there, so the guest could
# never write its log back to a path the host can read.
WORK=$(mktemp -d -p "${XDG_CACHE_HOME:-$HOME/.cache}" vigil-gpu.XXXXXX)
LOG=$WORK/gpu.log
: > "$LOG"

# --screenshot needs QMP to ask QEMU for the guest's display. Capturing what
# the virtual monitor actually shows is the only way to tell a frame that
# reached the screen from a commit that merely returned success.
#
# Software mode only. egl-headless keeps nothing in QEMU's Pixman buffer by
# design, so screendump there answers "no surface" -- the frame is on the
# guest's CRTC, there is just no host-side copy to hand back.
QMP=$WORK/qmp.sock
QMPOPT=
[ -n "$SHOT" ] && QMPOPT="-qmp unix:$QMP,server=on,wait=off"

if [ "$ACCEL" -eq 1 ]; then
    NODE=${VIGIL_GPU_RENDERNODE:-/dev/dri/renderD128}
    GPU="-device virtio-gpu-gl-pci -display egl-headless,rendernode=$NODE $QMPOPT"
else
    GPU="-device virtio-gpu-pci $QMPOPT"
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
    -e "$WORK/guest.sh" >/dev/null 2>&1 &
VM=$!

if [ -n "$SHOT" ]; then
    # Fire once the command has done its work but before the guest exits;
    # the modeset examples hold the mode briefly for exactly this.
    for _ in $(seq 120); do
        grep -q "MODESET OK" "$LOG" 2>/dev/null && break
        sleep 1
    done
    python3 - "$QMP" "$SHOT" <<'PY' || echo "screenshot failed" >&2
import json, socket, sys, time
qmp, out = sys.argv[1], sys.argv[2]
for _ in range(30):
    try:
        s = socket.socket(socket.AF_UNIX); s.connect(qmp); break
    except OSError:
        time.sleep(0.5)
else:
    sys.exit(1)
f = s.makefile("rw")
json.loads(f.readline())
def cmd(c, **args):
    f.write(json.dumps({"execute": c, "arguments": args}) + "\n"); f.flush()
    while True:
        r = json.loads(f.readline())
        if "return" in r or "error" in r:
            return r
cmd("qmp_capabilities")
print(cmd("screendump", filename=out))
PY
fi

wait $VM 2>/dev/null || true
cat "$LOG"
code=$(sed -n 's/^GPU-EXIT:\([0-9]*\)$/\1/p' "$LOG" | tail -1)
if [ -z "$code" ]; then
    echo "GPU HARNESS FAIL: guest never reported an exit code ($WORK)" >&2
    exit 1
fi
rm -rf "$WORK"
exit "$code"
