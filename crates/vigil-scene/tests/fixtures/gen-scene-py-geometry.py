#!/usr/bin/env python3
import os
import sys

checkout = sys.argv[1]
sys.path.insert(0, os.path.join(checkout, "lib"))
import scene  # noqa: E402

SIZES = [
    (640, 480), (800, 600), (1024, 768), (1280, 720), (1280, 800), (1280, 1024),
    (1366, 768), (1440, 900), (1600, 900), (1680, 1050), (1920, 1080), (1920, 1200),
    (2048, 1536), (2560, 1080), (2560, 1440), (2560, 1600), (2880, 1800), (3440, 1440),
    (3840, 1600), (3840, 2160), (5120, 1440), (5120, 2880), (7680, 4320),
    (1201, 799), (1023, 767), (1999, 1333), (333, 577), (1366, 769), (2161, 3841),
]
REFS = [0, 1080, 1440, 2160]
KEYS = ["logo", "glow", "logo_x", "logo_y", "glow_x", "glow_y",
        "bar_w", "bar_h", "bar_x", "bar_y", "gap"]

print("\t".join(["w", "h", "ref"] + KEYS))
for w, h in SIZES:
    for W, H in sorted({(w, h), (h, w)}):
        for ref in REFS:
            g = scene.geometry(W, H, ref)
            print("\t".join(str(v) for v in [W, H, ref] + [g[k] for k in KEYS]))
