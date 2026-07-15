//! Admission webhook support (mutating + validating)
//!
//! Implements K8s admission control chain:
//! 1. Mutating webhooks (can modify objects)
//! 2. Validating webhooks (can accept/reject)
//!
//! Webhooks are configured via MutatingWebhookConfiguration and ValidatingWebhookConfiguration
//! resources. Each webhook has rules for matching resources and operations.

use crate::error::ApiError;
use crate::storage::ResourceStorage;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::Duration;
use tracing::{debug, warn};

/// Admission review request/response envelope (K8s admission.k8s.io/v1)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdmissionReview {
    pub api_version: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request: Option<AdmissionRequest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response: Option<AdmissionResponse>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdmissionRequest {
    pub uid: String,
    pub kind: GroupVersionKind,
    pub resource: GroupVersionResource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sub_resource: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_kind: Option<GroupVersionKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_resource: Option<GroupVersionResource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_sub_resource: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    pub operation: String,
    pub user_info: UserInfo,
    pub object: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_object: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dry_run: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupVersionKind {
    pub group: String,
    pub version: String,
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupVersionResource {
    pub group: String,
    pub version: String,
    pub resource: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserInfo {
    pub username: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uid: Option<String>,
    #[serde(default)]
    pub groups: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdmissionResponse {
    pub uid: String,
    pub allowed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<ResponseStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patch: Option<String>, // base64-encoded JSON patch
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patch_type: Option<String>, // "JSONPatch"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audit_annotations: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warnings: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseStatus {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<i32>,
}

/// Webhook configuration (parsed from MutatingWebhookConfiguration or ValidatingWebhookConfiguration)
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct WebhookConfig {
    pub name: String,
    pub client_config: ClientConfig,
    pub rules: Vec<RuleWithOperations>,
    pub failure_policy: FailurePolicy,
    pub timeout_seconds: Option<i32>,
    pub namespace_selector: Option<Value>,
    pub object_selector: Option<Value>,
    pub side_effects: Option<String>,
    pub admission_review_versions: Vec<String>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ClientConfig {
    pub url: Option<String>,
    pub service: Option<ServiceReference>,
    pub ca_bundle: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ServiceReference {
    pub namespace: String,
    pub name: String,
    pub path: Option<String>,
    pub port: Option<i32>,
}

#[derive(Debug, Clone)]
pub struct RuleWithOperations {
    pub operations: Vec<String>, // CREATE, UPDATE, DELETE, CONNECT
    pub api_groups: Vec<String>,
    pub api_versions: Vec<String>,
    pub resources: Vec<String>,
    pub scope: Option<String>, // Cluster, Namespaced, *
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailurePolicy {
    Ignore,
    Fail,
}

/// Admission webhook chain (mutating + validating)
pub struct AdmissionChain {
    mutating: Vec<WebhookConfig>,
    validating: Vec<WebhookConfig>,
    client: reqwest::Client,
}

impl AdmissionChain {
    /// Create a new empty admission chain with default HTTP client
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        Self {
            mutating: Vec::new(),
            validating: Vec::new(),
            client,
        }
    }

    /// Load webhooks from storage
    pub async fn load_webhooks(storage: &ResourceStorage) -> Self {
        let mut chain = Self::new();

        // Load mutating webhooks
        let prefix = ResourceStorage::cluster_prefix("mutatingwebhookconfigurations");
        match storage.list(&prefix, 500, None).await {
            Ok((configs, _, _)) => {
                for config in &configs {
                    if let Some(webhooks) = parse_mutating_webhooks(config) {
                        chain.mutating.extend(webhooks);
                    }
                }
                debug!("Loaded {} mutating webhooks", chain.mutating.len());
            }
            Err(e) => {
                warn!("Failed to load mutating webhooks: {}", e);
            }
        }

        // Load validating webhooks
        let prefix = ResourceStorage::cluster_prefix("validatingwebhookconfigurations");
        match storage.list(&prefix, 500, None).await {
            Ok((configs, _, _)) => {
                for config in &configs {
                    if let Some(webhooks) = parse_validating_webhooks(config) {
                        chain.validating.extend(webhooks);
                    }
                }
                debug!("Loaded {} validating webhooks", chain.validating.len());
            }
            Err(e) => {
                warn!("Failed to load validating webhooks: {}", e);
            }
        }

        chain
    }

    /// Run mutating webhooks against an object
    ///
    /// Applies patches from webhooks in order. Returns error if any webhook
    /// fails and has failurePolicy=Fail.
    pub async fn run_mutating(
        &self,
        obj: &mut Value,
        kind: &str,
        operation: &str,
        namespace: Option<&str>,
    ) -> Result<(), ApiError> {
        for webhook in &self.mutating {
            if !matches_rules(&webhook.rules, kind, operation) {
                continue;
            }

            debug!(
                "Running mutating webhook: {} for {}/{}",
                webhook.name, kind, operation
            );

            match self.call_webhook(webhook, obj, None, kind, operation, namespace).await {
                Ok(response) => {
                    if !response.allowed {
                        let msg = response
                            .status
                            .and_then(|s| s.message)
                            .unwrap_or_else(|| "Webhook denied request".to_string());
                        return Err(ApiError::forbidden(&msg));
                    }

                    // Apply JSON patch if provided
                    if let Some(patch_b64) = response.patch {
                        if response.patch_type.as_deref() == Some("JSONPatch") {
                            if let Err(e) = apply_json_patch(obj, &patch_b64) {
                                warn!("Failed to apply patch from webhook {}: {}", webhook.name, e);
                                if webhook.failure_policy == FailurePolicy::Fail {
                                    return Err(ApiError::internal(&format!(
                                        "Webhook patch failed: {}",
                                        e
                                    )));
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!("Mutating webhook {} failed: {}", webhook.name, e);
                    if webhook.failure_policy == FailurePolicy::Fail {
                        return Err(e);
                    }
                    // Ignore failure and continue
                }
            }
        }

        Ok(())
    }

    /// Run validating webhooks against an object
    ///
    /// Returns error if any webhook rejects the object or fails with failurePolicy=Fail.
    pub async fn run_validating(
        &self,
        obj: &Value,
        kind: &str,
        operation: &str,
        namespace: Option<&str>,
    ) -> Result<(), ApiError> {
        for webhook in &self.validating {
            if !matches_rules(&webhook.rules, kind, operation) {
                continue;
            }

            debug!(
                "Running validating webhook: {} for {}/{}",
                webhook.name, kind, operation
            );

            match self.call_webhook(webhook, obj, None, kind, operation, namespace).await {
                Ok(response) => {
                    if !response.allowed {
                        let msg = response
                            .status
                            .and_then(|s| s.message)
                            .unwrap_or_else(|| "Webhook denied request".to_string());
                        return Err(ApiError::forbidden(&msg));
                    }
                }
                Err(e) => {
                    warn!("Validating webhook {} failed: {}", webhook.name, e);
                    if webhook.failure_policy == FailurePolicy::Fail {
                        return Err(e);
                    }
                    // Ignore failure and continue
                }
            }
        }

        Ok(())
    }

    /// Call a webhook via HTTP POST
    async fn call_webhook(
        &self,
        webhook: &WebhookConfig,
        obj: &Value,
        old_obj: Option<&Value>,
        kind: &str,
        operation: &str,
        namespace: Option<&str>,
    ) -> Result<AdmissionResponse, ApiError> {
        let url = webhook
            .client_config
            .url
            .as_ref()
            .ok_or_else(|| ApiError::internal("Webhook URL not configured"))?;

        let request = AdmissionRequest {
            uid: uuid::Uuid::new_v4().to_string(),
            kind: GroupVersionKind {
                group: "".to_string(), // TODO: parse from kind
                version: "v1".to_string(),
                kind: kind.to_string(),
            },
            resource: GroupVersionResource {
                group: "".to_string(),
                version: "v1".to_string(),
                resource: kind.to_lowercase() + "s",
            },
            sub_resource: None,
            request_kind: None,
            request_resource: None,
            request_sub_resource: None,
            name: obj.get("metadata").and_then(|m| m.get("name")).and_then(|n| n.as_str()).map(|s| s.to_string()),
            namespace: namespace.map(|s| s.to_string()),
            operation: operation.to_uppercase(),
            user_info: UserInfo {
                username: "system:admin".to_string(), // TODO: pass real user
                uid: None,
                groups: vec!["system:masters".to_string()],
                extra: None,
            },
            object: obj.clone(),
            old_object: old_obj.cloned(),
            dry_run: None,
            options: None,
        };

        let review = AdmissionReview {
            api_version: "admission.k8s.io/v1".to_string(),
            kind: "AdmissionReview".to_string(),
            request: Some(request),
            response: None,
        };

        let timeout = webhook
            .timeout_seconds
            .map(|s| Duration::from_secs(s as u64))
            .unwrap_or(Duration::from_secs(10));

        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .unwrap_or_else(|_| self.client.clone());

        let resp = client
            .post(url)
            .json(&review)
            .send()
            .await
            .map_err(|e| ApiError::internal(&format!("Webhook request failed: {}", e)))?;

        if !resp.status().is_success() {
            return Err(ApiError::internal(&format!(
                "Webhook returned status {}",
                resp.status()
            )));
        }

        let review_resp: AdmissionReview = resp
            .json()
            .await
            .map_err(|e| ApiError::internal(&format!("Failed to parse webhook response: {}", e)))?;

        review_resp
            .response
            .ok_or_else(|| ApiError::internal("Webhook response missing"))
    }
}

impl Default for AdmissionChain {
    fn default() -> Self {
        Self::new()
    }
}

/// Check if webhook rules match the given resource and operation
fn matches_rules(rules: &[RuleWithOperations], kind: &str, operation: &str) -> bool {
    if rules.is_empty() {
        return true; // No rules = match all
    }

    for rule in rules {
        // Check operation
        if !rule.operations.is_empty()
            && !rule.operations.iter().any(|op| op == "*" || op.eq_ignore_ascii_case(operation))
        {
            continue;
        }

        // Check resource (simplistic matching for now)
        let resource_name = kind.to_lowercase() + "s";
        if !rule.resources.is_empty()
            && !rule
                .resources
                .iter()
                .any(|r| r == "*" || r.eq_ignore_ascii_case(&resource_name))
        {
            continue;
        }

        // Match found
        return true;
    }

    false
}

/// Apply a base64-encoded JSON patch to an object
fn apply_json_patch(obj: &mut Value, patch_b64: &str) -> Result<(), String> {
    let patch_bytes = {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD
            .decode(patch_b64)
            .map_err(|e| format!("Invalid base64: {}", e))?
    };

    let patch_ops: Vec<Value> =
        serde_json::from_slice(&patch_bytes).map_err(|e| format!("Invalid JSON patch: {}", e))?;

    for op in patch_ops {
        apply_patch_operation(obj, &op)?;
    }

    Ok(())
}

/// Apply a single JSON patch operation
fn apply_patch_operation(obj: &mut Value, op: &Value) -> Result<(), String> {
    let op_type = op
        .get("op")
        .and_then(|v| v.as_str())
        .ok_or("Missing op field")?;
    let path = op
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or("Missing path field")?;

    match op_type {
        "add" | "replace" => {
            let value = op.get("value").ok_or("Missing value field")?.clone();
            set_json_pointer(obj, path, value)?;
        }
        "remove" => {
            remove_json_pointer(obj, path)?;
        }
        _ => {
            // copy, move, test not implemented yet
            warn!("Unsupported patch operation: {}", op_type);
        }
    }

    Ok(())
}

/// Set a value at a JSON pointer path
fn set_json_pointer(obj: &mut Value, path: &str, value: Value) -> Result<(), String> {
    if path.is_empty() {
        *obj = value;
        return Ok(());
    }

    let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    let mut current = obj;

    for (i, part) in parts.iter().enumerate() {
        let is_last = i == parts.len() - 1;

        if is_last {
            if let Some(obj_map) = current.as_object_mut() {
                obj_map.insert(part.to_string(), value.clone());
            } else if let Some(arr) = current.as_array_mut() {
                if *part == "-" {
                    arr.push(value.clone());
                } else if let Ok(idx) = part.parse::<usize>() {
                    if idx <= arr.len() {
                        arr.insert(idx, value.clone());
                    } else {
                        return Err(format!("Array index out of bounds: {}", idx));
                    }
                } else {
                    return Err(format!("Invalid array index: {}", part));
                }
            } else {
                return Err("Path does not exist".to_string());
            }
        } else if let Some(obj_map) = current.as_object_mut() {
            current = obj_map
                .entry(part.to_string())
                .or_insert_with(|| json!({}));
        } else {
            return Err("Path does not exist".to_string());
        }
    }

    Ok(())
}

/// Remove a value at a JSON pointer path
fn remove_json_pointer(obj: &mut Value, path: &str) -> Result<(), String> {
    if path.is_empty() {
        return Err("Cannot remove root".to_string());
    }

    let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    let mut current = obj;

    for (i, part) in parts.iter().enumerate() {
        let is_last = i == parts.len() - 1;

        if is_last {
            if let Some(obj_map) = current.as_object_mut() {
                obj_map.remove(*part);
            } else if let Some(arr) = current.as_array_mut() {
                if let Ok(idx) = part.parse::<usize>() {
                    if idx < arr.len() {
                        arr.remove(idx);
                    } else {
                        return Err(format!("Array index out of bounds: {}", idx));
                    }
                } else {
                    return Err(format!("Invalid array index: {}", part));
                }
            } else {
                return Err("Path does not exist".to_string());
            }
            return Ok(());
        } else if let Some(obj_map) = current.as_object_mut() {
            current = obj_map
                .get_mut(*part)
                .ok_or("Path does not exist")?;
        } else {
            return Err("Path does not exist".to_string());
        }
    }

    Ok(())
}

/// Parse mutating webhooks from a MutatingWebhookConfiguration resource
fn parse_mutating_webhooks(config: &Value) -> Option<Vec<WebhookConfig>> {
    let webhooks = config.get("webhooks")?.as_array()?;
    let mut result = Vec::new();

    for webhook in webhooks {
        if let Some(wh) = parse_webhook(webhook) {
            result.push(wh);
        }
    }

    Some(result)
}

/// Parse validating webhooks from a ValidatingWebhookConfiguration resource
fn parse_validating_webhooks(config: &Value) -> Option<Vec<WebhookConfig>> {
    let webhooks = config.get("webhooks")?.as_array()?;
    let mut result = Vec::new();

    for webhook in webhooks {
        if let Some(wh) = parse_webhook(webhook) {
            result.push(wh);
        }
    }

    Some(result)
}

/// Parse a single webhook from configuration
fn parse_webhook(webhook: &Value) -> Option<WebhookConfig> {
    let name = webhook.get("name")?.as_str()?.to_string();

    let client_config = webhook.get("clientConfig")?;
    let url = client_config.get("url").and_then(|u| u.as_str()).map(|s| s.to_string());
    let service = client_config.get("service").and_then(|s| {
        Some(ServiceReference {
            namespace: s.get("namespace")?.as_str()?.to_string(),
            name: s.get("name")?.as_str()?.to_string(),
            path: s.get("path").and_then(|p| p.as_str()).map(|s| s.to_string()),
            port: s.get("port").and_then(|p| p.as_i64()).map(|p| p as i32),
        })
    });

    let failure_policy = webhook
        .get("failurePolicy")
        .and_then(|f| f.as_str())
        .map(|s| {
            if s.eq_ignore_ascii_case("Ignore") {
                FailurePolicy::Ignore
            } else {
                FailurePolicy::Fail
            }
        })
        .unwrap_or(FailurePolicy::Fail);

    let timeout_seconds = webhook
        .get("timeoutSeconds")
        .and_then(|t| t.as_i64())
        .map(|t| t as i32);

    let rules = webhook
        .get("rules")
        .and_then(|r| r.as_array())
        .map(|rules_arr| {
            rules_arr
                .iter()
                .filter_map(|rule| {
                    Some(RuleWithOperations {
                        operations: rule
                            .get("operations")?
                            .as_array()?
                            .iter()
                            .filter_map(|o| o.as_str().map(|s| s.to_string()))
                            .collect(),
                        api_groups: rule
                            .get("apiGroups")
                            .and_then(|g| g.as_array())
                            .map(|arr| {
                                arr.iter()
                                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                                    .collect()
                            })
                            .unwrap_or_default(),
                        api_versions: rule
                            .get("apiVersions")
                            .and_then(|v| v.as_array())
                            .map(|arr| {
                                arr.iter()
                                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                                    .collect()
                            })
                            .unwrap_or_default(),
                        resources: rule
                            .get("resources")?
                            .as_array()?
                            .iter()
                            .filter_map(|r| r.as_str().map(|s| s.to_string()))
                            .collect(),
                        scope: rule.get("scope").and_then(|s| s.as_str()).map(|s| s.to_string()),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let admission_review_versions = webhook
        .get("admissionReviewVersions")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_else(|| vec!["v1".to_string()]);

    Some(WebhookConfig {
        name,
        client_config: ClientConfig {
            url,
            service,
            ca_bundle: None, // TODO: parse caBundle
        },
        rules,
        failure_policy,
        timeout_seconds,
        namespace_selector: webhook.get("namespaceSelector").cloned(),
        object_selector: webhook.get("objectSelector").cloned(),
        side_effects: webhook.get("sideEffects").and_then(|s| s.as_str()).map(|s| s.to_string()),
        admission_review_versions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_json_patch_add() {
        let mut obj = json!({
            "metadata": {
                "name": "test"
            }
        });

        set_json_pointer(&mut obj, "/metadata/labels", json!({"app": "test"})).unwrap();

        assert_eq!(
            obj.get("metadata")
                .unwrap()
                .get("labels")
                .unwrap()
                .get("app")
                .unwrap()
                .as_str()
                .unwrap(),
            "test"
        );
    }

    #[test]
    fn test_json_patch_remove() {
        let mut obj = json!({
            "metadata": {
                "name": "test",
                "labels": {
                    "app": "test"
                }
            }
        });

        remove_json_pointer(&mut obj, "/metadata/labels/app").unwrap();

        assert!(obj
            .get("metadata")
            .unwrap()
            .get("labels")
            .unwrap()
            .get("app")
            .is_none());
    }

    #[test]
    fn test_matches_rules() {
        let rules = vec![RuleWithOperations {
            operations: vec!["CREATE".to_string(), "UPDATE".to_string()],
            api_groups: vec!["".to_string()],
            api_versions: vec!["v1".to_string()],
            resources: vec!["pods".to_string()],
            scope: None,
        }];

        assert!(matches_rules(&rules, "Pod", "CREATE"));
        assert!(matches_rules(&rules, "Pod", "UPDATE"));
        assert!(!matches_rules(&rules, "Pod", "DELETE"));
        assert!(!matches_rules(&rules, "Service", "CREATE"));
    }
}
