//! Generic resource storage layer.
//!
//! Bridges K8s resource CRUD to the KvStore, handling JSON serialization,
//! resourceVersion tracking, key construction, and metadata injection.

use crate::error::ApiError;
use rk_core::store::KvStore;
use serde_json::Value;
use std::sync::Arc;

/// Key prefix for all resources in the store.
const REGISTRY_PREFIX: &str = "/registry";

/// Generic resource storage — handles any K8s resource type.
pub struct ResourceStorage {
    store: Arc<dyn KvStore>,
}

impl ResourceStorage {
    pub fn new(store: Arc<dyn KvStore>) -> Self {
        Self { store }
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
            Some((bytes, _rev)) => {
                let obj: Value = serde_json::from_slice(&bytes)
                    .map_err(|e| ApiError::internal(&e.to_string()))?;
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
        let result = self
            .store
            .list(prefix, limit, continue_token)
            .await
            .map_err(ApiError::from)?;

        let mut items = Vec::with_capacity(result.items.len());
        for (_key, bytes, _rev) in &result.items {
            let obj: Value = serde_json::from_slice(bytes)
                .map_err(|e| ApiError::internal(&e.to_string()))?;
            items.push(obj);
        }

        Ok((items, result.continue_token, result.revision))
    }

    /// Create a resource (fails if it already exists).
    pub async fn create(&self, key: &str, mut obj: Value) -> Result<Value, ApiError> {
        // Check for existing
        if let Some(_) = self.store.get(key).await.map_err(ApiError::from)? {
            let name = obj["metadata"]["name"]
                .as_str()
                .unwrap_or("unknown");
            let kind = obj["kind"].as_str().unwrap_or("resource");
            return Err(ApiError::already_exists(kind, name));
        }

        let bytes =
            serde_json::to_vec(&obj).map_err(|e| ApiError::internal(&e.to_string()))?;
        let rev = self
            .store
            .put(key, &bytes, None)
            .await
            .map_err(ApiError::from)?;

        // Set resourceVersion in the returned object
        if let Some(meta) = obj.get_mut("metadata").and_then(|m| m.as_object_mut()) {
            meta.insert(
                "resourceVersion".into(),
                Value::String(rev.to_string()),
            );
        }
        Ok(obj)
    }

    /// Update a resource (requires resourceVersion for optimistic concurrency).
    pub async fn update(
        &self,
        key: &str,
        mut obj: Value,
        prev_revision: Option<u64>,
    ) -> Result<Value, ApiError> {
        let bytes =
            serde_json::to_vec(&obj).map_err(|e| ApiError::internal(&e.to_string()))?;
        let rev = self
            .store
            .put(key, &bytes, prev_revision)
            .await
            .map_err(ApiError::from)?;

        if let Some(meta) = obj.get_mut("metadata").and_then(|m| m.as_object_mut()) {
            meta.insert(
                "resourceVersion".into(),
                Value::String(rev.to_string()),
            );
        }
        Ok(obj)
    }

    /// Delete a resource by key.
    pub async fn delete(&self, key: &str, prev_revision: Option<u64>) -> Result<(), ApiError> {
        self.store
            .delete(key, prev_revision)
            .await
            .map_err(ApiError::from)
    }

    /// Watch resources by prefix.
    pub async fn watch(
        &self,
        prefix: &str,
        start_revision: u64,
    ) -> Result<rk_core::store::WatchStream, ApiError> {
        self.store
            .watch(prefix, start_revision)
            .await
            .map_err(ApiError::from)
    }
}
