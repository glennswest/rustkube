# Control-plane container image (one per component). The workflow cargo-builds
# the release binaries once, then builds this image per component, passing the
# binary name as COMPONENT and staging it in the build context.
#
#   docker build -f deploy/images/Dockerfile --build-arg COMPONENT=kube-apiserver \
#     -t ghcr.io/glennswest/rustkube-apiserver:v0.7.34 -t ...:latest .
#
# Distroless/static: the binaries are musl-friendly rustls (no OpenSSL). The
# entrypoint is the component binary at a fixed path; static-pod manifests pass
# the flags as args.
FROM gcr.io/distroless/cc-debian12:nonroot
ARG COMPONENT
COPY ${COMPONENT} /usr/local/bin/rustkube-component
ENTRYPOINT ["/usr/local/bin/rustkube-component"]
