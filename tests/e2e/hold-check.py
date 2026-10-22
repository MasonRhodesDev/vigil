#!/usr/bin/env python3
"""Checks a run-scene.sh run with VIGIL_E2E_NEXT set: what the screens show
between vigil's exit and a next owner's first frame.

  hold-check.py WORKDIR hold     [render] hold_last_frame_on_exit was on
  hold-check.py WORKDIR          the control: it was off, and the run must show
                                 the black gap the holder exists to remove

With the holder on, per screen: after the rest frame is up there is no dark
frame before the next owner's pattern, the rest frame stayed up until then,
the pattern is still up at the end of the run (the holder's exit did not blank
it), the holder existed at vigil's exit and is gone after the handover, and the
next owner was not refused master.
"""
import json
import os
import re
import sys

import numpy as np

work = sys.argv[1]
hold = len(sys.argv) > 2 and sys.argv[2] == "hold"
# A compositor's first commit may itself be black (and its first real frame
# comes later, on the owner's schedule), so against Hyprland the claim is the
# weaker, honest one: the rest frame stays up until the owner's first commit.
compositor = os.environ.get("VIGIL_E2E_NEXT") == "hyprland"
HERE = os.path.dirname(os.path.abspath(__file__))
spec = json.load(open(os.path.join(work, "scene.json")))
BG = np.array([int(spec["palette"]["bg"][i:i + 2], 16) for i in (1, 3, 5)])
MARK = np.array([int(spec["palette"]["mark"][i:i + 2], 16) for i in (1, 3, 5)])
cap = json.load(open(os.path.join(work, "capture.json")))
log = open(os.path.join(work, "vigil.log"), errors="replace").read()
heads = sorted({c["dev"] for c in cap})
fails = []


def check(ok, what):
    print(("PASS  " if ok else "FAIL  ") + what)
    if not ok:
        fails.append(what)


def load(name):
    with open(os.path.join(work, name), "rb") as fh:
        data = fh.read()
    _magic, dims, _max, px = data.split(b"\n", 3)
    w, h = map(int, dims.split())
    return np.frombuffer(px, dtype=np.uint8).reshape(h, w, 3)


def classify(im):
    h, w = im.shape[:2]
    if int(im[::max(1, h // 40), ::max(1, w // 40)].max()) <= 3:
        return "dark"
    # Rest: ground at the corners (inside the debug border) and the mark's
    # colour present in numbers.
    b = max(4, round(min(w, h) * spec["debug"]["border_frac"])) + 3
    corners = [im[b, b], im[b, w - b - 1], im[h - b - 1, b], im[h - b - 1, w - b - 1]]
    ground = all(np.abs(c.astype(int) - BG).max() <= 2 for c in corners)
    # Every 3rd pixel each way is plenty to find a mark hundreds of pixels wide.
    sub = im[::3, ::3].astype(np.int16)
    marks = int(np.all(np.abs(sub - MARK) <= 6, axis=2).sum())
    if ground and marks > 50:
        return "rest"
    if not ground:
        return "other"
    return "mixed"


series = {}
for dev in heads:
    rows = []
    for c in cap:
        if c["dev"] == dev:
            rows.append((c["t"], classify(load(c["file"]))))
    series[dev] = rows

gaps = {}
rest_end = {}
for dev, rows in series.items():
    kinds = [k for _, k in rows]
    print(f"      {dev}: " + " ".join(f"{k[0]}" for k in kinds[-60:]) + "   (r rest, d dark, o other, m mixed; last 60 frames)")
    # The rest frame that matters is the last one: the greeter fades to it
    # after login, and then the next owner takes over.
    rest_idx = [i for i, k in enumerate(kinds) if k == "rest"]
    check(bool(rest_idx), f"{dev}: reaches the rest frame")
    if not rest_idx:
        continue
    # The next owner's frame is the final run of "other" frames (up to three
    # trailing frames may be the owner being killed at the end of the run). A
    # single odd frame right after the rest frame, as a screen goes black, is
    # not a next owner.
    end = len(kinds)
    for _ in range(3):
        if end > 0 and kinds[end - 1] != "other":
            end -= 1
    nxt = None
    if end > 0 and kinds[end - 1] == "other":
        nxt = end - 1
        while nxt > 0 and kinds[nxt - 1] == "other":
            nxt -= 1
    before = [i for i in rest_idx if nxt is None or i < nxt]
    if not before:
        before = rest_idx
    last_rest = before[-1]
    run_start = last_rest
    while run_start > 0 and kinds[run_start - 1] == "rest":
        run_start -= 1
    if not hold and nxt is None:
        print(f"      {dev}: control: no next owner frame seen; not a check")
        continue
    check(nxt is not None, f"{dev}: the next owner's frame shows")
    if nxt is None:
        continue
    seg = kinds[last_rest + 1:nxt]
    dark = seg.count("dark")
    t_rest = rows[run_start][0]
    t_last = rows[last_rest][0]
    t_next = rows[nxt][0]
    gaps[dev] = (dark, t_next - t_last)
    print(f"      {dev}: rest from {t_rest:.2f}s to {t_last:.2f}s, next owner at {t_next:.2f}s: "
          f"{dark} dark frame(s) of {len(seg)} between, gap {t_next - t_last:.2f}s")
    if hold and compositor:
        rest_end[dev] = t_last
        check(t_last - t_rest >= float(os.environ.get("VIGIL_E2E_NEXT_DELAY", "3")) * 0.8,
              f"{dev}: the rest frame stayed up through vigil's exit and the whole delay ({t_last - t_rest:.2f}s)")
        check(all(k == "other" for k in kinds[-20:-8]), f"{dev}: the compositor's frame is still up after the holder is gone")
    elif hold:
        check(dark == 0, f"{dev}: no black between the rest frame and the next owner")
        check(t_next - t_last <= 0.5, f"{dev}: the rest frame stays up until the next owner's first frame (gap {t_next - t_last:.2f}s)")
        check(t_last - t_rest >= float(os.environ.get("VIGIL_E2E_NEXT_DELAY", "3")) * 0.8,
              f"{dev}: the rest frame stayed up through vigil's exit and the whole delay ({t_last - t_rest:.2f}s)")
        tail = kinds[-20:-8]
        check(all(k == "other" for k in tail), f"{dev}: the next owner's frame is still up after the holder is gone")
    else:
        check(dark >= 3, f"{dev}: control: the screen goes black before the next owner ({dark} dark frames, gap {t_next - t_last:.2f}s)")

if hold and compositor and len(rest_end) == len(heads):
    # Host and guest clocks differ, but the spacing between the screens'
    # last rest frames must match the spacing between the holder's own
    # "crtc moved" timestamps: a screen leaves the rest frame exactly when the
    # owner commits to it, not before.
    moved = sorted(int(m) for m in re.findall(r"vigil-hold: \[\+(\d+)ms\] crtc", log))
    ends = sorted(rest_end.values())
    if len(moved) == len(ends):
        for i in range(1, len(ends)):
            d_frames, d_log = ends[i] - ends[0], (moved[i] - moved[0]) / 1000
            check(abs(d_frames - d_log) <= 0.3,
                  f"screen {i + 1} left the rest frame {d_frames:.2f}s after the first, the holder saw its handover {d_log:.2f}s after the first")
    else:
        check(False, f"the holder logged {len(moved)} handovers for {len(ends)} screens")
if hold:
    m = re.search(r"HOLDERS-AT-EXIT: ?(.*)", log)
    check(bool(m and m.group(1).strip()), f"a vigil-hold child existed when vigil exited ({m.group(1).strip() if m else 'no line'})")
    m = re.search(r"HOLDERS-AFTER-NEXT: ?(.*)", log)
    check(bool(m) and not m.group(1).strip(), "no vigil-hold child remains after the handover")
    check(len(re.findall(r"vigil-hold: released, handed over", log)) == 1, "the holder logged one handover")
    check("vigil-hold: released, no next owner" not in log, "the holder did not time out")
    check("hold: not holding" not in log, "vigil did not fall back to a plain exit")
    check("dropped DRM master" in log or "not master" in log, "vigil dealt with DRM master before exiting")
    for l in re.findall(r"vigil-hold: \[\+\d+ms\] crtc[^\n]*", log):
        print("      " + l)
    for l in re.findall(r"vigil: [^\n]*hold[^\n]*", log):
        print("      " + l)
else:
    m = re.search(r"HOLDERS-AT-EXIT: ?(.*)", log)
    check(bool(m) and not m.group(1).strip(), "control: no holder exists")
nl = open(os.path.join(work, "next.log"), errors="replace").read() if os.path.exists(os.path.join(work, "next.log")) else ""
refused = "failed to set mode" in nl.lower() or "permission denied" in nl.lower()
if hold and compositor:
    pass
elif hold:
    check("NEXT-OWNER-SET" in nl and not refused, "the next owner set its mode without being refused master")
else:
    print("      control: next owner " + ("was refused on a card (" + nl.strip().splitlines()[-1][:60] + ")" if refused else "set its mode everywhere") + "; not a check")
print("HOLD FAIL" if fails else "HOLD PASS")
sys.exit(1 if fails else 0)
