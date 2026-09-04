#!/usr/bin/env python3
"""Protocol-ordering checker for the nested lock harness (issue #44).

Parses a vigil-lock stderr capture produced with WAYLAND_DEBUG=1
(both wayland-backend debug formats: sys `[ts] iface#id.msg(args)` and
rs `[ts][rs] -> iface@id.req(args)` / `[ts][rs] <- iface@id.event, (args)`)
and asserts:

  --expect locked      the compositor granted the lock (locked event seen)
  --expect cancelled   the lock was NEVER requested (cancel-before-commit)
  --handoff            every warning surface teardown happens only after
                       every lock surface's first commit-with-buffer
                       (the handoff never exposes the desktop)
  --locked-to-commit-ms N
                       every output that went through the handoff committed
                       its first lock buffer within N ms of the `locked`
                       event (see "The black window" below); the per-output
                       numbers print on every run either way
  --first-frame-not-black
                       with VIGIL_FRAME_HASH=1 in the environment of the
                       traced locker: the first lock-surface frame of any
                       output whose warning painted is not the all-black
                       buffer (see "Frame hashes" below)
  --outputs N          at least N lock surfaces were created
  --reveal             the post-unlock reveal overlay (issue #52) was
                       created after `locked`, unlock_and_destroy came after
                       every reveal surface request (and after its first
                       buffer commit whenever the compositor configured it
                       before the unlock), and reveal teardown only after
                       unlock_and_destroy
  --no-reveal          no vigil-reveal overlay was ever created (the
                       instant-unlock default: reveal is opt-in only)
  --no-layer           no layer surface at all (`--immediate` path)
  --no-capture-only    only run the capture-bind check

Layer surfaces are classified by the namespace passed to
get_layer_surface: "vigil-warning" (pre-lock overlay, --handoff) versus
"vigil-reveal" (post-unlock overlay, --reveal).

The capture check always runs: binding any screencopy/image-capture/
export-dmabuf global is forbidden (capture-free warning, ADR).

The black window (issue #86)
----------------------------
The compositor stops rendering normal surfaces at the `locked` event, and
shows nothing on an output until that output's lock surface commits a
buffer. So the interval that is actually uncovered is, per output:

    locked_to_commit = first lock-surface commit-with-buffer - `locked`

That is what --locked-to-commit-ms gates. WAYLAND_DEBUG stamps every line
with a monotonic `[ ms.us ]`, so it is measured, not inferred. Only outputs
that went through the handoff are eligible: an output hotplugged into an
already-locked session has no relationship to a `locked` event that fired
before it existed, and including it would gate a number that means nothing.

A second, looser number is printed but NOT gated: from the warning
surface's last commit (or the lock request) to the same first lock commit.
It is an upper bound on the same window rather than the window itself,
because the warning surface keeps being *displayed* after it stops
committing - the ramp's commit-only ticks (`ramp_commit_only`) mean it can
stop committing buffers hundreds of milliseconds before `locked`. Gating it
made a flake, not a check: S9 measured 258 ms against a 250 ms bound on one
machine and 249 ms on another, and the black-committing code it was meant
to catch passed it at 3 ms.

Read either number for what it is: sway-tier timing is NOT Hyprland-tier
timing. This harness runs a headless pixman wlroots against an unoptimized
debug build, and its numbers say nothing about the seat's. What a bound can
catch is a regression that adds a whole scheduling round before the first
commit. What it cannot catch is #86 itself - the broken handoff committed
*fast*, and committed black. That is --first-frame-not-black's job.

Frame hashes (issue #86)
------------------------
The gap being short does not mean the frame is right: the old handoff
committed instantly and committed *black*. `VIGIL_FRAME_HASH=1` makes
vigil-lock emit a span-lines `event=frame.hash` record (FNV-1a over the
committed pixels) for every frame it commits, onto the same stderr this
trace captures. `--first-frame-not-black` uses those to assert the
positive: an output whose warning painted must not receive an all-black
first lock frame. Hashing costs a pass over the whole buffer, so it
inflates every timing in a trace that carries it — do not gate a gap on a
hashed run.

Full pixel continuity (warning's last frame == lock's first frame) is NOT
asserted here and cannot be: the warning buffer is ARGB after the overlay
blend and the lock buffer is XRGB straight out of the scene shadow, so the
two agree on colour and disagree on the fourth byte. Equality belongs to a
metal check that compares channels, alongside seat verification.
"""

import argparse
import re
import sys

SEND = re.compile(r"^\[\s*([0-9.]+)\](?:\[rs\])?\s*(?:\[discarded\])?\s*-> ([a-z0-9_]+)[#@](\d+)\.([a-z0-9_]+)\((.*)\)")
EVENT = re.compile(r"^\[\s*([0-9.]+)\](?:\[rs\])?\s*(?:<- )?([a-z0-9_]+)[#@](\d+)\.([a-z0-9_]+)[,(]")
# span-lines records share this stderr; they carry no `[ts]` prefix, so the
# protocol regexes above never match one.
HASH = re.compile(r"^event=frame\.hash\s")

FNV_OFFSET = 0xCBF29CE484222325
FNV_PRIME = 0x100000001B3
MASK64 = (1 << 64) - 1


def black_hash(byte_count):
    """The fingerprint vigil-lock emits for a buffer of `byte_count` zeros.

    FNV-1a over zeros never XORs anything in, so the whole loop collapses to
    repeated multiplication: h = offset * prime**n. Closed form rather than
    a Python loop over eight megabytes per output.
    """
    return f"{(FNV_OFFSET * pow(FNV_PRIME, byte_count, MASK64 + 1)) & MASK64:016x}"


def parse_record(line):
    """span-lines record -> dict. Keys and values never contain space or `=`."""
    fields = {}
    for token in line.split():
        key, sep, value = token.partition("=")
        if sep:
            fields[key] = value
    return fields

def locked_to_commit(locked_ms, first_lock_commit_ms, handoff_outputs):
    """{wl_output: ms} from the `locked` event to that output's first buffer.

    Restricted to outputs that carried a warning surface, i.e. the ones that
    were on screen when the lock was granted. An output hotplugged into an
    already-locked session commits whenever it is configured, which is not a
    handoff and not a measurement of one.
    """
    if locked_ms is None:
        return {}
    return {
        output: commit_ms - locked_ms
        for output, commit_ms in first_lock_commit_ms.items()
        if output in handoff_outputs and commit_ms >= locked_ms
    }


def handoff_gaps(lock_request_ms, first_lock_commit_ms, warning_commits_ms):
    """{wl_output: (gap_ms, start_ms, commit_ms)} for every output that locked.

    The gap starts at the last thing the user could still have been shown on
    that output — its warning surface's last commit — or at the lock request
    if the warning had already stopped committing, whichever is later. It
    ends at the lock surface's first commit-with-buffer.
    """
    gaps = {}
    for output, commit_ms in first_lock_commit_ms.items():
        candidates = [ms for ms in warning_commits_ms.get(output, []) if ms <= commit_ms]
        start = max(candidates) if candidates else None
        if lock_request_ms is not None and lock_request_ms <= commit_ms:
            start = lock_request_ms if start is None else max(start, lock_request_ms)
        if start is None:
            continue
        gaps[output] = (commit_ms - start, start, commit_ms)
    return gaps


CAPTURE_MARKERS = (
    "screencopy",
    "image_copy_capture",
    "export_dmabuf",
    "image_capture_source",
)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("trace")
    ap.add_argument("--expect", choices=["locked", "cancelled"])
    ap.add_argument("--handoff", action="store_true")
    ap.add_argument("--locked-to-commit-ms", type=float, metavar="N")
    ap.add_argument("--first-frame-not-black", action="store_true")
    ap.add_argument("--outputs", type=int, default=0)
    ap.add_argument("--reveal", action="store_true")
    ap.add_argument("--no-reveal", action="store_true")
    ap.add_argument("--no-layer", action="store_true")
    ap.add_argument("--no-capture-only", action="store_true")
    args = ap.parse_args()

    lock_requested = False
    locked_seen = False
    locked_line = None
    unlock_line = None
    lock_surfaces = {}      # ext_session_lock_surface id -> wl_surface id
    warning_surfaces = {}   # zwlr_layer_surface id -> wl_surface id
    pending_attach = {}     # wl_surface id -> has non-null buffer attached
    first_lock_commit = {}  # wl_surface id -> line index of first buffer commit
    warning_teardowns = []  # line indices of warning surface/layer destroys
    reveal_surfaces = {}    # zwlr_layer_surface id -> wl_surface id
    reveal_requests = []    # line indices of reveal get_layer_surface
    reveal_configured = {}  # zwlr_layer_surface id -> line of first configure
    reveal_first_commit = {}  # wl_surface id -> line of first buffer commit
    reveal_teardowns = []   # line indices of reveal surface/layer destroys
    layer_requests = 0
    capture_binds = []
    # Handoff-gap bookkeeping, all keyed by the wl_output the surface was
    # created on — the one identity the warning surface and the lock surface
    # of a single display share.
    lock_surface_output = {}     # wl_surface id -> wl_output id
    warning_surface_output = {}  # wl_surface id -> wl_output id
    lock_request_ms = None
    locked_ms = None
    first_lock_commit_ms = {}    # wl_output id -> ms of first buffer commit
    warning_commits_ms = {}      # wl_output id -> [ms of every buffer commit]
    # frame.hash records (VIGIL_FRAME_HASH=1), keyed by vigil's OutputId,
    # which is the wl_output's protocol id — the same number as above.
    first_lock_hash = {}         # output -> (hash, width, height)
    warning_hashed = set()       # outputs whose warning committed a frame
    # A frame.hash record is written immediately before its own attach, so
    # the next commit-with-buffer on that output's lock surface is the frame
    # it fingerprints. That is what turns "when did a buffer arrive" into
    # "when did a buffer that is not black arrive" — the number issue #86 is
    # actually about.
    pending_lock_hash = {}       # output -> is this next lock frame black
    first_nonblack_ms = {}       # output -> ms of first non-black lock commit

    with open(args.trace, errors="replace") as fh:
        for lineno, line in enumerate(fh):
            if HASH.match(line):
                record = parse_record(line)
                output, role = record.get("output"), record.get("role")
                if output is None:
                    continue
                if role == "warning":
                    warning_hashed.add(output)
                elif role == "lock":
                    digest = record.get("hash")
                    width, height = record.get("width", ""), record.get("height", "")
                    first_lock_hash.setdefault(output, (digest, width, height))
                    if width.isdigit() and height.isdigit():
                        black = black_hash(int(width) * int(height) * 4)
                        pending_lock_hash[output] = digest == black
                continue
            m = SEND.match(line)
            if m:
                ts = float(m.group(1))
                iface, oid, req, req_args = m.group(2), m.group(3), m.group(4), m.group(5)
                if iface == "wl_registry" and req == "bind":
                    lowered = req_args.lower()
                    if any(marker in lowered for marker in CAPTURE_MARKERS):
                        capture_binds.append((lineno, line.strip()))
                elif iface == "ext_session_lock_manager_v1" and req == "lock":
                    lock_requested = True
                    if lock_request_ms is None:
                        lock_request_ms = ts
                elif iface == "ext_session_lock_v1" and req == "get_lock_surface":
                    ids = re.findall(r"(?:ext_session_lock_surface_v1|wl_surface)[#@](\d+)", req_args)
                    surface = re.search(r"wl_surface[#@](\d+)", req_args)
                    new_id = re.search(r"ext_session_lock_surface_v1[#@](\d+)", req_args)
                    output = re.search(r"wl_output[#@](\d+)", req_args)
                    if surface:
                        lock_surfaces[new_id.group(1) if new_id else f"line{lineno}"] = surface.group(1)
                        if output:
                            lock_surface_output[surface.group(1)] = output.group(1)
                elif iface == "zwlr_layer_shell_v1" and req == "get_layer_surface":
                    layer_requests += 1
                    surface = re.search(r"wl_surface[#@](\d+)", req_args)
                    new_id = re.search(r"zwlr_layer_surface_v1[#@](\d+)", req_args)
                    output = re.search(r"wl_output[#@](\d+)", req_args)
                    key = new_id.group(1) if new_id else f"line{lineno}"
                    if surface and "vigil-reveal" in req_args:
                        reveal_surfaces[key] = surface.group(1)
                        reveal_requests.append(lineno)
                    elif surface:
                        warning_surfaces[key] = surface.group(1)
                        if output:
                            warning_surface_output[surface.group(1)] = output.group(1)
                elif iface == "ext_session_lock_v1" and req == "unlock_and_destroy":
                    unlock_line = lineno
                elif iface == "wl_surface" and req == "attach":
                    pending_attach[oid] = ("wl_buffer#" in req_args or "wl_buffer@" in req_args) or re.match(r"\s*\d+", req_args or "")
                elif iface == "wl_surface" and req == "commit":
                    attached = pending_attach.pop(oid, False)
                    if attached and oid in lock_surfaces.values():
                        first_lock_commit.setdefault(oid, lineno)
                    output = lock_surface_output.get(oid)
                    if attached and output is not None:
                        first_lock_commit_ms.setdefault(output, ts)
                        if pending_lock_hash.pop(output, None) is False:
                            first_nonblack_ms.setdefault(output, ts)
                    if attached and oid in warning_surface_output:
                        warning_commits_ms.setdefault(warning_surface_output[oid], []).append(ts)
                    if attached and oid in reveal_surfaces.values():
                        reveal_first_commit.setdefault(oid, lineno)
                elif req == "destroy":
                    if iface == "zwlr_layer_surface_v1" and oid in warning_surfaces:
                        warning_teardowns.append(lineno)
                    elif iface == "wl_surface" and oid in warning_surfaces.values():
                        warning_teardowns.append(lineno)
                    elif iface == "zwlr_layer_surface_v1" and oid in reveal_surfaces:
                        reveal_teardowns.append(lineno)
                    elif iface == "wl_surface" and oid in reveal_surfaces.values():
                        reveal_teardowns.append(lineno)
                continue
            m = EVENT.match(line)
            if m and m.group(2) == "ext_session_lock_v1" and m.group(4) == "locked":
                locked_seen = True
                locked_line = lineno
                if locked_ms is None:
                    locked_ms = float(m.group(1))
            elif m and m.group(2) == "zwlr_layer_surface_v1" and m.group(4) == "configure":
                if m.group(3) in reveal_surfaces:
                    reveal_configured.setdefault(m.group(3), lineno)

    gaps = handoff_gaps(lock_request_ms, first_lock_commit_ms, warning_commits_ms)
    uncovered = locked_to_commit(
        locked_ms, first_lock_commit_ms, set(warning_surface_output.values())
    )
    failures = []
    if capture_binds:
        for lineno, line in capture_binds:
            failures.append(f"capture global bound at line {lineno}: {line}")
    if not args.no_capture_only:
        if args.expect == "locked" and not locked_seen:
            failures.append("expected ext_session_lock_v1.locked event; none seen")
        if args.expect == "cancelled":
            if lock_requested:
                failures.append("cancelled run must never request ext_session_lock_manager_v1.lock")
            if locked_seen:
                failures.append("cancelled run saw a locked event")
        if args.outputs and len(lock_surfaces) < args.outputs:
            failures.append(f"expected >= {args.outputs} lock surfaces, saw {len(lock_surfaces)}")
        if args.handoff:
            if not warning_surfaces:
                failures.append("--handoff: no warning surfaces found in trace")
            elif not first_lock_commit:
                failures.append("--handoff: no lock-surface buffer commits found")
            elif warning_teardowns:
                last_commit = max(first_lock_commit.values())
                early = [t for t in warning_teardowns if t < last_commit]
                if early:
                    failures.append(
                        f"--handoff: warning surface destroyed at line {early[0]} before "
                        f"every lock surface had committed (last first-commit at {last_commit})"
                    )

        if args.locked_to_commit_ms is not None:
            if not uncovered:
                failures.append(
                    "--locked-to-commit-ms: no output went through the handoff "
                    "(needs a locked event and a warning surface that also got "
                    "a lock surface)"
                )
            for output, ms in sorted(uncovered.items()):
                if ms > args.locked_to_commit_ms:
                    failures.append(
                        f"--locked-to-commit-ms: output {output} was uncovered for "
                        f"{ms:.1f} ms after locked (bound {args.locked_to_commit_ms:.1f} ms)"
                    )
        if args.first_frame_not_black:
            if not first_lock_hash:
                failures.append(
                    "--first-frame-not-black: no frame.hash records; run the "
                    "locker with VIGIL_FRAME_HASH=1"
                )
            for output in sorted(warning_hashed):
                frame = first_lock_hash.get(output)
                if frame is None:
                    failures.append(
                        f"--first-frame-not-black: output {output} painted a warning "
                        "frame but never a lock frame"
                    )
                    continue
                digest, width, height = frame
                if not (width or "").isdigit() or not (height or "").isdigit():
                    failures.append(
                        f"--first-frame-not-black: output {output} frame.hash record "
                        f"has no usable size ({width}x{height})"
                    )
                    continue
                black = black_hash(int(width) * int(height) * 4)
                if digest == black:
                    failures.append(
                        f"--first-frame-not-black: output {output} committed an "
                        f"all-black {width}x{height} first lock frame ({digest}) after "
                        "its warning had painted — the warn→lock cut flashes (#86)"
                    )
        if args.no_layer and layer_requests:
            failures.append(f"--no-layer: {layer_requests} layer surface(s) requested")
        if args.reveal:
            if not reveal_surfaces:
                failures.append("--reveal: no vigil-reveal layer surface in trace")
            elif locked_line is None:
                failures.append("--reveal: no locked event")
            elif unlock_line is None:
                failures.append("--reveal: no unlock_and_destroy")
            else:
                if min(reveal_requests) < locked_line:
                    failures.append("--reveal: reveal surface requested before locked")
                if max(reveal_requests) > unlock_line:
                    failures.append("--reveal: reveal surface requested after unlock_and_destroy")
                for layer_id, configured_at in reveal_configured.items():
                    surface = reveal_surfaces[layer_id]
                    committed_at = reveal_first_commit.get(surface)
                    if configured_at < unlock_line and (committed_at is None or committed_at > unlock_line):
                        failures.append(
                            f"--reveal: reveal surface {surface} configured at line {configured_at} "
                            f"but not committed before unlock_and_destroy at {unlock_line}"
                        )
                early = [t for t in reveal_teardowns if t < unlock_line]
                if early:
                    failures.append(f"--reveal: reveal surface destroyed at line {early[0]} before unlock")
        if args.no_reveal and reveal_surfaces:
            failures.append(
                f"--no-reveal: {len(reveal_surfaces)} vigil-reveal surface(s) created; "
                "instant unlock must not create a reveal overlay"
            )

    # Printed on every run that measured one, pass or fail: a gate nobody
    # can read the number behind is a gate nobody retunes. `locked` is here
    # because the compositor stops rendering normal surfaces at that event,
    # not at the request, so it bounds the part of the gap that is actually
    # black on a strict compositor.
    for output, ms in sorted(uncovered.items()):
        print(f"check_trace: black window output {output}: {ms:.1f}ms (locked -> first lock commit)")
    for output, (gap, start, commit) in sorted(gaps.items()):
        since_locked = "" if locked_ms is None else f" since_locked={commit - locked_ms:.1f}ms"
        # The gap above ends at the first buffer; this one ends at the first
        # buffer that is not black, which is what the eye sees. They are the
        # same number exactly when the fix is working, and only a hashed run
        # can tell them apart.
        nonblack = first_nonblack_ms.get(output)
        to_content = "" if nonblack is None else f" to_content={nonblack - start:.1f}ms"
        print(
            f"check_trace: handoff gap output {output}: {gap:.1f}ms "
            f"(start {start:.3f} -> first lock commit {commit:.3f}){since_locked}{to_content}"
        )
    for output, (digest, width, height) in sorted(first_lock_hash.items()):
        warned = "after-warning" if output in warning_hashed else "no-warning"
        print(
            f"check_trace: first lock frame output {output}: {width}x{height} "
            f"hash={digest} ({warned})"
        )

    if failures:
        for failure in failures:
            print(f"check_trace: {failure}", file=sys.stderr)
        sys.exit(1)
    counts = (
        f"lock_surfaces={len(lock_surfaces)} warning_surfaces={len(warning_surfaces)} "
        f"reveal_surfaces={len(reveal_surfaces)} locked={locked_seen} "
        f"lock_requested={lock_requested} unlocked={unlock_line is not None}"
    )
    print(f"check_trace: ok ({counts})")


if __name__ == "__main__":
    main()
