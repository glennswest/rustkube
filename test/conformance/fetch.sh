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
#    RK_FASTETCD_COMMIT's). A stage rebuilds only the images whose sources
#    changed (#323): when <sha> has goldens but not one for every binary,
#    the missing one comes from the newest golden of its image built at an
#    ancestor of <sha> (the last RK_FETCH_ANCESTORS=300 commits) — the image
#    the stage kept. A commit with no golden at all is still refused,
# 2. writes MANIFEST: the commit, the goldens read (one taken from an
#    ancestor says `at <ancestor>`), every binary's sha256,
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

# A binary the stage at $SHA did not rebuild (its sources unchanged, #323):
# the newest golden of its image at an ancestor. `component fetch` exits 0
# with only some of the binaries asked for (stormcentral#1076) and non-zero
# at a commit with no image, so each ancestor is tried until the binary is
# written.
: >"$W/ancestor.log"
for b in kube-apiserver kube-controller-manager kube-scheduler; do
  [ -x "$W/bin/$b" ] && continue
  git rev-parse -q --verify "$SHA^{commit}" >/dev/null || git fetch -q origin
  for c in $(git rev-list --max-count="${RK_FETCH_ANCESTORS:-300}" "$SHA^"); do
    stormcentral component fetch rustkube --commit "$c" --out "$W/bin" --binary "$b" \
      >"$W/one.log" 2>&1 || continue
    [ -x "$W/bin/$b" ] || continue
    cat "$W/one.log"
    echo "$b: none built at $SHA; the newest at an ancestor is ${c:0:12}'s"
    sed -nE "s/^(golden-[^ ]+ \([^)]*\)): ($b) .*/golden \2 \1 at ${c:0:12}/p" "$W/one.log" >>"$W/ancestor.log"
    break
  done
done
rm -f "$W/one.log"

# A run on half a control plane tests nothing.
missing=""
for b in bin/kube-apiserver bin/kube-controller-manager bin/kube-scheduler fastetcd; do
  [ -x "$W/$b" ] || missing="$missing ${b#bin/}"
done
[ -z "$missing" ] || {
  echo "not delivered: no golden of$missing at $SHA or its last ${RK_FETCH_ANCESTORS:-300} ancestors" >&2
  exit 1
}

{
  echo "rustkube $SHA"
  # `golden-<name> (<source>): <binary> …` → `golden <binary> golden-<name> (<source>)`
  sed -nE 's/^(golden-[^ ]+ \([^)]*\)): ([^ ]+) .*/golden \2 \1/p' "$W/fetch.log"
  cat "$W/ancestor.log"
  (cd "$W" && sha256sum bin/* fastetcd)
} >"$W/MANIFEST"
rm "$W/fetch.log" "$W/ancestor.log"
cat "$W/MANIFEST"

rsync -a --delete "$W/" "$HOST:/srv/conformance/$SHA/"
echo "DELIVERED $HOST:/srv/conformance/$SHA"
