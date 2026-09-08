# Control-plane container image — one per component, on **no base at all**.
#
# The binary staged into this build context is a static-PIE musl build: `ldd`
# says "statically linked", so there is no loader, no libc and no `/usr/lib` to
# ship with it. `FROM scratch` is therefore not a minimisation trick — it is the
# accurate description of what the image needs.
#
# What that buys, measured on v0.7.35: the Distroless image was 38 MB across
# 1,282 files, of which 20 MB was glibc and 5 MB was documentation and locale
# data a control-plane process never opens. The same component as a static musl
# binary is 12–14 MB in one file (#50).
#
# It matters more here than the numbers suggest. stormcos ships each component
# as a **golden** — a sealed filesystem image every node carries whether or not
# it runs that component, so promoting a node is starting a container rather
# than installing anything — and a base layer is slab on every node in the
# fleet. And a dynamically linked binary starts if and only if its loader and
# libraries are exactly where it expects them; a static binary in a golden has
# one file to be wrong about. stormblock and stormpump made the same call.
#
#   deploy/build-release.sh              # builds musl binaries + these images
#   podman build -f deploy/images/Dockerfile --build-arg COMPONENT=kube-apiserver \
#     -t rustkube-apiserver:v0.7.36 .
#
# Runtime notes: `/etc/resolv.conf` and `/etc/hosts` are injected by the kubelet
# (and by podman), so name resolution works without anything in the image. TLS
# needs no system trust store — rustls carries its roots, and every path that
# verifies a peer is given an explicit CA file.
FROM scratch
ARG COMPONENT
COPY ${COMPONENT} /usr/local/bin/rustkube-component
ENTRYPOINT ["/usr/local/bin/rustkube-component"]
