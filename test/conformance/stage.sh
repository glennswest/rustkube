#!/usr/bin/env bash
#
# Build the binaries a conformance run needs, once, and publish them for the
# conformance VM — which has no toolchain and takes no build slot.
#
#   sc-build test/conformance/stage.sh
#
# Builds kube-apiserver, kube-controller-manager and kube-scheduler at this
# checkout's commit, and fastetcd-server at fastetcd's main, and copies them to
#
#   /build/assets/conformance/<rustkube sha>/{bin/,fastetcd,MANIFEST}
#
# on the build box. MANIFEST names both commits and every binary's sha256. The
# last line printed is `STAGED <dir>`; the VM runs
#
#   RK_BIN=<dir>/bin RK_FASTETCD=<dir>/fastetcd test/conformance/run.sh <focus>
#
# Debug builds, as the in-slot runs used, so results compare. Idempotent: a
# commit already staged is reported and not rebuilt.
set -euo pipefail
cd "$(dirname "$0")/../.."
mkdir -p "$HOME/tmp" && export TMPDIR="$HOME/tmp"

SHA=$(git rev-parse HEAD)
OUT=/build/assets/conformance/$SHA
if [ -f "$OUT/MANIFEST" ]; then
  echo "already staged: $SHA"
  echo "STAGED $OUT"
  exit 0
fi

export CARGO_TARGET_DIR=$HOME/target/rustkube-e2e
cargo build -q -p kube-apiserver -p kube-controller-manager -p kube-scheduler

W=$(mktemp -d)
trap 'rm -rf "$W"' EXIT
git clone -q --depth 1 https://github.com/glennswest/fastetcd "$W/fastetcd"
FSHA=$(git -C "$W/fastetcd" rev-parse HEAD)
(cd "$W/fastetcd" && CARGO_TARGET_DIR=$HOME/target/fastetcd-e2e cargo build -q -p fastetcd-server)
FASTETCD=$(ls "$HOME"/target/fastetcd-e2e/debug/fastetcd* | grep -v '\.d$' | head -1)

mkdir -p "$OUT.part/bin"
for b in kube-apiserver kube-controller-manager kube-scheduler; do
  cp "$CARGO_TARGET_DIR/debug/$b" "$OUT.part/bin/"
done
cp "$FASTETCD" "$OUT.part/fastetcd"
{
  echo "rustkube $SHA"
  echo "fastetcd $FSHA"
  (cd "$OUT.part" && sha256sum bin/* fastetcd)
} >"$OUT.part/MANIFEST"
mv "$OUT.part" "$OUT"
cat "$OUT/MANIFEST"
echo "STAGED $OUT"
