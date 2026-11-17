#!/usr/bin/env python3

import argparse
import re
import sys

SEND = re.compile(r"^\[\s*([0-9.]+)\](?:\[rs\])?\s*(?:\[discarded\])?\s*-> ([a-z0-9_]+)[#@](\d+)\.([a-z0-9_]+)\((.*)\)")
EVENT = re.compile(r"^\[\s*([0-9.]+)\](?:\[rs\])?\s*(?:<- )?([a-z0-9_]+)[#@](\d+)\.([a-z0-9_]+)[,(]")
HASH = re.compile(r"^event=frame\.hash\s")

FNV_OFFSET = 0xCBF29CE484222325
FNV_PRIME = 0x100000001B3
MASK64 = (1 << 64) - 1


def black_hash(byte_count):
    return f"{(FNV_OFFSET * pow(FNV_PRIME, byte_count, MASK64 + 1)) & MASK64:016x}"


def parse_record(line):
    fields = {}
    for token in line.split():
        key, sep, value = token.partition("=")
        if sep:
            fields[key] = value
    return fields


def dangling_parents(records):
    ids = {r["id"] for r in records if "id" in r}
    dangling = [
        r for r in records
        if r.get("parent", "-") != "-" and r["parent"] not in ids
    ]
    counts = {}
    for record in dangling:
        key = (record.get("span") or record.get("event") or "?", record["parent"])
        counts[key] = counts.get(key, 0) + 1
    summary = "; ".join(
        f"{n}x {name} -> missing parent {parent}"
        for (name, parent), n in sorted(counts.items(), key=lambda kv: -kv[1])
    )
    return dangling, summary

def locked_to_commit(locked_ms, first_lock_commit_ms, handoff_outputs):
    if locked_ms is None:
        return {}
    return {
        output: commit_ms - locked_ms
        for output, commit_ms in first_lock_commit_ms.items()
        if output in handoff_outputs and commit_ms >= locked_ms
    }


def handoff_gaps(lock_request_ms, first_lock_commit_ms, warning_commits_ms):
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
    ap.add_argument("--session-outcome", metavar="OUTCOME")
    ap.add_argument("--no-unlock", action="store_true")
    ap.add_argument("--no-capture-only", action="store_true")
    args = ap.parse_args()

    lock_requested = False
    locked_seen = False
    locked_line = None
    unlock_line = None
    lock_destroy_line = None
    span_records = []
    lock_surfaces = {}
    warning_surfaces = {}
    pending_attach = {}
    first_lock_commit = {}
    warning_teardowns = []
    reveal_surfaces = {}
    reveal_requests = []
    reveal_configured = {}
    reveal_first_commit = {}
    reveal_teardowns = []
    layer_requests = 0
    capture_binds = []
    lock_surface_output = {}
    warning_surface_output = {}
    handoff_outputs = set()
    lock_request_ms = None
    locked_ms = None
    first_lock_commit_ms = {}
    warning_commits_ms = {}
    first_lock_hash = {}
    warning_hashed = set()
    last_lock_commit_ms = {}
    first_nonblack_ms = {}

    with open(args.trace, errors="replace") as fh:
        for lineno, line in enumerate(fh):
            if line.startswith(("span=", "event=")):
                span_records.append(parse_record(line))
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
                    if (
                        width.isdigit()
                        and height.isdigit()
                        and digest != black_hash(int(width) * int(height) * 4)
                        and output in last_lock_commit_ms
                    ):
                        first_nonblack_ms.setdefault(output, last_lock_commit_ms[output])
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
                            handoff_outputs.add(output.group(1))
                elif iface == "ext_session_lock_v1" and req == "unlock_and_destroy":
                    unlock_line = lineno
                elif iface == "ext_session_lock_v1" and req == "destroy":
                    lock_destroy_line = lineno
                elif iface == "wl_surface" and req == "attach":
                    pending_attach[oid] = ("wl_buffer#" in req_args or "wl_buffer@" in req_args) or re.match(r"\s*\d+", req_args or "")
                elif iface == "wl_surface" and req == "commit":
                    attached = pending_attach.pop(oid, False)
                    if attached and oid in lock_surfaces.values():
                        first_lock_commit.setdefault(oid, lineno)
                    output = lock_surface_output.get(oid)
                    if attached and output is not None:
                        first_lock_commit_ms.setdefault(output, ts)
                        last_lock_commit_ms[output] = ts
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
                    if iface == "wl_surface":
                        gone = lock_surface_output.pop(oid, None)
                        warning_surface_output.pop(oid, None)
                        if gone is not None:
                            last_lock_commit_ms.pop(gone, None)
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
    uncovered = locked_to_commit(locked_ms, first_lock_commit_ms, handoff_outputs)
    sessions = [r for r in span_records if r.get("span") == "lock.session"]
    dangling, dangling_summary = dangling_parents(span_records)
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
        if args.session_outcome is not None:
            if len(sessions) != 1:
                died = (
                    " (no root at all: the locker left without closing its trace, #79)"
                    if not sessions
                    else ""
                )
                failures.append(
                    f"--session-outcome: expected exactly one lock.session record, "
                    f"found {len(sessions)}{died}"
                )
            elif sessions[0].get("outcome") != args.session_outcome:
                failures.append(
                    f"--session-outcome: lock.session carries "
                    f"outcome={sessions[0].get('outcome', '<absent>')}, "
                    f"expected {args.session_outcome}"
                )
            if dangling:
                failures.append(
                    f"--session-outcome: {len(dangling)} record(s) name a parent that "
                    f"was never written: {dangling_summary}"
                )
        if args.no_unlock:
            if unlock_line is not None:
                failures.append(
                    f"--no-unlock: unlock_and_destroy sent at line {unlock_line}; "
                    "a signal must not release the session lock"
                )
            if lock_destroy_line is not None:
                failures.append(
                    f"--no-unlock: ext_session_lock_v1.destroy sent at line "
                    f"{lock_destroy_line}; the lock object must be left to the compositor"
                )

    if args.session_outcome is not None:
        outcomes = ",".join(s.get("outcome", "<absent>") for s in sessions) or "<none>"
        print(
            f"check_trace: trace records={len(span_records)} "
            f"loop_iterations={sum(1 for r in span_records if r.get('span') == 'loop.iteration')} "
            f"lock_session={len(sessions)} outcome={outcomes} "
            f"dangling={len(dangling)}"
            + (f" ({dangling_summary})" if dangling else "")
        )

    for output, ms in sorted(uncovered.items()):
        print(f"check_trace: black window output {output}: {ms:.1f}ms (locked -> first lock commit)")
    for output, (gap, start, commit) in sorted(gaps.items()):
        since_locked = "" if locked_ms is None else f" since_locked={commit - locked_ms:.1f}ms"
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
