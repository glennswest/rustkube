//! Generic resource storage layer.
//!
//! Bridges K8s resource CRUD to the KvStore, handling JSON serialization,
//! resourceVersion tracking, key construction, and metadata injection.

use crate::error::ApiError;
use crate::watch_cache::WatchCache;
use apimachinery::store::KvStore;
use serde_json::Value;
use std::sync::Arc;

/// Key prefix for all resources in the store.
const REGISTRY_PREFIX: &str = "/registry";

/// Generic resource storage — handles any K8s resource type.
pub struct ResourceStorage {
    store: Arc<dyn KvStore>,
    watch_cache: Arc<WatchCache>,
}

impl ResourceStorage {
    pub fn new(store: Arc<dyn KvStore>) -> Self {
        let watch_cache = Arc::new(WatchCache::new(store.clone()));
        Self { store, watch_cache }
    }

    /// Build the store key for a cluster-scoped resource.
    pub fn cluster_key(resource: &str, name: &str) -> String {
        format!("{REGISTRY_PREFIX}/{resource}/{name}")
    }

    /// Build the store key for a namespace-scoped resource.
    pub fn namespaced_key(resource: &str, namespace: &str, name: &str) -> String {
        format!("{REGISTRY_PREFIX}/{resource}/{namespace}/{name}")
    }

    /// Prefix for listing all instances of a cluster-scoped resource.
    pub fn cluster_prefix(resource: &str) -> String {
        format!("{REGISTRY_PREFIX}/{resource}/")
    }

    /// Prefix for listing all instances of a namespaced resource in one namespace.
    pub fn namespace_prefix(resource: &str, namespace: &str) -> String {
        format!("{REGISTRY_PREFIX}/{resource}/{namespace}/")
    }

    /// Prefix for listing all instances of a namespaced resource across all namespaces.
    pub fn all_namespaces_prefix(resource: &str) -> String {
        format!("{REGISTRY_PREFIX}/{resource}/")
    }

    /// Get a single resource by key.
    pub async fn get(&self, key: &str) -> Result<Value, ApiError> {
        match self.store.get(key).await.map_err(ApiError::from)? {
            Some((bytes, rev)) => {
                let mut obj: Value = serde_json::from_slice(&bytes)
                    .map_err(|e| ApiError::internal(&e.to_string()))?;
                // resourceVersion is the store's mod_revision, NOT whatever was
                // baked into the JSON on the last write (#33). Returning a stale
                // baked-in value breaks optimistic concurrency: the client PUTs
                // it back, the store CASes it against the real mod_revision, and
                // the mismatch 409s — which loops leader election forever.
                inject_resource_version(&mut obj, rev);
                Ok(obj)
            }
            None => Err(ApiError::not_found("resource", key)),
        }
    }

    /// List resources by prefix with pagination.
    pub async fn list(
        &self,
        prefix: &str,
        limit: usize,
        continue_token: Option<&str>,
    ) -> Result<(Vec<Value>, Option<String>, u64), ApiError> {
        // Served from the shared watch cache's in-memory snapshot (seeded once
        // from the store), so relist storms don't fan out to fastetcd.
        let (raw, continue_token, revision) = self
            .watch_cache
            .list(prefix, limit, continue_token)
            .await
            .map_err(ApiError::from)?;

        let mut items = Vec::with_capacity(raw.len());
        for bytes in &raw {
            let obj: Value = serde_json::from_slice(bytes)
                .map_err(|e| ApiError::internal(&e.to_string()))?;
            items.push(obj);
        }

        Ok((items, continue_token, revision))
    }

    /// Create a resource (fails if it already exists).
    pub async fn create(&self, key: &str, mut obj: Value) -> Result<Value, ApiError> {
        // Never persist resourceVersion in the stored bytes — it is derived from
        // the store's mod_revision on read (#33). Baking it in makes later reads
        // return a stale RV.
        strip_resource_version(&mut obj);
        let bytes =
            serde_json::to_vec(&obj).map_err(|e| ApiError::internal(&e.to_string()))?;
        // Atomic create-if-not-exists: CAS against revision 0 (the store treats
        // an absent key as revision 0) fails with Conflict if the key exists, so
        // two concurrent creates can't both win.
        let rev = match self.store.put(key, &bytes, Some(0)).await {
            Ok(rev) => rev,
            Err(apimachinery::Error::Conflict) => {
                let name = obj["metadata"]["name"].as_str().unwrap_or("unknown");
                let kind = obj["kind"].as_str().unwrap_or("resource");
                return Err(ApiError::already_exists(kind, name));
            }
            Err(e) => return Err(ApiError::from(e)),
        };

        inject_resource_version(&mut obj, rev);
        Ok(obj)
    }

    /// Update a resource (requires resourceVersion for optimistic concurrency).
    pub async fn update(
        &self,
        key: &str,
        mut obj: Value,
        prev_revision: Option<u64>,
    ) -> Result<Value, ApiError> {
        // Strip the client-supplied resourceVersion from the stored bytes: it is
        // used for the CAS (prev_revision) but must not be baked into storage, or
        // the next read returns a stale RV and optimistic concurrency breaks (#33).
        strip_resource_version(&mut obj);
        let bytes =
            serde_json::to_vec(&obj).map_err(|e| ApiError::internal(&e.to_string()))?;
        let rev = self
            .store
            .put(key, &bytes, prev_revision)
            .await
            .map_err(ApiError::from)?;

        inject_resource_version(&mut obj, rev);
        Ok(obj)
    }

    /// Delete a resource by key.
    pub async fn delete(&self, key: &str, prev_revision: Option<u64>) -> Result<(), ApiError> {
        self.store
            .delete(key, prev_revision)
            .await
            .map_err(ApiError::from)
    }

    /// Watch resources by prefix, served through the shared watch cache (one
    /// upstream store watch per prefix, fanned out in-memory).
    pub async fn watch(
        &self,
        prefix: &str,
        start_revision: u64,
    ) -> Result<apimachinery::store::WatchStream, ApiError> {
        self.watch_cache
            .watch(prefix, start_revision)
            .await
            .map_err(ApiError::from)
    }
}

/// Set `metadata.resourceVersion` to the store revision the object reflects.
fn inject_resource_version(obj: &mut Value, rev: u64) {
    if !obj.get("metadata").map(Value::is_object).unwrap_or(false) {
        obj["metadata"] = serde_json::json!({});
    }
    obj["metadata"]["resourceVersion"] = Value::String(rev.to_string());
}

/// Remove `metadata.resourceVersion` so it is never persisted in the stored
/// bytes (it is always derived from the store's mod_revision on read).
fn strip_resource_version(obj: &mut Value) {
    if let Some(meta) = obj.get_mut("metadata").and_then(|m| m.as_object_mut()) {
        meta.remove("resourceVersion");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inject_overwrites_stale_resource_version() {
        // A read must report the store revision, not a stale baked-in RV (#33).
        let mut obj = serde_json::json!({
            "metadata": { "name": "x", "resourceVersion": "100" }
        });
        inject_resource_version(&mut obj, 237_900);
        assert_eq!(obj["metadata"]["resourceVersion"], "237900");
    }

    #[test]
    fn strip_removes_resource_version_for_storage() {
        let mut obj = serde_json::json!({
            "metadata": { "name": "x", "resourceVersion": "237871", "uid": "u" }
        });
        strip_resource_version(&mut obj);
        assert!(obj["metadata"].get("resourceVersion").is_none());
        // Other metadata is preserved.
        assert_eq!(obj["metadata"]["uid"], "u");
    }

    #[test]
    fn inject_tolerates_missing_metadata() {
        let mut obj = serde_json::json!({ "kind": "Lease" });
        inject_resource_version(&mut obj, 5);
        assert_eq!(obj["metadata"]["resourceVersion"], "5");
    }
}
