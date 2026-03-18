//! rk-store: Kubernetes-oriented KV store backed by stormforce-kv.
//!
//! Wraps the stormforce-kv MVCC store (etcd v3-compatible) to provide
//! the `KvStore` trait implementation for the API server. Stormforce-kv
//! provides Raft consensus, MVCC revisions, watch subscriptions, and
//! lease management — this crate adapts those to the K8s key schema
//! (`/{group}/{resource}/{namespace}/{name}`).

pub use stormforce_kv::{KvEngine, KvError};
