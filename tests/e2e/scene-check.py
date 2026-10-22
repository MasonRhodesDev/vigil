#!/usr/bin/env python3
"""Checks the frames run-scene.sh captured against the scene spec's rules.

Each screen is sampled from the host (screendump), so every instant below is
good to one capture sweep (tens of milliseconds); the thresholds allow for
that. Nothing here asserts exact pixels except the ground colour and the debug
border, which are exact by construction.

  1. every screen is lit in the splash stage, and they light together
  2. the ground covers each screen edge to edge (a pixel inside every corner)
  3. the mark has the same apparent size and position on every screen, on the
     short-side basis, including the one that is rotated
  4. the glow shows (a pixel beside the mark gains the accent)
  5. splash to greeter is one cross-fade: it starts on every screen together,
     moves one way and never overshoots
  6. after login every screen eases back to the rest frame together
"""
import json
import os
import re
import sys

import numpy as np

work = sys.argv[1]
HERE = os.path.dirname(os.path.abspath(__file__))
spec = json.load(open(os.path.join(HERE, "..", "fixtures", "scene.json")))
BG = tuple(int(spec["palette"]["bg"][i:i + 2], 16) for i in (1, 3, 5))
MARK = tuple(int(spec["palette"]["mark"][i:i + 2], 16) for i in (1, 3, 5))
SPLASH = tuple(int(spec["stages"]["greeter-splash"]["debug"][i:i + 2], 16) for i in (1, 3, 5))
GREETER = tuple(int(spec["stages"]["greeter"]["debug"][i:i + 2], 16) for i in (1, 3, 5))
LOGO_FRAC = spec["geometry"]["logo_frac"]
CENTRE = spec["geometry"]["centre_frac_of_h"]
# The fixture mark (tests/fixtures/mark.svg) spans 88 x 84 of its 100 box.
MARK_W, MARK_H = 0.88, 0.84
# transform per virtio-gpu card, as written by run-scene.sh's profile.
TRANSFORM = {"gpu0": 0, "gpu1": 0, "gpu2": 3}

cap = json.load(open(os.path.join(work, "capture.json")))
heads = sorted({c["dev"] for c in cap})
fails = []


def check(ok, what):
    print(("PASS  " if ok else "FAIL  ") + what)
    if not ok:
        fails.append(what)


def load(name):
    with open(os.path.join(work, name), "rb") as fh:
        data = fh.read()
    magic, dims, _max, px = data.split(b"\n", 3)
    w, h = map(int, dims.split())
    return np.frombuffer(px, dtype=np.uint8).reshape(h, w, 3)


def near(a, b, tol=2):
    return all(abs(int(x) - int(y)) <= tol for x, y in zip(a, b))


def stage_of(im):
    h, w = im.shape[:2]
    e = tuple(im[h // 2, 2])
    if near(e, SPLASH, 1):
        return "splash"
    if near(e, GREETER, 1):
        return "greeter"
    if near(e, (0, 0, 0), 1):
        return "dark"
    return "other"


def to_scene(t, w, h):
    """Panel pixel to scene pixel, the inverse of vigil's scene_to_panel for
    the quarter turns the run uses (0 and 3)."""
    def f(x, y):
        return (y, w - 1 - x) if t == 3 else (x, y)
    return f


series = {}
for dev in heads:
    rows = []
    for c in cap:
        if c["dev"] != dev:
            continue
        im = load(c["file"])
        rows.append({"t": (c["t"] + c["t_done"]) / 2, "im": im, "stage": stage_of(im)})
    series[dev] = rows

# 1. lit together
first = {}
for dev, rows in series.items():
    lit = [r for r in rows if r["stage"] == "splash"]
    check(bool(lit), f"{dev}: lit in the greeter-splash stage")
    if lit:
        first[dev] = lit[0]["t"]
if len(first) == len(heads):
    spread = (max(first.values()) - min(first.values())) * 1000
    print(f"      first light per screen (s): " + ", ".join(f"{d} {t:.3f}" for d, t in first.items()))
    check(spread <= 150, f"screens light together (spread {spread:.0f} ms, capture precision about 60 ms)")

# vigil's own first-frame stamps
log = open(os.path.join(work, "vigil.log"), errors="replace").read()
stamps = [int(m) for m in re.findall(r"\[t\+(\d+)ms\] splash first frame", log)]
if stamps:
    print(f"      vigil first-frame stamps (ms): {stamps}")
    check(max(stamps) - min(stamps) <= 100, f"vigil's own stamps agree (spread {max(stamps) - min(stamps)} ms)")

check(os.path.exists(os.path.join(work, "first-frame-hook")),
      "[splash] on_first_frame ran")
m = re.search(r"first frame presented on every output", log)
check(bool(m), "vigil logged the first frame on every output")
for line in re.findall(r"splash first frame on [^\n]*", log):
    print("      " + line)

# 2. cover and 3. the mark
norm = {}
for dev, rows in series.items():
    lit = [r for r in rows if r["stage"] == "splash"]
    if not lit:
        continue
    im = lit[0]["im"]
    h, w = im.shape[:2]
    b = max(4, round(min(w, h) * spec["debug"]["border_frac"]))
    corners = [im[b + 3, b + 3], im[b + 3, w - b - 4], im[h - b - 4, b + 3], im[h - b - 4, w - b - 4]]
    check(all(near(c, BG, 1) for c in corners), f"{dev}: ground to every corner ({w}x{h})")
    ys, xs = np.where(np.all(np.abs(im.astype(int) - np.array(MARK)) <= 6, axis=2))
    if len(xs) == 0:
        check(False, f"{dev}: the mark is on screen")
        continue
    t = TRANSFORM.get(dev, 0)
    cx, cy = (xs.min() + xs.max()) / 2, (ys.min() + ys.max()) / 2
    bw, bh = xs.max() - xs.min() + 1, ys.max() - ys.min() + 1
    sw, sh = (h, w) if t % 2 else (w, h)
    sx, sy = to_scene(t, w, h)(cx, cy)
    if t % 2:
        bw, bh = bh, bw
    S = min(sw, sh)
    norm[dev] = (bw / S, bh / S, sx / sw, sy / sh)
    print(f"      {dev}: mark {bw}x{bh} px on a {sw}x{sh} scene, {bw / S:.4f} x {bh / S:.4f} of the short side, centre ({sx / sw:.3f}, {sy / sh:.3f})")
for dev, (nw, nh, fx, fy) in norm.items():
    check(abs(nw - LOGO_FRAC * MARK_W) < 0.01 and abs(nh - LOGO_FRAC * MARK_H) < 0.01,
          f"{dev}: mark size is the spec's {LOGO_FRAC} of the short side")
    check(abs(fx - 0.5) < 0.004 and abs(fy - CENTRE) < 0.02,
          f"{dev}: mark is centred, {CENTRE} of the height")

# 4. glow: a pixel just above the mark gains blue over the ground
for dev, rows in series.items():
    t = TRANSFORM.get(dev, 0)
    best = 0
    for r in rows:
        if r["stage"] != "splash":
            continue
        im = r["im"]
        h, w = im.shape[:2]
        sw, sh = (h, w) if t % 2 else (w, h)
        S = min(sw, sh)
        sx, sy = sw // 2, int(sh * CENTRE - S * LOGO_FRAC * 0.5 - S * LOGO_FRAC * 0.12)
        px, py = {0: (sx, sy), 1: (sy, sw - 1 - sx), 3: (sh - 1 - sy, sx)}[t]
        best = max(best, int(im[py, px, 2]) - BG[2])
    check(best >= 3, f"{dev}: the glow shows beside the mark (peak +{best} blue over the ground)")

# 5. the cross-fade
def probe(im, t):
    """The mean of a patch low on the screen (away from the mark and the tag),
    so the wallpaper's grain does not read as motion."""
    h, w = im.shape[:2]
    sw, sh = (h, w) if t % 2 else (w, h)
    sx, sy = int(sw * 0.5), int(sh * 0.82)
    px, py = {0: (sx, sy), 1: (sy, sw - 1 - sx), 3: (sh - 1 - sy, sx)}[t]
    return im[py - 6:py + 7, px - 6:px + 7].reshape(-1, 3).mean(axis=0)


starts, ends, overshoot = {}, {}, {}
for dev, rows in series.items():
    t = TRANSFORM.get(dev, 0)
    dist = [(r["t"], np.abs(probe(r["im"], t) - np.array(BG)).sum(), r["stage"]) for r in rows if r["stage"] != "dark"]
    greeter = [d for d in dist if d[2] == "greeter"]
    if not greeter:
        check(False, f"{dev}: reaches the greeter")
        continue
    final = float(np.median([d[1] for d in greeter[len(greeter) // 4: len(greeter) // 2] or greeter]))
    check(final > 30, f"{dev}: the greeter shows its wallpaper (ground distance {final:.0f})")
    pre = [d for d in dist if d[2] == "splash"]
    leave = next((d[0] for d in dist if d[1] > 8 and d[2] != "dark"), None)
    starts[dev] = leave
    fade = [d for d in dist if leave is not None and leave <= d[0] <= leave + spec["motion"]["stage_fade_s"] + 0.1]
    vals = [d[1] for d in fade]
    mono = all(b >= a - 8 for a, b in zip(vals, vals[1:]))
    check(mono, f"{dev}: the fade moves one way ({len(vals)} samples)")
    peak = max(d[1] for d in greeter[: max(1, len(greeter) // 3)])
    check(peak <= final * 1.15 + 6, f"{dev}: the fade does not overshoot the wallpaper (peak {peak:.0f}, settled {final:.0f})")
if len(starts) == len(heads) and None not in starts.values():
    spread = (max(starts.values()) - min(starts.values())) * 1000
    check(spread <= 150, f"the cross-fade starts on every screen together (spread {spread:.0f} ms)")

# 6. after login: the rest frame, together
ends = {}
for dev, rows in series.items():
    t = TRANSFORM.get(dev, 0)
    # The run ends with vigil's exit, after which a VM screen goes dark; the
    # last picture that is not dark is the hand-off's result.
    dist = [(r["t"], np.abs(probe(r["im"], t) - np.array(BG)).sum()) for r in rows if r["stage"] != "dark"]
    # first time after the greeter is up that the ground is back for good
    back = None
    for i in range(len(dist) - 1, -1, -1):
        if dist[i][1] > 8:
            back = dist[i + 1][0] if i + 1 < len(dist) else None
            break
    ends[dev] = back
    check(back is not None, f"{dev}: ends on the rest frame (ground distance {dist[-1][1]:.0f})")
if len(ends) == len(heads) and None not in ends.values():
    spread = (max(ends.values()) - min(ends.values())) * 1000
    check(spread <= 200, f"the hand-off to rest ends on every screen together (spread {spread:.0f} ms)")
m = re.search(r"exiting \(rest frame shown, (\d+) ms after login\)", log)
if m:
    print(f"      vigil exits {m.group(1)} ms after the login was accepted")

print("SCENE FAIL" if fails else "SCENE PASS")
sys.exit(1 if fails else 0)
