# Vendored Kubernetes protobuf definitions

These `.proto` files are vendored verbatim from Kubernetes (release-1.36) and
gogo/protobuf, and are compiled by `../build.rs` into a FileDescriptorSet that
`apimachinery::protobuf` loads to decode/encode the
`application/vnd.kubernetes.protobuf` wire format.

This mirrors how `k8s.io/apimachinery` + `k8s.io/api` hold the codec used by both
`k8s.io/apiserver` and `k8s.io/client-go`.

## Provenance
- `k8s.io/api/**` and `k8s.io/apimachinery/**` —
  https://github.com/kubernetes/kubernetes (staging; the published
  `kubernetes/api`, `kubernetes/apimachinery` and
  `kubernetes/apiextensions-apiserver` repos), branch `release-1.36`.
  Licensed Apache-2.0.
- `gogoproto/gogo.proto` — https://github.com/gogo/protobuf. Licensed BSD-3-Clause.

`authentication/v1`, `authorization/v1` and `node/v1` were added from the
same branch on 2026-09-27 (#67).

All were re-fetched from `release-1.36` on 2026-10-07 (#122), matching the
API posture the apiserver reports: server-side field validation for built-in
objects reads its field names from these descriptors, so a field newer than
the vendored files would be refused as unknown. The update from 1.32 only
added messages and fields (none removed or renumbered); `resource/v1` was
from `release-1.34` (#137), `networking/v1` from `release-1.36` (#134).

`autoscaling/v1` (HorizontalPodAutoscaler v1, and the `Scale` of every
`/scale` subresource) was added from `release-1.36` (#123).

`google/protobuf/descriptor.proto` is not vendored; it ships with `protoc`.

## Updating
Re-fetch the `generated.proto` for a group from the matching Kubernetes release
and drop it in the same path, then add it to the `PROTOS` list in `build.rs`.
Do not hand-edit — these are generated upstream.
