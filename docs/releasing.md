# Building and delivery

As of 2026-10-02, builds run through **sc-build** after the commit is pushed.
GitHub Actions is disabled by owner decision (#114), and the repository has
no workflow: the old tag-triggered `.github/workflows/images.yml` was removed
on 2026-10-08. Nothing is built, tested or published on GitHub.

## Current delivery

```bash
git push
sc-build                       # cargo build && cargo test at the pushed commit
```

sc-build fetches the commit onto a private drive as the unprivileged build
user, runs the command and deletes the drive, pass or fail. Checkout, target,
HOME and TMPDIR are temporary. There is no persistent checkout on the build
host and no output directory that survives merely because it is outside the
checkout. A failed build is recorded as a GitHub build-failure issue. Since
dev.g8.lo was retired (stormcentral#521), `SC_BUILD_VM=1 sc-build '…'` runs the
same job on a fresh build VM. Tests of a running control plane (the e2e rigs,
conformance) never run in a build slot: see test/README.md.

For completed, verified work approved for delivery, request the component
golden once:

```bash
stormcentral component build rustkube --url http://stormcentral.g8.lo
```

This produces an immutable component golden containing the three binaries and
files the stormcos release request. No release mounts the component golden
yet (stormcos#62): the per-process `rustkube-apiserver`,
`rustkube-controller-manager` and `rustkube-scheduler` stormd goldens are
compiled from a rustkube source checkout by stormcos's stage build, which
records the commit. See README's **How it ships** for the checked stormcos
source and runtime flags. A component build or source commit is not proof
that a node has installed the resulting release.

Turbomode is on main (#163) and in every golden built since; its datastore
dependency fastetcd#50 is fixed in fastetcd v1.6.1, and latency and
multi-master acceptance remain #147/#149.

## Release build check (`deploy/build-release.sh`)

`deploy/build-release.sh` builds static musl binaries for all three components,
checks each with `file`, writes binary tarballs, and optionally builds and saves
`FROM scratch` images using podman. `protoc` is needed for the protobuf
schema descriptors. The script reads the version from `Cargo.toml`. Run it as
a build job, never as root (#156):

```bash
git push
sc-build deploy/build-release.sh                 # native target, tarballs + images
sc-build 'NO_IMAGES=1 deploy/build-release.sh'   # binaries and tarballs only
```

| Variable | Default | Meaning |
|---|---|---|
| `TARGETS` | `$(uname -m)-unknown-linux-musl` | comma-separated targets; non-native uses `cross` if installed, else (aarch64) clang + the GNU cross gcc, and an aarch64 binary must start under `qemu-aarch64-static` (#68) |
| `OUT` | `$TMPDIR/rustkube-release/v<version>` (`tmp/…` in the checkout without `TMPDIR`) | output directory, on the job's private volume |
| `CARGO_TARGET_DIR` | cargo's default, or what the job sets | build output directory |
| `NO_IMAGES` | unset | any nonempty value skips image creation |

ARM64 (#68): `sc-build 'TARGETS=x86_64-unknown-linux-musl,aarch64-unknown-linux-musl deploy/build-release.sh'`
builds both on a build VM. At 8e82cca the aarch64 release binaries were
kube-apiserver 16M, kube-controller-manager 11M, kube-scheduler 8.7M (x86_64:
17M, 12M, 9.4M), static, starting under qemu, with arm64 `FROM scratch`
images. No ARM64 golden is built (the component golden is x86_64), and no
image is published, so there is no multi-arch manifest to make.

Everything it writes lives on the job's private volume and is deleted with
it: the script **checks** that a commit builds into static binaries, tarballs
and images and prints their sizes and sha256s; it does not deliver them.
Delivery is the component golden above. There is no persistent output
directory, and none may be made with root, a persistent dev mount or a HOME
override. Conformance staging had the same class of issue in its own script;
the owner's answer there is goldens (#140), waiting for
stormcentral#557 (reading a golden's binaries from another machine).

Outputs, on the job's volume, when the script runs successfully:

- `rustkube-<component>-v<version>-<arch>-linux-musl.tar.gz`, containing
  `kube-<component>`;
- `rustkube-<component>-v<version>-<arch>.docker.tar.gz`, image tag
  `rustkube-<component>:v<version>-<arch>`.

`deploy/images/Dockerfile` places the binary at
`/usr/local/bin/rustkube-component` and sets that entrypoint. It supplies no
user directive, shell, init, CA files or runtime configuration. This scratch
image is different from stormcos's stormd-supervised runtime golden. GHCR is
not used. `deploy/packaging/nfpm.yaml` remains an RPM/deb description without
an active packaging path; legacy Terragrunt provisioning still requires that
RPM (#157).

## Architecture and evidence

The component golden is x86_64 musl. `deploy/build-release.sh` builds aarch64
musl too (above; 8e82cca), and the binaries start under qemu, but nothing has
run on an ARM64 device: device memory, startup and what a "minimal" MikroTik
build means stay with the owner on #68. Static linking and a qemu start do not
prove runtime support on the device.

Historical v0.8.0 measurements were approximately 14 MB for the apiserver,
10 MB for controller-manager and 8.1 MB for scheduler; they are not sizes for
the current binaries. That release's scratch apiserver answered readyz and
served namespaces in a podman test. Current verification is recorded against
specific commits in CLAUDE.md and the test documentation.

Version changes update the workspace version and Cargo.lock's workspace
packages together, with a separate release commit and tag per CLAUDE.md's
cross-project rules. A tag is version bookkeeping; Actions being disabled,
it does not trigger a supported build or deployment.
