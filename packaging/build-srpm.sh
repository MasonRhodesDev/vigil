#!/bin/bash
# Build the SRPM (source tarball from a git tag + vendored cargo deps) and
# optionally submit it to COPR.
#
# Release flow (Fedora + Arch from the same tag):
#   1. Bump Cargo.toml [workspace.package] version + spec Version
#      (+ %changelog) + PKGBUILD pkgver — one commit.
#   2. git tag vX.Y.Z && git push --tags
#   CI does the rest: the release workflow builds and publishes the Arch
#   package, and COPR rebuilds the SRPM off its GitHub webhook via
#   .copr/Makefile (which runs this script with --head).
#
# This script stays fully usable locally:
#   --head builds from HEAD instead of the tag (testing only — never
#   submit a --head build); --copr does a manual COPR submit.
set -euo pipefail

NAME="vigil"
CRATE="vigil"

REPO="$(cd "$(dirname "$0")/.." && pwd)"
SPEC="$REPO/packaging/$NAME.spec"
SOURCES="${HOME}/rpmbuild/SOURCES"
COPR_PROJECT="${COPR_PROJECT:-$NAME}"

VER=$(sed -n 's/^Version:[[:space:]]*//p' "$SPEC")
CARGO_VER=$(sed -n 's/^version = "\(.*\)"/\1/p' "$REPO/Cargo.toml" | head -1)
PKGBUILD_VER=$(sed -n 's/^pkgver=//p' "$REPO/packaging/PKGBUILD")
# Cargo.lock's own entry for this package (guards a stale lock).
LOCK_VER=$(awk '/^name = "'"$CRATE"'"$/{getline; gsub(/version = "|"/,""); print; exit}' "$REPO/Cargo.lock")
mismatch=""
[ "$CARGO_VER" = "$VER" ] || mismatch="$mismatch\n  Cargo.toml=$CARGO_VER"
[ "$PKGBUILD_VER" = "$VER" ] || mismatch="$mismatch\n  PKGBUILD pkgver=$PKGBUILD_VER"
[ "$LOCK_VER" = "$VER" ] || mismatch="$mismatch\n  Cargo.lock=$LOCK_VER"
if [ -n "$mismatch" ]; then
    echo "ERROR: version mismatch (spec Version=$VER):$(printf "$mismatch")" >&2
    echo "Bump spec, Cargo.toml, PKGBUILD pkgver, and Cargo.lock together." >&2
    exit 1
fi

REF="v$VER"
if [ "${1:-}" = "--head" ]; then
    REF="HEAD"
    echo "WARNING: building from HEAD (testing only)"
    shift
elif ! git -C "$REPO" rev-parse -q --verify "refs/tags/$REF" >/dev/null; then
    echo "ERROR: tag $REF not found — tag the release first (or use --head to test)" >&2
    exit 1
fi

mkdir -p "$SOURCES"
echo "==> source tarball from $REF"
git -C "$REPO" archive --format=tar.gz --prefix="$NAME-$VER/" \
    -o "$SOURCES/$NAME-$VER.tar.gz" "$REF"

echo "==> vendoring cargo dependencies (crates.io + pinned git sources)"
VENDOR_DIR=$(mktemp -d)
trap 'rm -rf "$VENDOR_DIR"' EXIT
git -C "$REPO" archive --prefix=src/ "$REF" | tar -x -C "$VENDOR_DIR"
(cd "$VENDOR_DIR/src" && cargo vendor --locked > "$VENDOR_DIR/vendor-config.toml")
# cargo-rpm-macros supplies the crates.io replacement. Preserve cargo
# vendor's exact git source IDs separately so tagged/revision dependencies
# resolve from the same offline vendor directory without a hand-maintained
# list drifting every time the workspace gains a shared crate.
awk '/^\[/{p = /^\[source\."git\+/} p' \
    "$VENDOR_DIR/vendor-config.toml" > "$VENDOR_DIR/vendor-git-sources.toml"
test -s "$VENDOR_DIR/vendor-git-sources.toml" || {
    echo "ERROR: cargo vendor emitted no git source replacements" >&2
    exit 1
}
# Prove the archived workspace resolves without network before producing an
# SRPM. This catches a missing tagged/revision replacement at source-bundle
# construction time instead of minutes later inside rpmbuild.
mkdir -p "$VENDOR_DIR/src/.cargo" "$VENDOR_DIR/cargo-home"
cp "$VENDOR_DIR/vendor-config.toml" "$VENDOR_DIR/src/.cargo/config.toml"
(cd "$VENDOR_DIR/src" && CARGO_HOME="$VENDOR_DIR/cargo-home" \
    cargo metadata --offline --locked --no-deps --format-version 1 >/dev/null)
tar -cJf "$SOURCES/$NAME-$VER-vendor.tar.xz" -C "$VENDOR_DIR/src" vendor \
    -C "$VENDOR_DIR" vendor-git-sources.toml

echo "==> building SRPM"
SRPM=$(rpmbuild -bs "$SPEC" | sed -n 's/^Wrote: //p')
echo "    $SRPM"
# rpmlint is gated in CI against the SRPM and binary RPMs together. Linting
# the SRPM alone treats binary-only filters as unused errors.

if [ "${1:-}" = "--copr" ]; then
    echo "==> submitting to COPR project $COPR_PROJECT"
    if ! copr-cli build "$COPR_PROJECT" "$SRPM"; then
        echo "ERROR: copr build failed. If this was a 401, the API token has" >&2
        echo "expired (~180 days) — renew at https://copr.fedorainfracloud.org/api/" >&2
        exit 1
    fi
fi
