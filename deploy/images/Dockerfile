# Control-plane container image (one per component). The images workflow
# cargo-builds the release binaries once, then builds this image per component,
# passing the binary name as COMPONENT and staging it in the build context. The
# resulting image is `docker save`d and attached to the GitHub release as a
# versioned tarball (no registry) — stormcos preloads it into the node store.
#
#   docker build -f deploy/images/Dockerfile --build-arg COMPONENT=kube-apiserver \
#     -t rustkube-apiserver:v0.7.34 .
#   docker save rustkube-apiserver:v0.7.34 | gzip > rustkube-apiserver-v0.7.34.docker.tar.gz
#
# Distroless: the binaries are rustls (no OpenSSL). The entrypoint is the
# component binary at a fixed path; static-pod manifests pass the flags as args.
FROM gcr.io/distroless/cc-debian12:nonroot
ARG COMPONENT
COPY ${COMPONENT} /usr/local/bin/rustkube-component
ENTRYPOINT ["/usr/local/bin/rustkube-component"]
