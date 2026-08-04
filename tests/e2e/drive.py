#!/usr/bin/env python3
"""Host-side driver: QMP keyboard injection + screendumps against the VM
launched by run.sh. Wrong password first (error path), then the correct one;
success is the guest printing VIGIL-EXIT:0."""
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

cmd("qmp_capabilities")
cmd("screendump", filename=f"{outdir}/1-login.ppm")
for k in "wrong":
    sendkey(k); time.sleep(0.15)
sendkey("ret"); time.sleep(2.0)
cmd("screendump", filename=f"{outdir}/2-error.ppm")
for k in ["h", "u", "n", "t", "e", "r", "2"]:
    sendkey(k); time.sleep(0.15)
time.sleep(0.4)
cmd("screendump", filename=f"{outdir}/3-typed.ppm")
sendkey("ret")
time.sleep(4)
print("driven; check guest log for VIGIL-EXIT:0")
