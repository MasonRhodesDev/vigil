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
  --no-capture-only    only run the capture-bind check

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
    ap.add_argument("--no-capture-only", action="store_true")
    args = ap.parse_args()

    lock_requested = False
    locked_seen = False
    lock_surfaces = {}      # ext_session_lock_surface id -> wl_surface id
    warning_surfaces = {}   # zwlr_layer_surface id -> wl_surface id
    pending_attach = {}     # wl_surface id -> has non-null buffer attached
    first_lock_commit = {}  # wl_surface id -> line index of first buffer commit
    warning_teardowns = []  # line indices of warning surface/layer destroys
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
                    surface = re.search(r"wl_surface[#@](\d+)", req_args)
                    new_id = re.search(r"zwlr_layer_surface_v1[#@](\d+)", req_args)
                    if surface:
                        warning_surfaces[new_id.group(1) if new_id else f"line{lineno}"] = surface.group(1)
                elif iface == "wl_surface" and req == "attach":
                    pending_attach[oid] = ("wl_buffer#" in req_args or "wl_buffer@" in req_args) or re.match(r"\s*\d+", req_args or "")
                elif iface == "wl_surface" and req == "commit":
                    if pending_attach.pop(oid, False) and oid in lock_surfaces.values():
                        first_lock_commit.setdefault(oid, lineno)
                elif req == "destroy":
                    if iface == "zwlr_layer_surface_v1" and oid in warning_surfaces:
                        warning_teardowns.append(lineno)
                    elif iface == "wl_surface" and oid in warning_surfaces.values():
                        warning_teardowns.append(lineno)
                continue
            m = EVENT.match(line)
            if m and m.group(1) == "ext_session_lock_v1" and m.group(3) == "locked":
                locked_seen = True

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

    if failures:
        for failure in failures:
            print(f"check_trace: {failure}", file=sys.stderr)
        sys.exit(1)
    counts = (
        f"lock_surfaces={len(lock_surfaces)} warning_surfaces={len(warning_surfaces)} "
        f"locked={locked_seen} lock_requested={lock_requested}"
    )
    print(f"check_trace: ok ({counts})")


if __name__ == "__main__":
    main()
