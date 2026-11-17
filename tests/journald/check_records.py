#!/usr/bin/env python3

import json
import sys


def fields(message):
    return dict(t.split("=", 1) for t in message.split() if "=" in t)


def main(path, trace_id):
    entries = []
    with open(path, encoding="utf-8") as handle:
        for line in handle:
            line = line.strip()
            if line:
                entries.append(json.loads(line))

    records = []
    for entry in entries:
        message = entry.get("MESSAGE", "")
        if isinstance(message, list):
            message = bytes(message).decode("utf-8", "replace")
        if not message.startswith(("span=", "event=")):
            continue
        parsed = fields(message)
        if parsed.get("trace") == trace_id:
            records.append((int(entry["__MONOTONIC_TIMESTAMP"]), parsed, message, entry))

    failures = []

    def check(ok, label, detail=""):
        print(f"  {'ok  ' if ok else 'FAIL'}: {label}{(' - ' + detail) if detail and not ok else ''}")
        if not ok:
            failures.append(label)

    print(f"journald round trip: {len(records)} records for trace {trace_id} "
          f"({len(entries)} entries in the window)")

    check(bool(records), "records reached journald at all",
          "nothing with this trace id - did the locker adopt TRACEPARENT?")
    if not records:
        return 1

    check(all("\n" not in m for _, _, m, _ in records), "no record was split or merged")

    spans = [f for _, f, _, _ in records if "span" in f]
    required = {"trace", "id", "parent", "seq", "t_us", "dur_us"}
    incomplete = [f for f in spans if not required <= set(f)]
    check(not incomplete, "every span record kept its full field set",
          f"{len(incomplete)} incomplete")

    seqs = [int(f["seq"]) for _, f, _, _ in records]
    check(sorted(seqs) == list(range(len(seqs))),
          "seq is contiguous from zero", f"min={min(seqs)} max={max(seqs)} n={len(seqs)}")

    ordered = sorted(records, key=lambda r: int(r[1]["seq"]))
    inversions = sum(1 for a, b in zip(ordered, ordered[1:]) if a[0] > b[0])
    check(inversions == 0, "__MONOTONIC_TIMESTAMP agrees with seq order",
          f"{inversions} inversions")

    ids = {f["id"] for _, f, _, _ in records if "id" in f}
    dangling = [f for _, f, _, _ in records
                if f.get("parent", "-") != "-" and f["parent"] not in ids]
    check(not dangling, "no record names a parent that never arrived",
          f"{len(dangling)} dangling")

    sessions = [f for _, f, _, _ in records if f.get("span") == "lock.session"]
    check(len(sessions) == 1, "exactly one lock.session record", f"got {len(sessions)}")
    check(bool(sessions) and "outcome" in sessions[0],
          "the session recorded its outcome",
          sessions[0].get("span", "") if sessions else "no session record")

    first = records[0][3]
    for key in ("__MONOTONIC_TIMESTAMP", "_BOOT_ID", "_PID", "_COMM"):
        check(key in first, f"journald stamped {key}")

    suppressed = sum(1 for e in entries if "Suppressed" in str(e.get("MESSAGE", "")))
    check(suppressed == 0, "journald suppressed nothing", f"{suppressed} notices")

    if failures:
        print(f"FAIL: {len(failures)} check(s) failed")
        return 1
    print("journald round trip intact")
    return 0


if __name__ == "__main__":
    if len(sys.argv) != 3:
        print("usage: check_records.py <journal.json> <trace-id>", file=sys.stderr)
        raise SystemExit(2)
    raise SystemExit(main(sys.argv[1], sys.argv[2]))
