#!/usr/bin/env python3
"""Host-side driver for run-scene.sh: watch every screen from the moment
vigil starts, log in once the greeter is up everywhere, and keep watching until
vigil exits. Every screendump is stamped with the host's monotonic clock; the
stamps and file names go to capture.json for scene-check.py."""
import json, os, socket, sys, time

qmp_path, outdir = sys.argv[1], sys.argv[2]
HEADS = [("gpu0", 0), ("gpu1", 0), ("gpu2", 0)]
LIMIT = float(os.environ.get("VIGIL_E2E_SECONDS", "40"))

def log_has(text):
    try:
        return text in open(f"{outdir}/vigil.log", errors="replace").read()
    except FileNotFoundError:
        return False

t0 = time.time()
while not os.path.exists(qmp_path):
    if time.time() - t0 > 60:
        sys.exit("no QMP socket")
    time.sleep(0.2)
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
    cmd("human-monitor-command", **{"command-line": f"sendkey {k}"})

def typestr(text):
    for k in text:
        sendkey(k)
        time.sleep(0.12)

cmd("qmp_capabilities")
while not log_has("VIGIL-START"):
    if time.time() - t0 > 120:
        sys.exit("guest never started vigil")
    time.sleep(0.2)

frames, n = [], 0
start = time.monotonic()
logged_in = False
greeter_since = None
exited = None
while time.monotonic() - start < LIMIT:
    for dev, head in HEADS:
        path = f"{outdir}/f{n:05d}-{dev}-{head}.ppm"
        t = time.monotonic() - start
        r = cmd("screendump", filename=path, device=dev, head=head)
        t_done = time.monotonic() - start
        if "error" not in r:
            frames.append({"t": round(t, 4), "t_done": round(t_done, 4),
                           "dev": dev, "head": head, "file": os.path.basename(path)})
        n += 1
    if not logged_in and log_has("selected user") is False and log_has("fade done"):
        greeter_since = greeter_since or time.monotonic()
        if time.monotonic() - greeter_since > 1.5:
            logged_in = True
            typestr("demo"); sendkey("ret"); time.sleep(1.0)
            typestr("hunter2"); sendkey("ret")
    if log_has("VIGIL-EXIT"):
        # A few more sweeps: the last screens after the exit are the rest frame.
        # With a next owner (VIGIL_E2E_NEXT) keep watching until it is done.
        exited = exited or time.monotonic()
        if log_has("NEXT-DONE") if os.environ.get("VIGIL_E2E_NEXT") else time.monotonic() - exited > 0.5:
            break
json.dump(frames, open(f"{outdir}/capture.json", "w"))
print(f"captured {len(frames)} frames in {time.monotonic() - start:.1f}s")
