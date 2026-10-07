# Building and delivery

As of 2026-10-02, builds run through **sc-build** after the commit is pushed.
GitHub Actions is disabled by owner decision (#114). The retained
`.github/workflows/images.yml` describes an obsolete tag-triggered publication
path; it does not currently publish releases. Removing that file remains #114.

## Current delivery

```bash
git push
sc-build                       # cargo build && cargo test at the pushed commit
```

sc-build fetches the commit onto a private drive as the unprivileged build
user, runs the command and deletes the drive, pass or fail. Checkout, target,
HOME and TMPDIR are temporary. There is no persistent checkout on dev and no
output directory that survives merely because it is outside the checkout.
A failed build is recorded as a GitHub build-failure issue.

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

Turbomode is on main (#163); its datastore dependency fastetcd#50 is fixed in
fastetcd v1.6.1, and runtime acceptance remains #147/#149.

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
| `TARGETS` | `$(uname -m)-unknown-linux-musl` | comma-separated targets; non-native uses `cross` |
| `OUT` | `$TMPDIR/rustkube-release/v<version>` (`tmp/…` in the checkout without `TMPDIR`) | output directory, on the job's private volume |
| `CARGO_TARGET_DIR` | cargo's default, or what the job sets | build output directory |
| `NO_IMAGES` | unset | any nonempty value skips image creation |

Everything it writes lives on the job's private volume and is deleted with
it: the script **checks** that a commit builds into static binaries, tarballs
and images and prints their sizes and sha256s; it does not deliver them.
Delivery is the component golden above. There is no persistent output
directory, and none may be made with root, a persistent dev mount or a HOME
override. Conformance staging had the same class of issue in its own script;
the owner's answer there is goldens (#140), not yet implemented.

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

The retained GitHub workflow describes only x86_64 musl. The standalone script
accepts aarch64 musl through `cross`, but no successful ARM64 build and runtime
acceptance has been recorded here (#68). Static linking alone does not prove
cross-architecture runtime support.

Historical v0.8.0 measurements were approximately 14 MB for the apiserver,
10 MB for controller-manager and 8.1 MB for scheduler; they are not sizes for
the current binaries. That release's scratch apiserver answered readyz and
served namespaces in a podman test. Current verification is recorded against
specific commits in CLAUDE.md and the test documentation.

Version changes update the workspace version and Cargo.lock's workspace
packages together, with a separate release commit and tag per CLAUDE.md's
cross-project rules. A tag is version bookkeeping; Actions being disabled,
it does not trigger a supported build or deployment.
