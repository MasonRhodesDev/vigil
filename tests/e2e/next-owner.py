#!/usr/bin/env python3
import glob
import os
import re
import subprocess
import sys

log = open(sys.argv[1], "w")
procs = []


def busid(card):
    for link in glob.glob("/dev/dri/by-path/pci-*-card"):
        if os.path.realpath(link) == os.path.realpath(card):
            return "pci:" + os.path.basename(link)[len("pci-"):-len("-card")]
    return None


for card in sorted(glob.glob("/dev/dri/card*")):
    dev = busid(card)
    out = subprocess.run(["modetest", "-D", dev, "-ec"], stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True).stdout
    enc_crtc = {m[0]: m[1] for m in re.findall(r"^(\d+)\s+(\d+)\s+\w+\s+0x", out, re.M)}
    m = re.search(r"^(\d+)\s+(\d+)\s+connected\s+\S+.*\n(?:.*\n)*?\s*#0 (\d+x\d+)", out, re.M)
    if not m:
        print(f"{card}: no connected connector; modetest said:\n{out}", file=log, flush=True)
        continue
    conn, enc, mode = m.groups()
    crtc = enc_crtc.get(enc)
    if not crtc:
        print(f"{card}: encoder {enc} has no crtc", file=log, flush=True)
        continue
    print(f"{card}: connector {conn} crtc {crtc} mode {mode}", file=log, flush=True)
    p = subprocess.Popen(["modetest", "-D", dev, "-s", f"{conn}@{crtc}:{mode}"],
                         stdin=subprocess.PIPE, stdout=log, stderr=log)
    procs.append(p)
print("NEXT-OWNER-SET", file=log, flush=True)
for p in procs:
    p.wait()
