# What the e2e rigs run besides the commit's own binaries, in one place, so
# the rigs and the test image (test/build.sh stages these, #173) agree.
# Each is overridable from the environment.
: "${RK_FASTETCD_REF:=v1.12.0}"        # fastetcd tag the rigs' datastore is built from
: "${SNAP_VERSION:=v8.6.0}"            # external-snapshotter: snapshot-controller, CRDs, RBAC
: "${HOSTPATH_VERSION:=v1.17.1}"       # CSI hostpath driver
: "${PROVISIONER_VERSION:=v6.3.0}"     # external-provisioner
# v2.3.0 is released on GitHub but has no image on registry.k8s.io yet.
: "${RESIZER_VERSION:=v2.2.0}"         # external-resizer
: "${KUBECTL_CHANNEL:=stable-1.36}"    # dl.k8s.io release channel for kubectl
