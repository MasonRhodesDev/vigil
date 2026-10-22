#!/usr/bin/env python3
"""Plays the session's first frame in the scene run: for every DRM card, find
the connected connector and the CRTC vigil lit, and put a modetest test
pattern on it (a real DRM client taking master and presenting its own
framebuffer). Stays alive until killed, so the pattern is not torn down.
Usage: next-owner.py LOGFILE"""
import glob
import os
import re
import subprocess
import sys

log = open(sys.argv[1], "w")
procs = []


def busid(card):
    """modetest -D takes a bus id; with several cards of one driver -M cannot
    tell them apart."""
    for link in glob.glob("/dev/dri/by-path/pci-*-card"):
        if os.path.realpath(link) == os.path.realpath(card):
            return "pci:" + os.path.basename(link)[len("pci-"):-len("-card")]
    return None


for card in sorted(glob.glob("/dev/dri/card*")):
    dev = busid(card)
    out = subprocess.run(["modetest", "-D", dev, "-ec"], stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True).stdout
    enc_crtc = {m[0]: m[1] for m in re.findall(r"^(\d+)\s+(\d+)\s+\w+\s+0x", out, re.M)}
    # Connector rows: "id encoder status name size..." then a modes table.
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
    # stdin stays open so modetest does not tear the pattern down.
    p = subprocess.Popen(["modetest", "-D", dev, "-s", f"{conn}@{crtc}:{mode}"],
                         stdin=subprocess.PIPE, stdout=log, stderr=log)
    procs.append(p)
print("NEXT-OWNER-SET", file=log, flush=True)
for p in procs:
    p.wait()
