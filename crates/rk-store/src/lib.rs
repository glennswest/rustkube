//! rk-store: Kubernetes-oriented KV store backed by stormforce-kv.
//!
//! Wraps stormforce-kv's MVCC store to provide the `KvStore` trait
//! for the API server. Handles K8s key schema:
//! `/registry/{resource}/{name}` (cluster-scoped)
//! `/registry/{resource}/{namespace}/{name}` (namespace-scoped)

mod adapter;

pub use adapter::StormforceStore;
pub use stormforce_kv::{KvEngine, KvError};
pub use stormforce_kv::store::MvccStore;
pub use stormforce_kv::watch::WatchHub;
pub use stormforce_kv::lease::LeaseManager;
