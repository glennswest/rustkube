# Vendored Kubernetes protobuf definitions

These `.proto` files are vendored verbatim from Kubernetes (release-1.32) and
gogo/protobuf, and are compiled by `../build.rs` into a FileDescriptorSet that
`apimachinery::protobuf` loads to decode/encode the
`application/vnd.kubernetes.protobuf` wire format.

This mirrors how `k8s.io/apimachinery` + `k8s.io/api` hold the codec used by both
`k8s.io/apiserver` and `k8s.io/client-go`.

## Provenance
- `k8s.io/api/**` and `k8s.io/apimachinery/**` —
  https://github.com/kubernetes/kubernetes (staging), tag/branch `release-1.32`.
  Licensed Apache-2.0.
- `gogoproto/gogo.proto` — https://github.com/gogo/protobuf. Licensed BSD-3-Clause.

`google/protobuf/descriptor.proto` is not vendored; it ships with `protoc`.

## Updating
Re-fetch the `generated.proto` for a group from the matching Kubernetes release
and drop it in the same path, then add it to the `PROTOS` list in `build.rs`.
Do not hand-edit — these are generated upstream.
