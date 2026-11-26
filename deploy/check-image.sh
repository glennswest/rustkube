#!/usr/bin/env bash
# Build the three control-plane golden trees with deploy/build-golden.sh (the
# entry point stormcentral runs) and check what is in them (#403).
#
#   deploy/check-image.sh [STORMCOS_CHECKOUT]
#
# With a stormcos checkout, also: its static-pod generator
# (deploy/static-pods/gen-static-pod.py, stormcos#559) reads each tree's
# default config and its templates, as the stage did, and the command and
# args it writes are the stage's for that component — taken from the stage
# recipe's own heredocs in that checkout's deploy/build-goldens.sh, less the
# stormcos-only edits (APISERVER_EXTRA, the cadvisor and VIP rewrites).
# Run on a build VM (sc-build); writes only under $TMPDIR.
set -euo pipefail
cd "$(dirname "$0")/.."
stormcos=${1:-}
work=$(mktemp -d "${TMPDIR:-/tmp}/check-image.XXXXXX")
fail=0
bad() { echo "FAIL: $*"; fail=1; }
for name in rustkube-apiserver rustkube-controller-manager rustkube-scheduler; do
    comp=${name#rustkube-}; bin=kube-$comp; out="$work/$name"
    echo "== $name"
    bash deploy/build-golden.sh "$name" "$out"
    [ -x "$out/usr/bin/$bin" ] || bad "$name: no usr/bin/$bin"
    [ "$(readlink "$out/stormd")" = .stormd/bin/stormd ] || bad "$name: /stormd is not the link .stormd/bin/stormd"
    for d in bin usr/bin sbin usr/sbin etc/stormd data dev proc sys run tmp var/log/stormd .stormd; do
        [ -d "$out/$d" ] || bad "$name: no $d/"
    done
    [ "$(stat -c %a "$out/tmp")" = 1777 ] || bad "$name: tmp is not 1777"
    [ "$(readlink "$out/bin/ls")" = ../stormd ] && [ "$(readlink "$out/usr/bin/grep")" = ../../stormd ] || bad "$name: applets are not links to ../stormd"
    # Nothing of stormcos's or stormcert's.
    for x in usr/bin/stormcert usr/bin/stormcert-agent usr/bin/stormcertctl etc/stormcert-server.toml etc/kubernetes/static etc/kubernetes/network; do
        [ ! -e "$out/$x" ] || bad "$name: carries $x (stormcos/stormcert content, #403)"
    done
    if [ "$comp" = apiserver ]; then
        [ -d "$out/etc/kubernetes/manifests.d" ] && [ -z "$(ls -A "$out/etc/kubernetes/manifests.d")" ] || bad "$name: manifests.d is not an empty mount point"
    fi
    files=$(cd "$out" && find . -type f | sort)
    echo "   files: $(echo "$files" | tr '\n' ' ')"
    [ "$(echo "$files" | wc -l)" = 2 ] || bad "$name: expected 2 files (the binary, the config), got: $files"
    # A build is reproducible: the same commit, the same binary.
    sha1=$(sha256sum < "$out/usr/bin/$bin")
    bash deploy/build-golden.sh "$name" "$work/$name.again" >/dev/null
    [ "$(sha256sum < "$work/$name.again/usr/bin/$bin")" = "$sha1" ] || bad "$name: a second build of the same commit gave other bytes"
    cmp -s "$out/etc/stormd/config.toml" "$work/$name.again/etc/stormd/config.toml" || bad "$name: a second build gave another config"
done
# Unknown names and architectures are refused.
if bash deploy/build-golden.sh rustkube-node "$work/x" 2>/dev/null; then bad "rustkube-node was accepted"; fi
if GOLDEN_ARCH=riscv64 bash deploy/build-golden.sh rustkube-scheduler "$work/y" 2>/dev/null; then bad "GOLDEN_ARCH=riscv64 was accepted"; fi

if [ -n "$stormcos" ]; then
    echo "== against stormcos $(git -C "$stormcos" rev-parse --short HEAD): gen-static-pod.py and the stage's flags"
    for name in rustkube-apiserver rustkube-controller-manager rustkube-scheduler; do
        comp=${name#rustkube-}
        python3 "$stormcos/deploy/static-pods/gen-static-pod.py" "$work/$name" "$comp" "$stormcos/deploy/static-pods/kube-$comp.yaml" || bad "$name: gen-static-pod.py refused the default config"
    done
    python3 - "$stormcos/deploy/build-goldens.sh" "$work" << 'PY' || fail=1
import json, re, sys
src, work = open(sys.argv[1]).read(), sys.argv[2]
def args_of(block, pname):
    m = re.search(r'\[\[process\]\]\nname = "' + re.escape(pname) + r'"\ncommand = "([^"]+)"(.*?)\nargs = \[(.*?)\n\]', block, re.S)
    if not m: sys.exit(f"no [[process]] {pname} in the stage recipe")
    body = re.sub(r'#[^\n]*', '', m.group(3)).replace('APISERVER_EXTRA', '').replace('$extra', '')
    return m.group(1), re.findall(r'"((?:[^"\\]|\\.)*)"', body)
ok = True
for comp in ["apiserver", "controller-manager", "scheduler"]:
    if comp == "apiserver":
        cmd, want = args_of(src, "apiserver")
    else:
        cmd, want = args_of(src, "$comp")
        cmd = cmd.replace("$comp", comp)
        want = [a.replace("$comp", comp).replace("\\${NODE_IP}", "${NODE_IP}") for a in want]
        if comp == "controller-manager":
            m = re.search(r'controller-manager\) port=9083.*?extra=\'\s*(.*?)\' ;;', src, re.S)
            want += re.findall(r'"([^"]+)"', m.group(1))
    pod = json.load(open(f"{work}/rustkube-{comp}/etc/kubernetes/static/kube-{comp}.json"))
    c = pod["spec"]["containers"][0]
    got = [a.replace("$(NODE_IP)", "${NODE_IP}") for a in c["args"]]
    if c["command"] != [cmd] or got != want:
        ok = False
        print(f"FAIL: {comp}: static pod runs {c['command']} {got}\n      the stage ran [{cmd!r}] {want}")
    else:
        print(f"   {comp}: static pod command and {len(got)} args are the stage's")
sys.exit(0 if ok else 1)
PY
fi
rm -rf "$work"
[ "$fail" = 0 ] && echo "check-image: all passed" || { echo "check-image: FAILED"; exit 1; }
