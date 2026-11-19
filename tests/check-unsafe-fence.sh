#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"
baseline=tests/unsafe-baseline.txt
columns='unsafe, allow(unsafe_code), unjustified, plaintext exemptions'
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
                exposed += gsub(/clippy::disallowed_methods/, "&", line)
            }
            END {
                if (unsafe + allows + unjustified + exposed > 0)
                    printf "%s %d %d %d %d\n", file, unsafe, allows, unjustified, exposed
            }
        ' "$file"
    done
}

totals() {
    awk '{ u += $2; a += $3; j += $4; e += $5 } END { printf "%d %d %d %d\n", u, a, j, e }'
}

measured=$(measure)

if [ "${1:-}" = "--write" ]; then
    printf '%s\n' "$measured" > "$baseline"
    printf 'wrote %s\n' "$baseline"
    exit 0
fi

recorded=$(cat "$baseline")

diff=$(printf '%s\n' "$measured" | awk -v recorded="$recorded" -v columns="$columns" '
    BEGIN {
        n = split(recorded, lines, "\n")
        for (i = 1; i <= n; i++) {
            split(lines[i], f, " ")
            if (f[1] != "") want[f[1]] = f[2] " " f[3] " " f[4] " " f[5]
        }
    }
    {
        have = $2 " " $3 " " $4 " " $5
        if (!($1 in want))
            printf "%s: outside the fence (%s = %s)\n", $1, columns, have
        else if (want[$1] != have)
            printf "%s: measured %s, baseline says %s (%s)\n", $1, have, want[$1], columns
        seen[$1] = 1
    }
    END {
        for (path in want)
            if (!(path in seen))
                printf "%s: in the baseline but has nothing fenced left; remove the line\n", path
    }
')
if [ -n "$diff" ]; then
    printf '%s\n' "$diff" >&2
    say "The fence is $baseline. New unsafe or plaintext access needs a reviewed baseline change; removing some must lower it."
fi

for attr in unsafe_code unsafe_op_in_unsafe_fn warnings 'clippy::restriction' \
    'clippy::undocumented_unsafe_blocks' 'clippy::multiple_unsafe_ops_per_block' \
    'clippy::disallowed_methods' 'clippy::disallowed_types'; do
    if git ls-files '*.rs' | xargs rg -n "^\s*#!\[(allow|expect|warn)\([^]]*${attr}"; then
        say "a crate-wide allow of $attr defeats the per-item fence"
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
grep -q 'path = "vigil_secret::Secret::expose"' clippy.toml ||
    say "clippy.toml no longer disallows Secret::expose"

for dir in crates/*/; do
    crate=${dir%/}
    crate_root=$crate/src/lib.rs
    [ -f "$crate_root" ] || crate_root=$crate/src/main.rs
    if awk '$2 > 0 || $3 > 0 || $4 > 0' "$baseline" | grep -q "^$crate/src/"; then
        grep -qx '#!\[deny(unsafe_code)\]' "$crate_root" ||
            say "$crate_root holds fenced unsafe and must open with #![deny(unsafe_code)]"
    else
        grep -qx '#!\[forbid(unsafe_code)\]' "$crate_root" ||
            say "$crate_root has no unsafe and must open with #![forbid(unsafe_code)]"
    fi
done

base=${VIGIL_FENCE_BASE-origin/main}
if [ -z "$base" ] || ! git rev-parse -q --verify "$base^{commit}" >/dev/null; then
    printf 'fence: no base ref %s; skipping the shrink-only check\n' "${base:-(empty)}"
elif ! base_recorded=$(git show "$base:$baseline" 2>/dev/null); then
    printf 'fence: %s has no %s yet; skipping the shrink-only check\n' "$base" "$baseline"
else
    set -- $(printf '%s\n' "$recorded" | totals) $(printf '%s\n' "$base_recorded" | totals)
    if [ "$1" -gt "$5" ] || [ "$2" -gt "$6" ] || [ "$3" -gt "$7" ] || [ "$4" -gt "$8" ]; then
        say "the fence grew against $base ($columns): $5 $6 $7 $8 -> $1 $2 $3 $4"
    fi
fi

if [ "$fail" -ne 0 ]; then
    exit 1
fi
set -- $(printf '%s\n' "$recorded" | totals)
printf 'fence holds: %s unsafe, %s allow(unsafe_code), %s unjustified, %s plaintext exemptions\n' "$1" "$2" "$3" "$4"
