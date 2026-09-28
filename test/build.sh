#!/bin/sh
# Build rustkube's test image context for the commit checked out (#96).
#
#   test/build.sh [target]        default x86_64-unknown-linux-musl
#
# Per stormcentral docs/test-standard.md this runs first, in the checkout on
# the build box, with cargo: it builds the static test binary and stages it
# in test/.stage/, and test/Containerfile (context: the repo root) packages
# it FROM scratch. One image serves all three suites (`/test <suite>`).
# With STAGE_ONLY=1 it stops after staging; otherwise, run by hand, it also
# runs `podman build` and tags rustkube-test.
set -eu
target=${1:-x86_64-unknown-linux-musl}
root=$(cd "$(dirname "$0")/.." && pwd)
commit=$(git -C "$root" rev-parse HEAD 2>/dev/null || echo unknown)

cargo build --release --locked --target "$target" -p rustkube-test --manifest-path "$root/Cargo.toml"
tdir=$(cargo metadata --format-version 1 --no-deps --manifest-path "$root/Cargo.toml" |
    sed 's/.*"target_directory":"\([^"]*\)".*/\1/')
stage="$root/test/.stage"
rm -rf "$stage"
mkdir -p "$stage"
cp "$tdir/$target/release/rustkube-test" "$stage/"

if [ "${STAGE_ONLY:-0}" = 1 ] || [ -n "${CARGO_TARGET_DIR:-}" ]; then
    # stormcentral's runner (which sets CARGO_TARGET_DIR) does its own podman build.
    echo "$stage"
    exit 0
fi
podman build -f "$root/test/Containerfile" --build-arg COMMIT="$commit" -t rustkube-test "$root"
