#!/usr/bin/env bash
# The control-plane goldens — rustkube-apiserver, rustkube-controller-manager,
# rustkube-scheduler — built here, from this checkout, and nowhere else (#403,
# stormcentral#1836: "do repos, repos are not expensive"; stormcos's stage
# mode is retired).
#
#   deploy/build-image.sh <rustkube-apiserver|rustkube-controller-manager|rustkube-scheduler> OUT
#
# stormcentral's component build runs it (kind `tree`: `deploy/build-golden.sh
# <name> OUT`, which execs this) in a checkout fetched at the exact commit, on
# a fresh build VM, as root of the build's own user namespace. OUT is copied
# into the sealed read-only golden `golden-<name>-<build>`, sized from the
# content, with `SBOM/crates.csv` added by the platform. Each is a member of
# the `kube1` pallet (stormcos deploy/image.toml), laid at /pallets/<name> and
# run by the kubelet as a static pod (stormcos#559).
#
# What a golden holds — only what is rustkube's (#403, the master's option A):
#
#   usr/bin/kube-<component>  the binary, static musl, the release profile
#                             (`dist` if Cargo.toml ever defines one). The
#                             path stormcos's static pods run (`command`).
#   stormd -> .stormd/bin/stormd   the release's one stormd, mounted at
#                             /.stormd by the kubelet (rustkube-node#465) and
#                             the units (stormcos#1551): stormcentral#1628's
#                             phase 3, so no stormd bytes are in the image.
#   bin/<applet>, usr/bin/<applet>  links to ../stormd (stormd's applet
#                             list), the tools a `kubectl exec` gets.
#   etc/stormd/config.toml    the default config: this component's own
#                             [[process]] and API probe, with the flags the
#                             stage wrote for it, and nothing for any other
#                             program. See below.
#   etc/kubernetes/manifests.d/  apiserver only: empty, the mount point for
#                             the cluster manifests (--manifest-dir).
#   bin sbin usr/sbin data dev proc sys run tmp var/log/stormd .stormd
#                             the stormdbase mount points, as the stage made.
#
# What the stage also put in these goldens and this does NOT (it is not
# rustkube's, and a stormcos commit must never rebuild a rustkube image):
#
#   - stormcos's 67 cluster manifests (deploy/manifests, the network/cilium
#     and network/flowsdn trees, the cadvisor route, the lb-pool links);
#   - the apiserver's ~80 stormcert mints, renew, the stormcert server and
#     its config, the Cilium Secret writers — the node's certificate policy;
#   - etc/kubernetes/static/*.json|.env, made by stormcos's gen-static-pod.py;
#   - the sno `--dev-anonymous-admin`, the cluster VIP SAN, cadvisor TLS and
#     stormd API TLS edits;
#   - stormcert, stormcert-agent and stormcertctl (stormcert's own golden).
#
# Those move to stormcos's content golden (stormcentral#1836). Until it
# carries them, the stage keeps building these goldens: the registry row is
# flipped from special to tree only when both halves exist.
#
# GOLDEN_ARCH: x86_64 (native) or aarch64 (cargo-zigbuild, as stormcentral's
# own component builds; prints `@@ARCH aarch64`). Anything else exits 2.
set -euo pipefail
usage="usage: deploy/build-image.sh <rustkube-apiserver|rustkube-controller-manager|rustkube-scheduler> OUT"
name=${1:?$usage}
out=${2:?$usage}
case "$name" in
    rustkube-apiserver|rustkube-controller-manager|rustkube-scheduler) ;;
    *) echo "build-image: this repo builds rustkube-apiserver, rustkube-controller-manager and rustkube-scheduler, not '$name'" >&2; exit 2 ;;
esac
comp=${name#rustkube-}
bin=kube-$comp
arch=${GOLDEN_ARCH:-x86_64}
case "$arch" in arm64) arch=aarch64 ;; amd64) arch=x86_64 ;; esac
case "$arch" in
    x86_64)  cb=build ;;
    aarch64) cb=zigbuild; echo "@@ARCH aarch64" ;;
    *) echo "build-image: GOLDEN_ARCH=$arch: x86_64 or aarch64 only" >&2; exit 2 ;;
esac
cd "$(dirname "$0")/.."
# shellcheck disable=SC1091
source "$HOME/.cargo/env" 2>/dev/null || true
TARGET=$arch-unknown-linux-musl
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$PWD/target}"
commit=$(git rev-parse HEAD)
version=$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
# The profile a golden ships (stormblock#209): `dist` when the workspace
# defines one, else `release` — the stage's rule and stormcentral's, so one
# commit gives the same bytes either way.
profile=release; grep -q '^\[profile\.dist\]' Cargo.toml && profile=dist

echo "==> cargo $cb --profile $profile --locked --target $TARGET -p $bin ($commit)"
CONST_RANDOM_SEED="$commit" SOURCE_DATE_EPOCH="$(git log -1 --format=%ct)" \
    cargo "$cb" --profile "$profile" --locked --target "$TARGET" --quiet -p "$bin"
BIN="$CARGO_TARGET_DIR/$TARGET/$profile/$bin"
[ -x "$BIN" ] || { echo "build-image: cargo produced no $BIN" >&2; exit 1; }
# Static: the golden has no loader and no libc.
if command -v file >/dev/null 2>&1; then
    file "$BIN" | grep -Eq 'static-pie linked|statically linked' || { echo "build-image: not static: $(file "$BIN")" >&2; exit 1; }
fi
# It runs, and is the program it is named for (native only: a cross build
# cannot run here). The binaries have no --version (clap `version` unset).
if [ "$arch" = x86_64 ]; then
    help=$("$BIN" --help 2>&1) || { echo "build-image: $bin --help failed: $(printf '%s\n' "$help" | tail -2)" >&2; exit 1; }
    case "$help" in *"$bin"*) ;; *) echo "build-image: $bin --help does not name $bin" >&2; exit 1 ;; esac
fi

# The stormdbase layout (stormcos's stormdbase_stage, less the stormd copy).
mkdir -p "$out"/{bin,usr/bin,sbin,usr/sbin,etc/stormd,data,dev,proc,sys,run,tmp,var/log/stormd,.stormd}
chmod 0755 "$out" "$out"/{bin,usr,usr/bin,sbin,usr/sbin,etc,etc/stormd,data,dev,proc,sys,run,var,var/log,var/log/stormd,.stormd}
chmod 1777 "$out/tmp"
ln -s .stormd/bin/stormd "$out/stormd"
# The applets the stage linked: what its stormd listed (`--list-commands`,
# stormd's shell::STANDALONE_COMMANDS, here as at stormd d2cf426). With no
# stormd in the image there is none to ask at build time; a name stormd does
# not know is refused by it at run time (stormd#11), never run as another.
APPLETS="ls dir cat head tail cp mv rm mkdir touch chmod chown \
find ln stat pwd wc du readlink file sha256sum tee md5sum \
ifconfig ip ping curl wget netstat ss nslookup dig hostname route \
mount df free uname date id kill printenv export unset \
sleep echo env whoami which type lsof true false clear \
sort uniq cut tr sed rev base64 xxd grep test ["
set -f   # `[` is a name here, not a pattern
for a in $APPLETS; do
    ln -s ../stormd "$out/bin/$a"
    ln -s ../../stormd "$out/usr/bin/$a"
done
set +f
install -m 0755 "$BIN" "$out/usr/bin/$bin"

# The default config. stormd is not this container's PID 1 on a node since
# stormcos#559 (the kubelet runs the static pod's command directly), so this
# is what the static pod's command and args are generated from and what a
# stormd run of the image starts. Its flags are the stage's for this
# component, which read the node's tier-0 pairs under /data/stormcert and
# ${NODE_IP} from the downward API; the processes that mint those pairs are
# the node's, not here.
cfg="$out/etc/stormd/config.toml"
case "$comp" in
apiserver)
    mkdir -p "$out/etc/kubernetes/manifests.d"
    chmod 0755 "$out/etc/kubernetes" "$out/etc/kubernetes/manifests.d"
    cat > "$cfg" << 'CONF'
[general]
name = "rustkube-apiserver"
log_dir = "/var/log/stormd"

[api_health]
state_file = "/run/stormpump/health.d/rustkube-apiserver.json"

[api]
bind = "0.0.0.0:9082"

[[process]]
name = "apiserver"
command = "/usr/bin/kube-apiserver"
args = [
  "--etcd-servers", "https://127.0.0.1:2379",
  "--etcd-cacert", "/data/stormcert/ca.crt",
  "--etcd-cert", "/data/stormcert/apiserver-etcd-client.crt",
  "--etcd-key", "/data/stormcert/apiserver-etcd-client.key",
  "--bind-addr", "0.0.0.0",
  "--advertise-address", "${NODE_IP}",
  "--tls-cert-file", "/data/stormcert/apiserver.crt",
  "--tls-private-key-file", "/data/stormcert/apiserver.key",
  "--token-auth-file", "/state/config/token-auth.csv",
  "--service-account-signing-key-file", "/data/stormcert/sa-token.key",
  "--service-account-key-file", "/data/stormcert/sa-token.pub",
  "--client-ca-file", "/data/stormcert/client-ca.crt",
  "--client-crl-file", "/data/stormcert/client.crl,/data/stormcert/crl/root.crl",
  "--proxy-client-cert-file", "/data/stormcert/front-proxy-client.crt",
  "--proxy-client-key-file", "/data/stormcert/front-proxy-client.key",
  "--requestheader-client-ca-file", "/data/stormcert/ca.crt",
  "--requestheader-allowed-names", "front-proxy-client",
  "--manifest-dir", "/etc/kubernetes/manifests.d"
]
on_failure = "restart"
on_exit = "restart"
# The datastore first: the apiserver exits when etcd is unreachable.
ready_probe = { type = "tcp", port = 2379, interval_secs = 1 }

[[process.api]]
name = "namespace-default"
url = "https://127.0.0.1:6443/api/v1/namespaces/default"
p99_ms = 200
timeout_secs = 10
token_file = "/data/stormcert/node-admin.token"
restart_after_stalled_secs = 600
CONF
    ;;
controller-manager|scheduler)
    if [ "$comp" = controller-manager ]; then port=9083; health=10257; else port=9084; health=10259; fi
    {
        cat << CONF
[general]
name = "rustkube-$comp"
log_dir = "/var/log/stormd"

[api_health]
state_file = "/run/stormpump/health.d/rustkube-$comp.json"

[api]
bind = "0.0.0.0:$port"

[[process]]
name = "$comp"
command = "/usr/bin/kube-$comp"
args = [
  "--apiserver", "https://\${NODE_IP}:6443",
  "--certificate-authority", "/data/stormcert/ca.crt",
  "--client-certificate", "/data/stormcert/kube-$comp.crt",
  "--client-key", "/data/stormcert/kube-$comp.key",
  "--tls-cert-file", "/data/stormcert/kube-$comp-serving.crt",
  "--tls-private-key-file", "/data/stormcert/kube-$comp-serving.key",
CONF
        # The signerNames stormcert's server signs (#286, #199): the
        # controller-manager neither approves nor signs them.
        [ "$comp" = controller-manager ] && echo '  "--csr-external-signer-names", "kubernetes.io/kube-apiserver-client,kubernetes.io/kube-apiserver-client-kubelet,kubernetes.io/kubelet-serving",'
        cat << CONF
]
on_failure = "restart"
on_exit = "restart"
# The apiserver on this machine first: a leader election against nothing
# elects nobody.
ready_probe = { type = "http", url = "https://127.0.0.1:6443/healthz", interval_secs = 2 }

[[process.api]]
name = "healthz"
url = "https://127.0.0.1:$health/healthz"
p99_ms = 100
timeout_secs = 5
restart_after_stalled_secs = 600
CONF
    } > "$cfg"
    ;;
esac
# The stage's log limits (stormd_log_limits).
cat >> "$cfg" << 'CONF'

[stormlog.file]
max_size_bytes = 8388608
max_files = 3
max_runs = 5
CONF
chmod 0644 "$cfg"
# The config parses, and names the binary that is in the image.
python3 - "$cfg" "/usr/bin/$bin" << 'PY'
import sys, tomllib
cfg = tomllib.load(open(sys.argv[1], "rb"))
procs = cfg.get("process", [])
if len(procs) != 1 or procs[0]["command"] != sys.argv[2]:
    sys.exit(f"build-image: the config's processes are {[p.get('command') for p in procs]}, not [{sys.argv[2]!r}]")
PY

# Read back what was copied: a short copy is never sealed (stormcentral#716).
[ "$(sha256sum < "$out/usr/bin/$bin")" = "$(sha256sum < "$BIN")" ] || { echo "build-image: usr/bin/$bin does not read back as built" >&2; exit 1; }
echo "==> $name golden tree in $out: $bin $version at $commit ($arch)"
