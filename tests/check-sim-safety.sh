#!/bin/sh
set -eu

allowed='
vigil-sim
vigil-config
vigil-core
vigil-flow
vigil-log
vigil-secret
vigil-theme
vigil-ui
vigil-warning
'

tree=$(cargo tree -p vigil-sim --prefix none)
violations=$(
    printf '%s\n' "$tree" \
        | sed -n 's/^\(vigil-[a-z-]*\) v.*/\1/p' \
        | sort -u \
        | while read -r crate; do
            printf '%s\n' "$allowed" | grep -qx "$crate" || printf '%s\n' "$crate"
        done
)

if [ -n "$violations" ]; then
    printf 'vigil-sim safety violation: dependency on %s\n' $violations >&2
    printf 'If this is deliberate, add it to the allowlist in %s.\n' "$0" >&2
    exit 1
fi

for forbidden in pam-client pam-sys greetd_ipc zbus libseat smithay; do
    if printf '%s\n' "$tree" | grep -Eq "^${forbidden} v"; then
        printf 'vigil-sim safety violation: dependency on %s\n' "$forbidden" >&2
        exit 1
    fi
done

printf 'vigil-sim dependency boundary is safe\n'
