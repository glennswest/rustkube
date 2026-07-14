//! rk-store: Kubernetes-oriented datastore client backed by external etcd/fastetcd.
//!
//! Speaks the etcd v3 gRPC wire protocol (via the `etcd-client` crate) to a
//! standalone datastore — fastetcd in the RustKube stack, or any etcd v3 server.
//! This is the "kube architecture": the API server talks to an external store
//! over the network rather than embedding one in-process.
//!
//! Handles the K8s key schema:
//! `/registry/{resource}/{name}` (cluster-scoped)
//! `/registry/{resource}/{namespace}/{name}` (namespace-scoped)

mod adapter;

pub use adapter::{EtcdStore, EtcdTls};
