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
  --outputs N          at least N lock surfaces were created
  --reveal             the post-unlock reveal overlay (issue #52) was
                       created after `locked`, unlock_and_destroy came after
                       every reveal surface request (and after its first
                       buffer commit whenever the compositor configured it
                       before the unlock), and reveal teardown only after
                       unlock_and_destroy
  --no-layer           no layer surface at all (`--immediate` path)
  --no-capture-only    only run the capture-bind check

Layer surfaces are classified by the namespace passed to
get_layer_surface: "vigil-warning" (pre-lock overlay, --handoff) versus
"vigil-reveal" (post-unlock overlay, --reveal).

The capture check always runs: binding any screencopy/image-capture/
export-dmabuf global is forbidden (capture-free warning, ADR).
"""

import argparse
import re
import sys

SEND = re.compile(r"^\[[^\]]+\](?:\[rs\])?\s*(?:\[discarded\])?\s*-> ([a-z0-9_]+)[#@](\d+)\.([a-z0-9_]+)\((.*)\)")
EVENT = re.compile(r"^\[[^\]]+\](?:\[rs\])?\s*(?:<- )?([a-z0-9_]+)[#@](\d+)\.([a-z0-9_]+)[,(]")

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
    ap.add_argument("--outputs", type=int, default=0)
    ap.add_argument("--reveal", action="store_true")
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

    with open(args.trace, errors="replace") as fh:
        for lineno, line in enumerate(fh):
            m = SEND.match(line)
            if m:
                iface, oid, req, req_args = m.group(1), m.group(2), m.group(3), m.group(4)
                if iface == "wl_registry" and req == "bind":
                    lowered = req_args.lower()
                    if any(marker in lowered for marker in CAPTURE_MARKERS):
                        capture_binds.append((lineno, line.strip()))
                elif iface == "ext_session_lock_manager_v1" and req == "lock":
                    lock_requested = True
                elif iface == "ext_session_lock_v1" and req == "get_lock_surface":
                    ids = re.findall(r"(?:ext_session_lock_surface_v1|wl_surface)[#@](\d+)", req_args)
                    surface = re.search(r"wl_surface[#@](\d+)", req_args)
                    new_id = re.search(r"ext_session_lock_surface_v1[#@](\d+)", req_args)
                    if surface:
                        lock_surfaces[new_id.group(1) if new_id else f"line{lineno}"] = surface.group(1)
                elif iface == "zwlr_layer_shell_v1" and req == "get_layer_surface":
                    layer_requests += 1
                    surface = re.search(r"wl_surface[#@](\d+)", req_args)
                    new_id = re.search(r"zwlr_layer_surface_v1[#@](\d+)", req_args)
                    key = new_id.group(1) if new_id else f"line{lineno}"
                    if surface and "vigil-reveal" in req_args:
                        reveal_surfaces[key] = surface.group(1)
                        reveal_requests.append(lineno)
                    elif surface:
                        warning_surfaces[key] = surface.group(1)
                elif iface == "ext_session_lock_v1" and req == "unlock_and_destroy":
                    unlock_line = lineno
                elif iface == "wl_surface" and req == "attach":
                    pending_attach[oid] = ("wl_buffer#" in req_args or "wl_buffer@" in req_args) or re.match(r"\s*\d+", req_args or "")
                elif iface == "wl_surface" and req == "commit":
                    attached = pending_attach.pop(oid, False)
                    if attached and oid in lock_surfaces.values():
                        first_lock_commit.setdefault(oid, lineno)
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
            if m and m.group(1) == "ext_session_lock_v1" and m.group(3) == "locked":
                locked_seen = True
                locked_line = lineno
            elif m and m.group(1) == "zwlr_layer_surface_v1" and m.group(3) == "configure":
                if m.group(2) in reveal_surfaces:
                    reveal_configured.setdefault(m.group(2), lineno)

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
