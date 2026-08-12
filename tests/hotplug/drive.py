#!/usr/bin/env python3
"""Host-side driver for the GPU unplug/replug test (issue #6).

Waits for the greeter to paint, PCI-unplugs the second virtio-gpu under
it, proves the greeter keeps rendering and authenticating, hot-adds a
third GPU (must be ignored politely), and completes a keyboard-only login.

The vigil log is on a --rwdir shared with the host, so guest-side evidence
(output removal, scan errors, panics) is asserted by reading it directly.
"""
import json, socket, sys, time

qmp_path, outdir = sys.argv[1], sys.argv[2]
ROW_WIDTH = int(sys.argv[3]) if len(sys.argv) > 3 else 1280
s = socket.socket(socket.AF_UNIX)
s.connect(qmp_path)
f = s.makefile("rw")
json.loads(f.readline())

events = []

def cmd(c, **args):
    f.write(json.dumps({"execute": c, "arguments": args}) + "\n")
    f.flush()
    while True:
        r = json.loads(f.readline())
        if "event" in r:
            events.append(r)
            continue
        if "return" in r or "error" in r:
            return r

def fail(msg):
    # Killing the VM through QMP is the only reliable teardown: run.sh's
    # `kill` reaches the vng launcher, not the reparented qemu, and an
    # orphaned guest holds every inherited pipe open forever.
    print(msg, file=sys.stderr, flush=True)
    try:
        cmd("quit")
    except Exception:
        pass
    sys.exit(1)

def sendkey(k):
    return cmd("human-monitor-command", **{"command-line": f"sendkey {k}"})

def typestr(text):
    for k in text:
        sendkey(k)
        time.sleep(0.15)

def load_ppm(path):
    with open(path, "rb") as fh:
        data = fh.read()
    magic, dims, _maxval, px = data.split(b"\n", 3)
    if magic != b"P6":
        sys.exit(f"{path}: not a binary P6 ppm")
    w, h = map(int, dims.split())
    return w, h, px

def distinct_colors(img, step=8):
    w, h, px = img
    seen = set()
    for y in range(0, h, step):
        row = y * w
        for x in range(0, w, step):
            i = (row + x) * 3
            seen.add(px[i:i + 3])
    return len(seen)

def dump_live(name):
    """Screendump that must show a live greeter frame, not a blank."""
    cmd("screendump", filename=f"{outdir}/{name}")
    n = distinct_colors(load_ppm(f"{outdir}/{name}"))
    if n < 8:
        fail(f"HOTPLUG FAIL: {name} is blank ({n} distinct colours)")
    return n

def log():
    try:
        with open(f"{outdir}/vigil.log") as fh:
            return fh.read()
    except OSError:
        return ""

def wait_in_log(needles, timeout, why):
    t0 = time.time()
    while time.time() - t0 < timeout:
        if any(n in log() for n in needles):
            return
        time.sleep(0.5)
    fail(f"HOTPLUG FAIL: {why} (none of {needles} within {timeout}s)")

cmd("qmp_capabilities")

# The VM's console shows stale fbcon for ~10s after vigil starts presenting
# (same wait as tests/e2e/drive.py).
t0 = time.time()
while True:
    cmd("screendump", filename=f"{outdir}/0-first-paint.ppm")
    if distinct_colors(load_ppm(f"{outdir}/0-first-paint.ppm")) >= 8:
        break
    if time.time() - t0 > 90:
        fail("greeter never appeared on screen after 90s")
    time.sleep(1)

assert log().count("vigil: output") >= 2, "both GPUs must be up before the unplug"
dump_live("1-baseline.ppm")

# Unplug the second GPU under the greeter. DEVICE_DELETED confirms QEMU
# finished the removal; the guest side is asserted through vigil's log.
r = cmd("device_del", id="gpu1")
if "error" in r:
    fail(f"HOTPLUG FAIL: device_del: {r}")
t0 = time.time()
while not any(e.get("event") == "DEVICE_DELETED" for e in events):
    if time.time() - t0 > 30:
        print("events seen:", [e.get("event") for e in events], file=sys.stderr)
        pci = cmd("query-pci")
        devs = [
            d.get("qdev_id")
            for bus in pci.get("return", [])
            for d in bus.get("devices", [])
        ]
        print("pci qdev ids:", devs, file=sys.stderr)
        fail("HOTPLUG FAIL: no DEVICE_DELETED within 30s")
    # QMP only delivers events while we are reading; poke it.
    cmd("query-status")
    time.sleep(0.5)
print("device_del: done", flush=True)

# The greeter must notice the loss and keep rendering on the survivor.
# Either the render loop notices first (present -> DeviceLost) or the
# udev rescan does (manager tombstone); both are correct.
wait_in_log(["lost; awaiting rescan", "vanished; dropping its outputs"], 30,
            "greeter never noticed the GPU vanish")
time.sleep(1)
dump_live("2-after-unplug.ppm")

# Auth must still work: username stage -> password stage.
typestr("demo")
sendkey("ret")
wait_in_log(["create_session"], 15, "username submit after unplug")
time.sleep(1)
dump_live("3-password-after-unplug.ppm")

# Hot-add a third GPU. vigil has no manager for a card born mid-session;
# the requirement is a polite ignore — no crash, no black screen.
r = cmd("device_add", driver="bochs-display", id="gpu2")
if "error" in r:
    fail(f"HOTPLUG FAIL: device_add: {r}")
time.sleep(5)
if "VIGIL-EXIT" in log():
    fail("HOTPLUG FAIL: greeter died on device_add")
dump_live("4-after-replug.ppm")

# Finish the login on the surviving output.
typestr("hunter2")
sendkey("ret")
time.sleep(4)
print("driven; check guest log for VIGIL-EXIT:0", flush=True)
