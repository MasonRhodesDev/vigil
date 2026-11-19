#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
today=${VIGIL_TODAY:-$(date -u +%Y-%m-%d)}
horizon_days=183

awk -v today="$today" -v horizon="$horizon_days" '
    function days(date,    p, y, m) {
        split(date, p, "-")
        y = p[1] + 0
        m = p[2] + 0
        if (m <= 2) { y--; m += 12 }
        return 365 * y + int(y / 4) - int(y / 100) + int(y / 400) + int((153 * (m - 3) + 2) / 5) + p[3]
    }
    /^\[advisories\]/ { section = 1; next }
    /^\[/ { section = 0 }
    section && /(RUSTSEC|GHSA)-/ {
        id = $0
        sub(/.*id = "/, "", id)
        sub(/".*/, "", id)
        if (!match($0, /reason = "expires [0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]: [^"]/)) {
            printf "%s: an ignore needs reason = \"expires YYYY-MM-DD: why\"\n", id > "/dev/stderr"
            bad = 1
            next
        }
        expires = substr($0, RSTART + 18, 10)
        left = days(expires) - days(today)
        if (left < 0) {
            printf "%s: ignore expired on %s; fix the dependency or re-justify with a new date\n", id, expires > "/dev/stderr"
            bad = 1
        } else if (left > horizon) {
            printf "%s: expiry %s is more than %d days out\n", id, expires, horizon > "/dev/stderr"
            bad = 1
        } else {
            count++
        }
    }
    END {
        if (bad) exit 1
        printf "advisory ignores carry live expiry dates (%d checked, as of %s)\n", count, today
    }
' "$root/deny.toml"
