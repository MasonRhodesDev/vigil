#!/usr/bin/env python3
"""Host-side driver: QMP keyboard/tablet injection + screendumps against the
VM launched by run.sh, walking the full greeter-spec flow:

  username stage -> wrong password (error path) -> Escape (back to username)
  -> username again -> cycle the session picker by mouse -> correct password
  -> start_session

Success is the guest printing VIGIL-EXIT:0 with the picked session's cmd in
the fake-greetd log (asserted by run.sh)."""
import json, socket, sys, time

qmp_path, outdir = sys.argv[1], sys.argv[2]
# Total layout width in pixels: the usb-tablet's absolute axes span ALL
# outputs side by side, so click math needs the full row, not one screen.
ROW_WIDTH = int(sys.argv[3]) if len(sys.argv) > 3 else 1280
ROW_HEIGHT = int(sys.argv[4]) if len(sys.argv) > 4 else 800
s = socket.socket(socket.AF_UNIX)
s.connect(qmp_path)
f = s.makefile("rw")
json.loads(f.readline())

def cmd(c, **args):
    f.write(json.dumps({"execute": c, "arguments": args}) + "\n")
    f.flush()
    while True:
        r = json.loads(f.readline())
        if "return" in r or "error" in r:
            return r

def sendkey(k):
    return cmd("human-monitor-command", **{"command-line": f"sendkey {k}"})

def typestr(text):
    for k in text:
        sendkey(k)
        time.sleep(0.15)

def click(x, y, width=None, height=None):
    """Absolute click via the usb-tablet (QMP abs axes are 0..32767)."""
    width, height = width or ROW_WIDTH, height or ROW_HEIGHT
    ax, ay = int(x / width * 32767), int(y / height * 32767)
    events = [
        {"type": "abs", "data": {"axis": "x", "value": ax}},
        {"type": "abs", "data": {"axis": "y", "value": ay}},
    ]
    cmd("input-send-event", events=events)
    time.sleep(0.3)
    cmd("input-send-event", events=[{"type": "btn", "data": {"down": True, "button": "left"}}])
    time.sleep(0.15)
    cmd("input-send-event", events=[{"type": "btn", "data": {"down": False, "button": "left"}}])
    time.sleep(0.3)

cmd("qmp_capabilities")

def load_ppm(path):
    """Binary P6 only. Returns (width, height, rgb_bytes)."""
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

# The VM's console shows stale fbcon (a black frame, <=2 distinct colours)
# for ~10s after vigil starts presenting. Driving before the greeter is
# actually on screen would capture frames that say nothing about it — and a
# renderer that never paints anything must fail here, not "pass" blind.
FIRST_PAINT_TIMEOUT = 90
t0 = time.time()
while True:
    cmd("screendump", filename=f"{outdir}/0-first-paint.ppm")
    if distinct_colors(load_ppm(f"{outdir}/0-first-paint.ppm")) >= 8:
        break
    if time.time() - t0 > FIRST_PAINT_TIMEOUT:
        sys.exit(f"greeter never appeared on screen after {FIRST_PAINT_TIMEOUT}s")
    time.sleep(1)

# Username stage.
cmd("screendump", filename=f"{outdir}/1-username.ppm")
typestr("demo")
sendkey("ret"); time.sleep(1.5)
cmd("screendump", filename=f"{outdir}/2-password.ppm")

# VT round trip: the greeter owns Ctrl+Alt+Fn (libinput swallows the
# kernel's handling). Away to the VT2 text console, back via the kernel's
# text-mode switching, and the greeter must re-modeset and redraw.
sendkey("ctrl-alt-f2"); time.sleep(2.0)
cmd("screendump", filename=f"{outdir}/2b-vt2.ppm")
sendkey("ctrl-alt-f1"); time.sleep(2.0)
cmd("screendump", filename=f"{outdir}/2c-back.ppm")

# Wrong password: error surfaces, conversation auto-restarts.
typestr("wrong")
sendkey("ret"); time.sleep(2.0)
cmd("screendump", filename=f"{outdir}/3-error.ppm")

# Escape returns to the username stage.
sendkey("esc"); time.sleep(1.0)
cmd("screendump", filename=f"{outdir}/4-username-again.ppm")
typestr("demo")
sendkey("ret"); time.sleep(1.5)

# Cycle the session picker: "›" button center at 1280x800 with the default
# theme. The user cycler (issue #21) sits above the password field, so the
# session row moved down — verify with the theme_preview example if the
# card layout changes again.
click(797, 433)
cmd("screendump", filename=f"{outdir}/5-picker.ppm")

# Correct password.
typestr("hunter2")
time.sleep(0.4)
cmd("screendump", filename=f"{outdir}/6-typed.ppm")
sendkey("ret")
time.sleep(4)
print("driven; check guest log for VIGIL-EXIT:0")
