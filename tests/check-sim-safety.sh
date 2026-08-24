#!/bin/sh
set -eu

# The simulator is safe by construction only while none of the host-effect
# crates enter its dependency closure. Keep this as a dependency-tree gate,
# not a convention in the simulator source.
#
# ALLOWLIST, not a denylist. A denylist only rejects the breaches someone
# thought of: the previous fixed list of seven vigil-* crates passed with
# pam-client, vigil-auth (greetd_ipc) and vigil-login (zbus/logind) linked
# in — all named explicitly by ADR 0004 as things the simulator must not
# reach. Anything not enumerated below is a violation until a human adds
# it here on purpose.
allowed='
vigil-sim
vigil-config
vigil-core
vigil-flow
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

# Host-effect crates from outside the workspace. The allowlist above cannot
# catch these, since they do not carry the vigil- prefix.
for forbidden in pam-client pam-sys greetd_ipc zbus libseat smithay; do
    if printf '%s\n' "$tree" | grep -Eq "^${forbidden} v"; then
        printf 'vigil-sim safety violation: dependency on %s\n' "$forbidden" >&2
        exit 1
    fi
done

printf 'vigil-sim dependency boundary is safe\n'
