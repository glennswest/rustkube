#!/usr/bin/env bash
#
# The whole conformance run, on the conformance VM (conform.g8.lo), for one
# staged commit:
#
#   test/conformance/vm.sh <rustkube sha>      # as `conform`, from ~/rustkube
#
# 1. fetches /build/assets/conformance/<sha> from the build box (read-only
#    rsync; stage it first with `sc-build test/conformance/stage.sh`),
# 2. checks this clone out at the same commit, so the test scripts match the
#    binaries,
# 3. runs the six SIG chunks side by side, each on its own ports,
# 4. writes ~/results/<sha>/<chunk>.log and a summary of RESULT counts.
#
# Nothing is compiled here and no build slot is used.
set -uo pipefail
SHA=${1:?usage: vm.sh <rustkube sha>}
SRC=${RK_STAGE_SRC:-stormbuild@dev.g8.lo:}
S=/srv/conformance/$SHA
OUT=$HOME/results/$SHA
cd "$(dirname "$0")/../.."

mkdir -p "$S" "$OUT"
rsync -a "$SRC$SHA/" "$S/" || { echo "cannot fetch $SHA: stage it with sc-build test/conformance/stage.sh"; exit 2; }
(cd "$S" && grep -E '^[0-9a-f]{64}  ' MANIFEST | sha256sum -c --quiet) || { echo "staged binaries fail their MANIFEST"; exit 2; }
git fetch -q origin && git checkout -q "$SHA" || { echo "cannot check out $SHA"; exit 2; }
cp "$S/MANIFEST" "$OUT/"

i=0
for f in 'sig-api-machinery' 'sig-apps' 'sig-(auth|cli|instrumentation|architecture|scheduling)' \
         'sig-network' 'sig-node' 'sig-storage'; do
  i=$((i + 10))
  name=$(echo "$f" | tr -c 'a-z0-9\n' '-' | sed 's/-*$//')
  RK_BIN=$S/bin RK_FASTETCD=$S/fastetcd RK_PORT_OFFSET=$i RK_SUITE_TIMEOUT=${RK_SUITE_TIMEOUT:-45m} \
    bash test/conformance/run.sh "\[$f\].*\[Conformance\]" >"$OUT/$name.log" 2>&1 &
done
wait

{
  echo "rustkube $SHA — $(date -Is)"
  for l in "$OUT"/*.log; do
    printf '%-60s passed %4d  failed %4d  skipped %4d\n' "$(basename "$l" .log)" \
      "$(grep -c '^RESULT passed' "$l")" "$(grep -c '^RESULT failed' "$l")" "$(grep -c '^RESULT skipped' "$l")"
  done
} | tee "$OUT/SUMMARY"
