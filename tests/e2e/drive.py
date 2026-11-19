#!/usr/bin/env python3
import json, socket, sys, time

qmp_path, outdir = sys.argv[1], sys.argv[2]
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

cmd("qmp_capabilities")

def conversations():
    with open(f"{outdir}/vigil.log") as fh:
        return fh.read().count("fake-greetd: <- create_session")

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

FIRST_PAINT_TIMEOUT = 90
t0 = time.time()
while True:
    cmd("screendump", filename=f"{outdir}/0-first-paint.ppm")
    if distinct_colors(load_ppm(f"{outdir}/0-first-paint.ppm")) >= 8:
        break
    if time.time() - t0 > FIRST_PAINT_TIMEOUT:
        sys.exit(f"greeter never appeared on screen after {FIRST_PAINT_TIMEOUT}s")
    time.sleep(1)

cmd("screendump", filename=f"{outdir}/0b-second-output.ppm", device="gpu1")

cmd("screendump", filename=f"{outdir}/1-username.ppm")
typestr("demo")
time.sleep(1.0)
if conversations() != 0:
    sys.exit("greetd was sent a create_session before the first Enter")
sendkey("ret"); time.sleep(1.5)
cmd("screendump", filename=f"{outdir}/2-password.ppm")

sendkey("ctrl-alt-f2"); time.sleep(2.0)
cmd("screendump", filename=f"{outdir}/2b-vt2.ppm")
sendkey("ctrl-alt-f1"); time.sleep(2.0)
cmd("screendump", filename=f"{outdir}/2c-back.ppm")

typestr("wrong")
sendkey("ret"); time.sleep(2.0)
cmd("screendump", filename=f"{outdir}/3-error.ppm")
if conversations() == 0:
    sys.exit("greetd was sent no create_session after a password was submitted")

sendkey("esc"); time.sleep(1.0)
cmd("screendump", filename=f"{outdir}/4-username-again.ppm")
typestr("demo")
sendkey("ret"); time.sleep(1.5)

for k in ("tab", "tab", "right", "shift-tab", "spc"):
    sendkey(k); time.sleep(0.3)
cmd("screendump", filename=f"{outdir}/5-picker.ppm")
sendkey("shift-tab"); time.sleep(0.3)

before_typing = conversations()
typestr("hunter2")
sendkey("alt-f8")
time.sleep(0.4)
cmd("screendump", filename=f"{outdir}/6-typed.ppm")
if conversations() != before_typing:
    sys.exit("greetd was sent a create_session while a password was being typed")
sendkey("ret")
time.sleep(4)
if conversations() != before_typing + 1:
    sys.exit("submitting the password did not open exactly one conversation")
print("driven; check guest log for VIGIL-EXIT:0")
