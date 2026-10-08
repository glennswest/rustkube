#!/usr/bin/env bash
#
# Build and check the release artifacts: static musl binaries, their tarballs
# and `FROM scratch` images.
#
# **Runs as a build job, not on a workstation and never as root** (#156):
#
#   git push && sc-build deploy/build-release.sh
#   sc-build 'NO_IMAGES=1 deploy/build-release.sh'      # binaries and tarballs only
#   sc-build 'TARGETS=x86_64-unknown-linux-musl,aarch64-unknown-linux-musl deploy/build-release.sh'
#
# The target is Linux and half this codebase is behind `cfg(target_os =
# "linux")`; a build produced anywhere else is not the thing that runs.
#
# sc-build gives the job a private volume — checkout, target dir, HOME and
# TMPDIR — and deletes it when the job ends, pass or fail. So everything here
# stays on that volume: cargo's own target dir (or the CARGO_TARGET_DIR the
# job sets), and OUT under $TMPDIR. **Nothing this script writes outlives the
# job.** It proves a commit builds into static binaries, tarballs and images,
# and prints their sizes and sha256s. Delivery is the component golden,
# `stormcentral component build rustkube` (docs/releasing.md); there is no
# persistent output directory, and none must be made with root or a mount.
#
# A non-native target is built with `cross` (containerised toolchain) when it
# is installed. Without it, aarch64-unknown-linux-musl is built with clang
# (`--target=aarch64-unknown-linux-musl -ffreestanding`: `ring`'s C needs only
# freestanding headers, and the GNU cross gcc ships no libc headers) and
# `llvm-ar`, linked by `aarch64-linux-gnu-gcc` (the build VMs have all three
# and the Rust target, #68); Rust's own self-contained musl makes the binary
# static either way. An aarch64 binary
# is then run once under `qemu-aarch64-static`, when there is one, so a
# binary that does not start is caught here.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

VERSION="$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)"
COMPONENTS=(kube-apiserver kube-controller-manager kube-scheduler)
NATIVE="$(uname -m)-unknown-linux-musl"
TARGETS="${TARGETS:-$NATIVE}"
# On the job's private volume (#156): TMPDIR there, the checkout's tmp/ when
# run by hand. CARGO_TARGET_DIR is cargo's own default unless the job sets it.
OUT="${OUT:-${TMPDIR:-$REPO_ROOT/tmp}/rustkube-release/v$VERSION}"
TARGET_DIR="${CARGO_TARGET_DIR:-$REPO_ROOT/target}"

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
    pkgs=()
    for c in "${COMPONENTS[@]}"; do pkgs+=(-p "$c"); done

    if [ "$target" = "$NATIVE" ]; then
        cargo build --release --target "$target" "${pkgs[@]}" 2>&1 | tail -3
    else
        if command -v cross >/dev/null; then
            cross build --release --target "$target" "${pkgs[@]}" 2>&1 | tail -3
        elif [ "$target" = aarch64-unknown-linux-musl ] && command -v aarch64-linux-gnu-gcc >/dev/null \
                && command -v clang >/dev/null && command -v llvm-ar >/dev/null; then
            CC_aarch64_unknown_linux_musl=clang \
            CFLAGS_aarch64_unknown_linux_musl="--target=aarch64-unknown-linux-musl -ffreestanding" \
            AR_aarch64_unknown_linux_musl=llvm-ar \
            CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=aarch64-linux-gnu-gcc \
                cargo build --release --target "$target" "${pkgs[@]}" 2>&1 | grep -E '^error|fatal error|Finished' -A6 | tail -20
        else
            echo "no cross and no cross compiler for $target — see the header" >&2
            exit 1
        fi
    fi

    bindir="$TARGET_DIR/$target/release"

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

        if [ "$arch" = aarch64 ] && command -v qemu-aarch64-static >/dev/null; then
            qemu-aarch64-static "$bin" --help >/dev/null 2>&1 \
                || { echo "REFUSING: $c (aarch64) does not start under qemu" >&2; exit 1; }
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
echo "=== $OUT (deleted with the job) ==="
ls -lh "$OUT" | tail -n +2 | awk '{printf "  %-52s %s\n", $9, $5}'
echo
echo "=== sha256 ==="
(cd "$OUT" && sha256sum -- *)
