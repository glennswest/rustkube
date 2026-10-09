#!/bin/sh
# Build rustkube's test image context for the commit checked out (#96).
#
#   test/build.sh [target]        default x86_64-unknown-linux-musl
#   (the rustkube binaries follow [target]; oc, kubectl and the CSI tools
#   staged below are x86_64/amd64 whatever it is)
#
# Per stormcentral docs/test-standard.md this runs first, in the checkout on
# the build box, with cargo: it builds the static test binary and stages it
# in test/.stage/, and test/Containerfile (context: the repo root) packages
# it. One image serves every suite (`/test <suite>`): short, medium and long
# against the node's cluster, and rigs/rigs-night, the e2e rigs (#173), for
# which this also stages the commit's control-plane binaries, fastetcd and the
# upstream tools the rigs drive. Needs network access to GitHub,
# mirror.openshift.com, dl.k8s.io and registry.k8s.io (the build box has it;
# the test pod needs none).
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

# --- the e2e rigs (`/test rigs`, `/test rigs-night`, #173) --------------------
# Each rig starts its own fastetcd and control plane inside the test pod, from
# this commit's binaries; the upstream tools some rigs drive are staged here
# too, so the pod needs neither cargo, podman nor the internet. What versions:
# test/e2e/versions.sh, shared with the rigs.
. "$root/test/e2e/versions.sh"
rigs="$stage/rigs"
mkdir -p "$rigs/bin" "$rigs/tools/snapshot"
cargo build --release --locked --target "$target" --manifest-path "$root/Cargo.toml" \
    -p kube-apiserver -p kube-controller-manager -p kube-scheduler
for b in kube-apiserver kube-controller-manager kube-scheduler; do
    cp "$tdir/$target/release/$b" "$rigs/bin/"
done

# fastetcd at the pinned tag, built here in the job's own directory.
fe="$stage/.fastetcd-src"
git -c advice.detachedHead=false clone -q --depth 1 --branch "$RK_FASTETCD_REF" \
    https://github.com/glennswest/fastetcd "$fe"
fe_target="$tdir/fastetcd"
if cargo build -q --release --locked --target "$target" -p fastetcd-server \
        --manifest-path "$fe/Cargo.toml" --target-dir "$fe_target"; then
    cp "$fe_target/$target/release/fastetcd" "$rigs/bin/"
else
    # Not every fastetcd builds for musl; the image is fedora-minimal, so a
    # glibc build runs there too.
    echo "fastetcd: no $target build, building for the host" >&2
    cargo build -q --release --locked -p fastetcd-server \
        --manifest-path "$fe/Cargo.toml" --target-dir "$fe_target"
    cp "$fe_target/release/fastetcd" "$rigs/bin/"
fi
echo "fastetcd $(git -C "$fe" rev-parse HEAD) ($RK_FASTETCD_REF)" >"$rigs/VERSIONS"
rm -rf "$fe"

# Upstream tools.
fetch() { python3 -I "$root/test/fetch-image-file.py" "$@"; }
tools="$rigs/tools"
curl -sfL https://mirror.openshift.com/pub/openshift-v4/x86_64/clients/ocp/stable/openshift-client-linux.tar.gz \
    | tar -xz -C "$tools" oc
kver=$(curl -sfL "https://dl.k8s.io/release/$KUBECTL_CHANNEL.txt")
curl -sfL -o "$tools/kubectl" "https://dl.k8s.io/$kver/bin/linux/amd64/kubectl"
chmod +x "$tools/oc" "$tools/kubectl"
fetch "registry.k8s.io/sig-storage/hostpathplugin:$HOSTPATH_VERSION" /hostpathplugin "$tools/hostpathplugin"
fetch "registry.k8s.io/sig-storage/csi-provisioner:$PROVISIONER_VERSION" /csi-provisioner "$tools/csi-provisioner"
fetch "registry.k8s.io/sig-storage/csi-resizer:$RESIZER_VERSION" /csi-resizer "$tools/csi-resizer"
fetch "registry.k8s.io/sig-storage/snapshot-controller:$SNAP_VERSION" /snapshot-controller "$tools/snapshot-controller"
raw="https://raw.githubusercontent.com/kubernetes-csi/external-snapshotter/$SNAP_VERSION"
for c in snapshot.storage.k8s.io_volumesnapshotclasses snapshot.storage.k8s.io_volumesnapshotcontents \
         snapshot.storage.k8s.io_volumesnapshots groupsnapshot.storage.k8s.io_volumegroupsnapshotclasses \
         groupsnapshot.storage.k8s.io_volumegroupsnapshotcontents groupsnapshot.storage.k8s.io_volumegroupsnapshots; do
    curl -sfL -o "$tools/snapshot/$c.yaml" "$raw/client/config/crd/$c.yaml"
done
curl -sfL -o "$tools/snapshot/rbac-snapshot-controller.yaml" \
    "$raw/deploy/kubernetes/snapshot-controller/rbac-snapshot-controller.yaml"
{
    echo "oc $("$tools/oc" version --client 2>/dev/null | head -1)"
    echo "kubectl $kver"
    echo "hostpathplugin $HOSTPATH_VERSION, csi-provisioner $PROVISIONER_VERSION, csi-resizer $RESIZER_VERSION"
    echo "snapshot-controller $SNAP_VERSION"
} >>"$rigs/VERSIONS"
cat "$rigs/VERSIONS"

if [ "${STAGE_ONLY:-0}" = 1 ] || [ -n "${CARGO_TARGET_DIR:-}" ]; then
    # stormcentral's runner (which sets CARGO_TARGET_DIR) does its own podman build.
    echo "$stage"
    exit 0
fi
podman build -f "$root/test/Containerfile" --build-arg COMMIT="$commit" -t rustkube-test "$root"
