//! API aggregation layer.
//!
//! Allows external API servers to register and serve their APIs
//! through the main API server via APIService resources.

use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

/// Registry for tracking aggregated API services.
pub struct ApiServiceRegistry {
    /// group/version → APIService info
    services: Arc<RwLock<HashMap<String, ApiServiceEntry>>>,
}

/// Information about a registered external API service.
#[derive(Clone, Debug)]
pub struct ApiServiceEntry {
    pub group: String,
    pub version: String,
    pub service_name: String,
    pub service_namespace: String,
    pub service_port: u16,
    pub ca_bundle: Option<Vec<u8>>,
    pub priority: i32,
    pub available: bool,
}

impl ApiServiceRegistry {
    /// Creates a new empty API service registry.
    pub fn new() -> Self {
        Self {
            services: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Registers an API service from a K8s APIService resource.
    ///
    /// Expected format:
    /// ```json
    /// {
    ///   "apiVersion": "apiregistration.k8s.io/v1",
    ///   "kind": "APIService",
    ///   "metadata": {
    ///     "name": "v1beta1.metrics.k8s.io"
    ///   },
    ///   "spec": {
    ///     "group": "metrics.k8s.io",
    ///     "version": "v1beta1",
    ///     "service": {
    ///       "name": "metrics-server",
    ///       "namespace": "kube-system",
    ///       "port": 443
    ///     },
    ///     "caBundle": "base64-encoded-ca-cert",
    ///     "groupPriorityMinimum": 100,
    ///     "versionPriority": 100
    ///   },
    ///   "status": {
    ///     "conditions": [
    ///       {"type": "Available", "status": "True"}
    ///     ]
    ///   }
    /// }
    /// ```
    pub async fn register(&self, api_service: &Value) {
        let spec = match api_service.get("spec") {
            Some(s) => s,
            None => {
                warn!("APIService missing spec field");
                return;
            }
        };

        let group = spec
            .get("group")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let version = spec
            .get("version")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        if group.is_empty() || version.is_empty() {
            warn!("APIService missing group or version");
            return;
        }

        let service = match spec.get("service") {
            Some(s) => s,
            None => {
                // Local APIService (no external service) - skip registration
                debug!("APIService {}/{} is local, skipping registration", group, version);
                return;
            }
        };

        let service_name = service
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let service_namespace = service
            .get("namespace")
            .and_then(|v| v.as_str())
            .unwrap_or("default")
            .to_string();
        let service_port = service
            .get("port")
            .and_then(|v| v.as_u64())
            .unwrap_or(443) as u16;

        let ca_bundle = spec
            .get("caBundle")
            .and_then(|v| v.as_str())
            .and_then(|s| {
                use base64::Engine;
                base64::engine::general_purpose::STANDARD.decode(s).ok()
            });

        let priority = spec
            .get("groupPriorityMinimum")
            .and_then(|v| v.as_i64())
            .unwrap_or(100) as i32;

        // Check availability status
        let available = api_service
            .get("status")
            .and_then(|s| s.get("conditions"))
            .and_then(|c| c.as_array())
            .map(|conditions| {
                conditions.iter().any(|cond| {
                    cond.get("type")
                        .and_then(|t| t.as_str())
                        .map(|t| t == "Available")
                        .unwrap_or(false)
                        && cond
                            .get("status")
                            .and_then(|s| s.as_str())
                            .map(|s| s == "True")
                            .unwrap_or(false)
                })
            })
            .unwrap_or(true); // Default to available if no status

        let entry = ApiServiceEntry {
            group: group.clone(),
            version: version.clone(),
            service_name,
            service_namespace,
            service_port,
            ca_bundle,
            priority,
            available,
        };

        let key = format!("{}/{}", group, version);
        let mut services = self.services.write().await;
        services.insert(key.clone(), entry);

        info!("Registered API service: {}", key);
    }

    /// Unregisters an API service by name.
    ///
    /// The name format is typically "version.group" (e.g., "v1beta1.metrics.k8s.io").
    pub async fn unregister(&self, name: &str) {
        // Parse name into group/version
        // Format is usually "version.group"
        let parts: Vec<&str> = name.splitn(2, '.').collect();
        if parts.len() != 2 {
            warn!("Invalid APIService name format: {}", name);
            return;
        }

        let version = parts[0];
        let group = parts[1];
        let key = format!("{}/{}", group, version);

        let mut services = self.services.write().await;
        if services.remove(&key).is_some() {
            info!("Unregistered API service: {}", key);
        } else {
            debug!("API service not found for unregister: {}", key);
        }
    }

    /// Looks up an API service by group and version.
    pub async fn lookup(&self, group: &str, version: &str) -> Option<ApiServiceEntry> {
        let key = format!("{}/{}", group, version);
        let services = self.services.read().await;
        services.get(&key).cloned()
    }

    /// Returns true if the group/version is handled locally (not aggregated).
    ///
    /// A group/version is local if:
    /// - It's not registered in the aggregation layer
    /// - It's registered but marked as unavailable
    pub async fn is_local(&self, group: &str, version: &str) -> bool {
        match self.lookup(group, version).await {
            None => true,
            Some(entry) => !entry.available,
        }
    }

    /// Lists all registered API services.
    pub async fn list_services(&self) -> Vec<ApiServiceEntry> {
        let services = self.services.read().await;
        services.values().cloned().collect()
    }

    /// Loads APIService resources from storage and registers them.
    pub async fn load_from_storage(
        storage: &crate::storage::ResourceStorage,
    ) -> anyhow::Result<Self> {
        let registry = Self::new();

        // List all APIService resources
        let prefix = crate::storage::ResourceStorage::cluster_prefix("apiservices");
        match storage.list(&prefix, 500, None).await {
            Ok((items, _, _)) => {
                for item in &items {
                    registry.register(item).await;
                }
                info!("Loaded {} API services from storage", registry.services.read().await.len());
            }
            Err(e) => {
                tracing::debug!("No API services in storage yet: {}", e);
            }
        }

        Ok(registry)
    }
}

impl Default for ApiServiceRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Proxies a request to an aggregated API server.
///
/// This function forwards the request to the external API server specified
/// in the APIService configuration.
pub async fn proxy_to_aggregated(
    service_entry: &ApiServiceEntry,
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> Result<Value, crate::error::ApiError> {
    if !service_entry.available {
        return Err(crate::error::ApiError::internal(&format!(
            "API service {}/{} is not available",
            service_entry.group, service_entry.version
        )));
    }

    // Construct target URL
    // In a real cluster, this would resolve via DNS to the ClusterIP
    let scheme = if service_entry.service_port == 443 { "https" } else { "http" };
    let target_url = format!(
        "{}://{}.{}.svc.cluster.local:{}{}",
        scheme,
        service_entry.service_name,
        service_entry.service_namespace,
        service_entry.service_port,
        path
    );

    debug!("Proxying {} {} to aggregated API server", method, target_url);

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .danger_accept_invalid_certs(true) // TODO: use caBundle for verification
        .build()
        .map_err(|e| crate::error::ApiError::internal(&format!("Failed to build HTTP client: {}", e)))?;

    let mut request = match method.to_uppercase().as_str() {
        "GET" => client.get(&target_url),
        "POST" => client.post(&target_url),
        "PUT" => client.put(&target_url),
        "PATCH" => client.patch(&target_url),
        "DELETE" => client.delete(&target_url),
        _ => {
            return Err(crate::error::ApiError::invalid(&format!(
                "Unsupported HTTP method: {}",
                method
            )))
        }
    };

    if let Some(b) = body {
        request = request.json(b);
    }

    let response = request
        .send()
        .await
        .map_err(|e| crate::error::ApiError::internal(&format!("Failed to proxy request: {}", e)))?;

    let status = response.status();
    let response_body = response
        .text()
        .await
        .map_err(|e| crate::error::ApiError::internal(&format!("Failed to read response: {}", e)))?;

    if !status.is_success() {
        // Try to parse as JSON error, fallback to plain text
        if let Ok(error_json) = serde_json::from_str::<Value>(&response_body) {
            return Err(crate::error::ApiError::internal(&format!(
                "Aggregated API server returned error: {}",
                error_json
            )));
        } else {
            return Err(crate::error::ApiError::internal(&format!(
                "Aggregated API server returned error (status {}): {}",
                status, response_body
            )));
        }
    }

    // Parse response as JSON
    serde_json::from_str(&response_body).map_err(|e| {
        crate::error::ApiError::internal(&format!("Failed to parse response as JSON: {}", e))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_api_service(group: &str, version: &str, available: bool) -> Value {
        json!({
            "apiVersion": "apiregistration.k8s.io/v1",
            "kind": "APIService",
            "metadata": {
                "name": format!("{}.{}", version, group)
            },
            "spec": {
                "group": group,
                "version": version,
                "service": {
                    "name": "test-service",
                    "namespace": "test-ns",
                    "port": 443
                },
                "groupPriorityMinimum": 100,
                "versionPriority": 100
            },
            "status": {
                "conditions": [
                    {
                        "type": "Available",
                        "status": if available { "True" } else { "False" }
                    }
                ]
            }
        })
    }

    fn local_api_service(group: &str, version: &str) -> Value {
        json!({
            "apiVersion": "apiregistration.k8s.io/v1",
            "kind": "APIService",
            "metadata": {
                "name": format!("{}.{}", version, group)
            },
            "spec": {
                "group": group,
                "version": version,
                "groupPriorityMinimum": 1000,
                "versionPriority": 100
            }
        })
    }

    #[tokio::test]
    async fn test_register_and_lookup() {
        let registry = ApiServiceRegistry::new();
        let api_service = sample_api_service("metrics.k8s.io", "v1beta1", true);

        registry.register(&api_service).await;

        let entry = registry
            .lookup("metrics.k8s.io", "v1beta1")
            .await
            .expect("API service should be registered");

        assert_eq!(entry.group, "metrics.k8s.io");
        assert_eq!(entry.version, "v1beta1");
        assert_eq!(entry.service_name, "test-service");
        assert_eq!(entry.service_namespace, "test-ns");
        assert_eq!(entry.service_port, 443);
        assert!(entry.available);
    }

    #[tokio::test]
    async fn test_unregister() {
        let registry = ApiServiceRegistry::new();
        let api_service = sample_api_service("metrics.k8s.io", "v1beta1", true);

        registry.register(&api_service).await;
        assert!(registry.lookup("metrics.k8s.io", "v1beta1").await.is_some());

        registry.unregister("v1beta1.metrics.k8s.io").await;
        assert!(registry.lookup("metrics.k8s.io", "v1beta1").await.is_none());
    }

    #[tokio::test]
    async fn test_is_local() {
        let registry = ApiServiceRegistry::new();
        let external_service = sample_api_service("metrics.k8s.io", "v1beta1", true);
        let unavailable_service = sample_api_service("custom.io", "v1", false);

        // Not registered - should be local
        assert!(registry.is_local("apps", "v1").await);

        // External service - should not be local
        registry.register(&external_service).await;
        assert!(!registry.is_local("metrics.k8s.io", "v1beta1").await);

        // Unavailable service - should be local
        registry.register(&unavailable_service).await;
        assert!(registry.is_local("custom.io", "v1").await);
    }

    #[tokio::test]
    async fn test_list_services() {
        let registry = ApiServiceRegistry::new();

        let service1 = sample_api_service("metrics.k8s.io", "v1beta1", true);
        let service2 = sample_api_service("custom.io", "v1", true);

        registry.register(&service1).await;
        registry.register(&service2).await;

        let services = registry.list_services().await;
        assert_eq!(services.len(), 2);

        let groups: Vec<String> = services.iter().map(|s| s.group.clone()).collect();
        assert!(groups.contains(&"metrics.k8s.io".to_string()));
        assert!(groups.contains(&"custom.io".to_string()));
    }

    #[tokio::test]
    async fn test_local_api_service_not_registered() {
        let registry = ApiServiceRegistry::new();
        let local_service = local_api_service("", "v1");

        registry.register(&local_service).await;

        // Local services (no service spec) should not be registered
        let services = registry.list_services().await;
        assert_eq!(services.len(), 0);
    }

    #[tokio::test]
    async fn test_missing_fields() {
        let registry = ApiServiceRegistry::new();

        // Missing spec
        let invalid = json!({
            "apiVersion": "apiregistration.k8s.io/v1",
            "kind": "APIService",
            "metadata": {"name": "test"}
        });
        registry.register(&invalid).await;
        assert_eq!(registry.list_services().await.len(), 0);

        // Missing group
        let invalid = json!({
            "apiVersion": "apiregistration.k8s.io/v1",
            "kind": "APIService",
            "metadata": {"name": "test"},
            "spec": {
                "version": "v1",
                "service": {"name": "test", "namespace": "default"}
            }
        });
        registry.register(&invalid).await;
        assert_eq!(registry.list_services().await.len(), 0);
    }

    #[tokio::test]
    async fn test_priority_and_ca_bundle() {
        let registry = ApiServiceRegistry::new();
        let api_service = json!({
            "apiVersion": "apiregistration.k8s.io/v1",
            "kind": "APIService",
            "metadata": {"name": "v1.custom.io"},
            "spec": {
                "group": "custom.io",
                "version": "v1",
                "service": {"name": "svc", "namespace": "ns", "port": 8443},
                "caBundle": "dGVzdA==",  // "test" in base64
                "groupPriorityMinimum": 200
            },
            "status": {
                "conditions": [{"type": "Available", "status": "True"}]
            }
        });

        registry.register(&api_service).await;

        let entry = registry
            .lookup("custom.io", "v1")
            .await
            .expect("Service should be registered");

        assert_eq!(entry.priority, 200);
        assert_eq!(entry.service_port, 8443);
        assert!(entry.ca_bundle.is_some());
        assert_eq!(entry.ca_bundle.unwrap(), b"test");
    }
}
