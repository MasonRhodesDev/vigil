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
ROW_HEIGHT = 800
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
