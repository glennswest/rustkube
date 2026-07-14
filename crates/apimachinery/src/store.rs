use crate::watch::WatchEvent;
use crate::Result;
use async_trait::async_trait;
use std::time::Duration;
use tokio::sync::mpsc;

/// Unique lease identifier.
pub type LeaseId = u64;

/// Result of a list operation with pagination support.
pub struct ListResult {
    /// Key-value pairs with their revisions.
    pub items: Vec<(String, Vec<u8>, u64)>,
    /// Continuation token for the next page, if more results exist.
    pub continue_token: Option<String>,
    /// The current store revision at the time of the list.
    pub revision: u64,
}

/// Stream of watch events from the store.
pub type WatchStream = mpsc::Receiver<WatchEvent>;

/// Distributed KV store trait — the foundation of the API server.
///
/// All values are opaque byte slices (serialized JSON in practice).
/// Every mutation produces a monotonically increasing revision number
/// that maps to Kubernetes' `resourceVersion`.
#[async_trait]
pub trait KvStore: Send + Sync + 'static {
    /// Get a single key. Returns (value, revision) or None.
    async fn get(&self, key: &str) -> Result<Option<(Vec<u8>, u64)>>;

    /// Put a key-value pair. If `prev_revision` is Some, performs CAS.
    /// Returns the new revision.
    async fn put(&self, key: &str, value: &[u8], prev_revision: Option<u64>) -> Result<u64>;

    /// Delete a key. If `prev_revision` is Some, performs CAS.
    async fn delete(&self, key: &str, prev_revision: Option<u64>) -> Result<()>;

    /// List keys with a given prefix, with pagination.
    async fn list(
        &self,
        prefix: &str,
        limit: usize,
        continue_token: Option<&str>,
    ) -> Result<ListResult>;

    /// Watch for changes to keys with a given prefix, starting from a revision.
    async fn watch(&self, prefix: &str, start_revision: u64) -> Result<WatchStream>;

    /// Grant a lease with the given TTL.
    async fn lease_grant(&self, ttl: Duration) -> Result<LeaseId>;

    /// Keep a lease alive.
    async fn lease_keepalive(&self, id: LeaseId) -> Result<()>;

    /// Revoke a lease, deleting all associated keys.
    async fn lease_revoke(&self, id: LeaseId) -> Result<()>;

    /// Compact all revisions up to and including the given revision.
    async fn compact(&self, revision: u64) -> Result<()>;
}
