#!/bin/sh
set -eu

# The simulator is safe by construction only while none of the host-effect
# crates enter its dependency closure. Keep this as a dependency-tree gate,
# not a convention in the simulator source.
tree=$(cargo tree -p vigil-sim --prefix none)
for forbidden in vigil-pam vigil-session vigil-wayland vigil-input vigil-outputs vigil-present-dumb vigil-present-gl; do
    if printf '%s\n' "$tree" | grep -Eq "^${forbidden} v"; then
        printf 'vigil-sim safety violation: dependency on %s\n' "$forbidden" >&2
        exit 1
    fi
done
printf 'vigil-sim dependency boundary is safe\n'
