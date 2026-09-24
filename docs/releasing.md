# Releasing

Two artifacts per component, both static: a **binary tarball** and a
**`FROM scratch` container image**. Nothing else is published — no registry, no
GHCR (see the note at the end). The `kubernetes-rs` RPM/deb that
`deploy/packaging/nfpm.yaml` describes is built by nothing; the last release
that carried one was v0.7.30.

**stormcos does not consume these.** Its `deploy/build-goldens.sh` builds the
three binaries from source at a pinned commit and copies them into its
goldens (see the README's *How it ships*).

## Build

Official release artifacts come from CI (below). To produce the same thing on
the build box, run `deploy/build-release.sh` through `sc-build`, with `OUT`
somewhere that survives the scratch checkout:

```bash
sc-build 'NO_IMAGES=1 OUT=$HOME/rustkube-release deploy/build-release.sh'
```

It builds `x86_64-unknown-linux-musl` release binaries for the three
components (`protoc` is required: `pkg/apimachinery/build.rs` compiles the
protobuf descriptors), refuses to continue if any came out dynamically linked,
tars each one, and — unless `NO_IMAGES=1` — builds a `FROM scratch` image
around it with podman. Knobs:

| variable | default | |
|---|---|---|
| `OUT` | `/build/rustkube-release/v<version>` | where artifacts go |
| `TARGETS` | the native `<arch>-unknown-linux-musl` | comma-separated; a non-native target goes through `cross` |
| `NO_IMAGES` | unset | binaries and tarballs only |
| `CARGO_TARGET_DIR` | `/build/cargo/rustkube` | |

An aarch64 build (`TARGETS=…,aarch64-unknown-linux-musl`) goes through
`cross`, because `ring` needs a C toolchain for the target and dev has none.
**No aarch64 build has been recorded as succeeding** (#68).

## What comes out, and why it is static

| | v0.7.35 (Distroless) | v0.8.0 (static musl, scratch) |
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
linked` or `statically linked`. A build that silently goes dynamic works perfectly on the build host
and fails on the node, which is the worst place to find out.

## Verified on dev

The scratch image is not a theory — it serves:

```
$ podman run --network host rustkube-apiserver:v0.8.0-x86_64 \
    --etcd-servers http://127.0.0.1:2379 --tls --secure-port 6443
readyz:  ok
namespaces: default kube-node-lease kube-public kube-system
```

`/etc/resolv.conf` and `/etc/hosts` are injected by the kubelet (and by podman),
so name resolution works with nothing in the image. TLS needs no system trust
store: rustls carries its roots, and every path that verifies a peer is given an
explicit CA file.

## CI

`.github/workflows/images.yml` runs on a `v*` tag (or by hand, with a `ref`)
on ubuntu-latest, x86_64 only: musl build, static check, then
`docker build -f deploy/images/Dockerfile --build-arg COMPONENT=<c>`. It
creates the GitHub release if it does not exist and attaches six assets:

```
rustkube-{apiserver,controller-manager,scheduler}-<tag>-x86_64-linux-musl.tar.gz
rustkube-{apiserver,controller-manager,scheduler}-<tag>.docker.tar.gz
```

The two paths are the same build but not the same names: CI tags images
`rustkube-<c>:<tag>` with no arch and takes the version from the git tag;
`build-release.sh` tags `rustkube-<c>:v<version>-<arch>` and reads the version
from `Cargo.toml`. In the image the binary is `/usr/local/bin/rustkube-component`
and runs as uid 0.

GHCR is deliberately unused, and nothing on a node's start path pulls,
extracts or verifies an image.
