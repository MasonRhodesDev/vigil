#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"
baseline=tests/unsafe-baseline.txt
fail=0

say() {
    printf '%s\n' "$*" >&2
    fail=1
}

measure() {
    git ls-files '*.rs' | while read -r file; do
        awk -v file="$file" '
            /^[[:space:]]*\/\// { next }
            {
                line = $0
                unsafe += gsub(/(^|[^A-Za-z0-9_])unsafe([^A-Za-z0-9_]|$)/, "&", line)
                allows += gsub(/allow\(unsafe_code\)/, "&", line)
                unjustified += gsub(/clippy::undocumented_unsafe_blocks/, "&", line)
            }
            END {
                if (unsafe + allows + unjustified > 0)
                    printf "%s %d %d %d\n", file, unsafe, allows, unjustified
            }
        ' "$file"
    done
}

totals() {
    awk '{ u += $2; a += $3; j += $4 } END { printf "%d %d %d\n", u, a, j }'
}

measured=$(measure)

if [ "${1:-}" = "--write" ]; then
    printf '%s\n' "$measured" > "$baseline"
    printf 'wrote %s\n' "$baseline"
    exit 0
fi

recorded=$(cat "$baseline")

diff=$(printf '%s\n' "$measured" | awk -v recorded="$recorded" '
    BEGIN {
        n = split(recorded, lines, "\n")
        for (i = 1; i <= n; i++) {
            split(lines[i], f, " ")
            if (f[1] != "") want[f[1]] = f[2] " " f[3] " " f[4]
        }
    }
    {
        have = $2 " " $3 " " $4
        if (!($1 in want))
            printf "%s: unsafe outside the fence (unsafe, allow(unsafe_code), unjustified = %s)\n", $1, have
        else if (want[$1] != have)
            printf "%s: measured %s, baseline says %s\n", $1, have, want[$1]
        seen[$1] = 1
    }
    END {
        for (path in want)
            if (!(path in seen))
                printf "%s: in the baseline but has no unsafe left; remove the line\n", path
    }
')
if [ -n "$diff" ]; then
    printf '%s\n' "$diff" >&2
    say "The unsafe fence is $baseline. New unsafe needs a reviewed baseline change; removed unsafe must lower it."
fi

for attr in unsafe_code unsafe_op_in_unsafe_fn warnings 'clippy::restriction' \
    'clippy::undocumented_unsafe_blocks' 'clippy::multiple_unsafe_ops_per_block'; do
    if git ls-files '*.rs' | xargs rg -n "^\s*#!\[(allow|expect|warn)\([^]]*${attr}" ; then
        say "crate-wide allow of $attr defeats the per-item fence"
    fi
done

for manifest in crates/*/Cargo.toml; do
    if ! awk '/^\[lints\]/ { l = 1; next } /^\[/ { l = 0 } l && /^workspace = true/ { ok = 1 } END { exit !ok }' "$manifest"; then
        say "$manifest must inherit the workspace lints: [lints] workspace = true"
    fi
done
for lint in 'unsafe_code = "deny"' 'unsafe_op_in_unsafe_fn = "deny"' 'undocumented_unsafe_blocks = "deny"'; do
    grep -qx "$lint" Cargo.toml || say "Cargo.toml [workspace.lints] lost: $lint"
done

for dir in crates/*/; do
    crate=${dir%/}
    crate_root=$crate/src/lib.rs
    [ -f "$crate_root" ] || crate_root=$crate/src/main.rs
    if grep -q "^$crate/src/" "$baseline"; then
        grep -qx '#!\[deny(unsafe_code)\]' "$crate_root" ||
            say "$crate_root holds fenced unsafe and must open with #![deny(unsafe_code)]"
    else
        grep -qx '#!\[forbid(unsafe_code)\]' "$crate_root" ||
            say "$crate_root has no unsafe and must open with #![forbid(unsafe_code)]"
    fi
done

base=${VIGIL_FENCE_BASE-origin/main}
if [ -z "$base" ] || ! git rev-parse -q --verify "$base^{commit}" >/dev/null; then
    printf 'unsafe fence: no base ref %s; skipping the shrink-only check\n' "${base:-(empty)}"
elif ! base_recorded=$(git show "$base:$baseline" 2>/dev/null); then
    printf 'unsafe fence: %s has no %s yet; skipping the shrink-only check\n' "$base" "$baseline"
else
    now=$(printf '%s\n' "$recorded" | totals)
    was=$(printf '%s\n' "$base_recorded" | totals)
    set -- $now $was
    if [ "$1" -gt "$4" ] || [ "$2" -gt "$5" ] || [ "$3" -gt "$6" ]; then
        say "the unsafe baseline grew against $base: unsafe $4 -> $1, allow(unsafe_code) $5 -> $2, unjustified $6 -> $3"
    fi
fi

if [ "$fail" -ne 0 ]; then
    exit 1
fi
set -- $(printf '%s\n' "$recorded" | totals)
printf 'unsafe fence holds: %s unsafe, %s allow(unsafe_code), %s unjustified\n' "$1" "$2" "$3"
