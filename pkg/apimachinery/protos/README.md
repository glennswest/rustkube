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

`authentication/v1`, `authorization/v1` and `node/v1` were added from the
same branch on 2026-09-27 (#67).

`resource/v1` (Dynamic Resource Allocation, GA in 1.34) is from `release-1.34`
(#137): it does not exist in 1.32. Its imports (core/v1 `NodeSelector`,
`Quantity`, `RawExtension`) are all in the 1.32 files above.

`google/protobuf/descriptor.proto` is not vendored; it ships with `protoc`.

## Updating
Re-fetch the `generated.proto` for a group from the matching Kubernetes release
and drop it in the same path, then add it to the `PROTOS` list in `build.rs`.
Do not hand-edit — these are generated upstream.
