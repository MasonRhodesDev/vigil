#!/usr/bin/env python3
"""Pixel assertions over the screendumps drive.py captured.

Region samples and distinct-colour counts, not golden images: text
rasterization differs across host font stacks and the guest inherits the
HOST's installed theme, so exact pixels would be flaky (same reasoning as
crates/vigil/tests/theme_states.rs). What is stable: a rendered frame has
many distinct colours, a dead renderer's frame has almost none, and the
login card occupies a fixed centered region at every stage.

The stages are display 0 (QMP screendump without a device argument
captures the first virtio-gpu console). The second output is captured once,
by device id, for its large clock.

Usage: check_frames.py WORKDIR
"""
import sys

GREETER_STAGES = [
    "1-username.ppm", "2-password.ppm", "2c-back.ppm", "3-error.ppm",
    "4-username-again.ppm", "5-picker.ppm", "6-typed.ppm",
]
# 2b-vt2.ppm is the VT2 text console — nothing of vigil's to assert.
DISTINCT_MIN = 8


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


def card_rect(img):
    """The card's field and status rows, recentered on the actual frame
    (1280x800 geometry in theme_states.rs). It starts below where either
    clock size ends, so the clock alone cannot pass for a rendered card."""
    w, h, _ = img
    return (w // 2 - 175, h // 2 + 25, w // 2 + 175, h // 2 + 88)


def sample(img, rect, step=4):
    w, h, px = img
    x0, y0, x1, y1 = rect
    out = []
    for y in range(y0, y1, step):
        row = y * w
        for x in range(x0, x1, step):
            i = (row + x) * 3
            out.append(px[i:i + 3])
    return out


# Clock glyphs fill about a quarter of their bounding box; a GL surface
# without stencil fills each glyph's whole box (issue: greeter clock as
# white rectangles), which measured 0.69 here against 0.28 for real text.
CLOCK_FILL_MAX = 0.5


def clock_fill(img):
    """Fraction of the clock's bounding box that is ink, on an output with
    no card: the only bright pixels above the footer are the clock's."""
    w, h, px = img
    ink = []
    for y in range(0, h - 120):
        row = y * w
        for x in range(w):
            i = (row + x) * 3
            if px[i] + px[i + 1] + px[i + 2] > 3 * 160:
                ink.append((x, y))
    if not ink:
        return None
    xs = [x for x, _ in ink]
    ys = [y for _, y in ink]
    area = (max(xs) - min(xs) + 1) * (max(ys) - min(ys) + 1)
    return len(ink) / area


def main(workdir):
    failures = []
    try:
        second = load_ppm(f"{workdir}/0b-second-output.ppm")
    except OSError:
        failures.append("0b-second-output.ppm: missing")
    else:
        fill = clock_fill(second)
        if fill is None:
            failures.append("0b-second-output.ppm: no clock drawn")
        elif fill > CLOCK_FILL_MAX:
            failures.append(
                f"0b-second-output.ppm: clock fills {fill:.2f} of its box "
                f"(over {CLOCK_FILL_MAX}: glyphs drawn as rectangles)")
    imgs = {}
    for name in GREETER_STAGES:
        try:
            img = load_ppm(f"{workdir}/{name}")
        except OSError:
            failures.append(f"{name}: missing")
            continue
        imgs[name] = img
        n = distinct_colors(img)
        if n < DISTINCT_MIN:
            failures.append(f"{name}: frame is blank ({n} distinct colours)")
        n = len(set(sample(img, card_rect(img))))
        if n < DISTINCT_MIN:
            failures.append(f"{name}: card region is empty ({n} distinct colours)")

    def card(name):
        return sample(imgs[name], card_rect(imgs[name]))

    if "1-username.ppm" in imgs and "2-password.ppm" in imgs \
            and card("1-username.ppm") == card("2-password.ppm"):
        failures.append(
            "1-username vs 2-password: card did not change (stage transition not rendered)")
    if "5-picker.ppm" in imgs and "6-typed.ppm" in imgs \
            and card("5-picker.ppm") == card("6-typed.ppm"):
        failures.append(
            "5-picker vs 6-typed: card did not change (typed bullets not rendered)")

    for failure in failures:
        print(f"FRAME FAIL: {failure}", file=sys.stderr)
    if failures:
        sys.exit(1)
    print(f"frames ok ({len(GREETER_STAGES)} stages)")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        print("usage: check_frames.py WORKDIR", file=sys.stderr)
        sys.exit(2)
    main(sys.argv[1])
