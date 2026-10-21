#!/usr/bin/env bash
#
# Deliver one commit's conformance binaries to the conformance VM, from the
# goldens (#140, owner: "Goldens"). Nothing is compiled.
#
#   test/conformance/fetch.sh <rustkube sha>
#
# Run where the `stormcentral` CLI and a token are (a stormcentral session on
# stormcentral.g8.lo): conform.g8.lo has neither. It
#
# 1. reads kube-apiserver, kube-controller-manager and kube-scheduler from the
#    rustkube-* stage goldens built at <sha> (`stormcentral component fetch`,
#    stormcentral#557: each binary checked against its build's sha256; exits
#    non-zero when nothing was built at <sha> — `stormcentral component build
#    rustkube` makes one), and fastetcd from its golden (the newest, or
#    RK_FASTETCD_COMMIT's),
# 2. writes MANIFEST: the commit, the goldens read, every binary's sha256,
# 3. copies the directory to $RK_CONFORM_HOST:/srv/conformance/<sha>, where
#    `test/conformance/vm.sh <sha>` runs it.
#
# The goldens are release builds (the in-slot runs and the old stage.sh used
# debug ones), and fastetcd is a golden, not fastetcd's main.
set -euo pipefail
SHA=${1:?usage: fetch.sh <rustkube sha>}
HOST=${RK_CONFORM_HOST:-conform@conform.g8.lo}
cd "$(dirname "$0")/../.."

W=${TMPDIR:-$PWD/tmp}/conformance-$SHA
rm -rf "$W" && mkdir -p "$W/bin" "$W/fastetcd.d"
trap 'rm -rf "$W"' EXIT

stormcentral component fetch rustkube --commit "$SHA" --out "$W/bin" \
  --binary kube-apiserver --binary kube-controller-manager --binary kube-scheduler | tee "$W/fetch.log"
stormcentral component fetch fastetcd ${RK_FASTETCD_COMMIT:+--commit "$RK_FASTETCD_COMMIT"} \
  --out "$W/fastetcd.d" --binary fastetcd | tee -a "$W/fetch.log"
mv "$W/fastetcd.d/fastetcd" "$W/fastetcd" && rmdir "$W/fastetcd.d"

{
  echo "rustkube $SHA"
  # `golden-<name> (<source>): <binary> …` → `golden <binary> golden-<name> (<source>)`
  sed -nE 's/^(golden-[^ ]+ \([^)]*\)): ([^ ]+) .*/golden \2 \1/p' "$W/fetch.log"
  (cd "$W" && sha256sum bin/* fastetcd)
} >"$W/MANIFEST"
rm "$W/fetch.log"
cat "$W/MANIFEST"

rsync -a --delete "$W/" "$HOST:/srv/conformance/$SHA/"
echo "DELIVERED $HOST:/srv/conformance/$SHA"
