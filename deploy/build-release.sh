#!/usr/bin/env bash
#
# Build the release artifacts: static musl binaries and `FROM scratch` images.
#
# **Runs on dev (`root@dev.g8.lo`), not on a workstation.** The target is Linux
# and half this codebase is behind `cfg(target_os = "linux")`; a build produced
# anywhere else is not the thing that runs. See ../CLAUDE.md.
#
# Everything lands in $OUT, which defaults to /build (the spinning 2 TB drive —
# nothing that persists goes on the SSD root). Point OUT at a golden's NVMe
# mount to have the artifacts written straight there:
#
#   OUT=/mnt/goldens/rustkube ./deploy/build-release.sh
#
# Usage:
#   ./deploy/build-release.sh                  # native arch, all components
#   TARGETS=x86_64-unknown-linux-musl,aarch64-unknown-linux-musl ./deploy/build-release.sh
#   NO_IMAGES=1 ./deploy/build-release.sh      # binaries and tarballs only
#
# A non-native target is built with `cross` (containerised toolchain), which is
# the proven path here; dev has no aarch64 musl cross-compiler installed and
# `ring` needs a C compiler for the target.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

VERSION="$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)"
COMPONENTS=(kube-apiserver kube-controller-manager kube-scheduler)
NATIVE="$(uname -m)-unknown-linux-musl"
TARGETS="${TARGETS:-$NATIVE}"
OUT="${OUT:-/build/rustkube-release/v$VERSION}"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-/build/cargo/rustkube}"

mkdir -p "$OUT"
echo "rustkube v$VERSION -> $OUT"
echo "targets: $TARGETS"
echo

for target in ${TARGETS//,/ }; do
    arch="${target%%-*}"
    echo "=== $target ==="

    # `cross` for anything that is not this machine's own architecture: it
    # carries the target's C toolchain, which `ring` needs and which dev does
    # not have installed for aarch64.
    if [ "$target" = "$NATIVE" ]; then
        cargo build --release --target "$target" \
            "${COMPONENTS[@]/#/-p }" 2>&1 | tail -3
    else
        command -v cross >/dev/null || {
            echo "cross is not installed and $target is not native — see the header" >&2
            exit 1
        }
        cross build --release --target "$target" \
            "${COMPONENTS[@]/#/-p }" 2>&1 | tail -3
    fi

    bindir="$CARGO_TARGET_DIR/$target/release"

    for c in "${COMPONENTS[@]}"; do
        bin="$bindir/$c"
        [ -x "$bin" ] || { echo "missing $bin" >&2; exit 1; }

        # A build that silently went dynamic is the failure this check exists
        # for: it works on the build host, and the golden it lands in has no
        # loader for it. Cheap to assert, expensive to discover on a node.
        if ! file "$bin" | grep -q "static-pie linked\|statically linked"; then
            echo "REFUSING: $c is not statically linked:" >&2
            file "$bin" >&2
            exit 1
        fi

        comp="rustkube-${c#kube-}"
        tar -C "$bindir" -czf \
            "$OUT/$comp-v$VERSION-$arch-linux-musl.tar.gz" "$c"

        if [ -z "${NO_IMAGES:-}" ]; then
            # podman, not docker (project rule), and OCI is a build-time input
            # only: what ships is the tarball, which stormcos preloads into the
            # node image store.
            cp "$bin" "./$c"
            podman build --quiet --platform "linux/${arch/x86_64/amd64}" \
                -f deploy/images/Dockerfile \
                --build-arg "COMPONENT=$c" -t "$comp:v$VERSION-$arch" . >/dev/null
            rm -f "./$c"
            podman save "$comp:v$VERSION-$arch" \
                | gzip > "$OUT/$comp-v$VERSION-$arch.docker.tar.gz"
        fi
        printf '  %-28s %s\n' "$c" "$(du -h "$bin" | cut -f1)"
    done
done

echo
echo "=== $OUT ==="
ls -lh "$OUT" | tail -n +2 | awk '{printf "  %-52s %s\n", $9, $5}'
