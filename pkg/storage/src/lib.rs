//! storage: Kubernetes-oriented datastore client backed by external etcd/fastetcd.
//!
//! Speaks the etcd v3 gRPC wire protocol (via the `etcd-client` crate) to a
//! standalone datastore — fastetcd in the RustKube stack, or any etcd v3 server.
//! This is the "kube architecture": the API server talks to an external store
//! over the network rather than embedding one in-process.
//!
//! Keys are opaque here. The apiserver lays them out as
//! `/registry/{resource}/[{namespace}/]{name}`, and custom resources as
//! `/registry/{group}/{plural}/[{namespace}/]{name}` (#76).

mod adapter;

pub use adapter::{EtcdStore, EtcdTls};
