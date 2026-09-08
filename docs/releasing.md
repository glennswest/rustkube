# Releasing

Two artifacts per component, both static: a **binary tarball** and a
**`FROM scratch` container image**. Nothing else is published — no registry, no
GHCR (see the note at the end).

## Build

On `root@dev.g8.lo`, never on a workstation:

```bash
./deploy/build-release.sh
```

That builds `x86_64-unknown-linux-musl` release binaries for the three
components, refuses to continue if any of them came out dynamically linked,
tars each one, and builds a `FROM scratch` image around it with podman.

Output goes to `$OUT`, default `/build/rustkube-release/v<version>` — the
spinning drive, because nothing that persists belongs on the SSD root. **Point
`OUT` at a golden's NVMe mount to write the artifacts straight there:**

```bash
OUT=/mnt/goldens/rustkube ./deploy/build-release.sh
TARGETS=x86_64-unknown-linux-musl,aarch64-unknown-linux-musl ./deploy/build-release.sh
NO_IMAGES=1 ./deploy/build-release.sh     # binaries only
```

A non-native target goes through `cross`: `ring` needs a C toolchain for the
target and dev has none installed for aarch64.

## What comes out, and why it is static

| | v0.7.35 (Distroless) | v0.7.36 (static musl, scratch) |
|---|---|---|
| apiserver | 37.2 MB | **14 MB** |
| controller-manager | 34.8 MB | **10 MB** |
| scheduler | 33.1 MB | **8.1 MB** |

The Distroless base carried 20 MB of glibc and 5 MB of documentation and locale
data per component, none of which a control-plane process opens (#50).

Size is the smaller half of the argument. stormcos ships each component as a
**golden** — a sealed filesystem image every node carries whether or not it runs
that component, so promoting a node to control plane is *starting a container*,
not installing anything. A base layer is slab on every node in the fleet. And a
dynamically linked binary starts if and only if its loader and libraries are
exactly where it expects them; a static binary in a golden has one file to be
wrong about. stormblock and stormpump made the same call.

The build refuses to package a binary `file(1)` does not call `static-pie
linked`. A build that silently goes dynamic works perfectly on the build host
and fails on the node, which is the worst place to find out.

## Verified on dev

The scratch image is not a theory — it serves:

```
$ podman run --network host rustkube-apiserver:v0.7.36-x86_64 \
    --etcd-servers http://127.0.0.1:2379 --tls --secure-port 6443
readyz:  ok
namespaces: default kube-node-lease kube-public kube-system
```

`/etc/resolv.conf` and `/etc/hosts` are injected by the kubelet (and by podman),
so name resolution works with nothing in the image. TLS needs no system trust
store: rustls carries its roots, and every path that verifies a peer is given an
explicit CA file.

## CI

`.github/workflows/images.yml` runs on a `v*` tag and does the same thing —
musl target, static check, scratch image — attaching both the `.docker.tar.gz`
and the `-x86_64-linux-musl.tar.gz` to the GitHub release. It must stay the same
build as `build-release.sh`: shipping a differently-linked artifact from the one
that was tested is the failure release assets exist to prevent.

GHCR is deliberately unused. CI attaches tarballs to the release; stormcos
preloads them into the node image store at image-build time, and the node
receives a golden as a CoW clone over NVMe/TCP. Nothing on the start path pulls,
extracts or verifies an image.
