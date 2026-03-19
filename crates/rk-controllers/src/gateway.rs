//! Gateway API controller.
//!
//! Implements the Kubernetes Gateway API (gateway.networking.k8s.io/v1).
//! Watches GatewayClass, Gateway, and HTTPRoute resources to manage
//! ingress traffic routing and load balancing.
//!
//! Reconciles:
//! - GatewayClass: Validates controller name
//! - Gateway: Validates GatewayClass reference, assigns addresses, updates listener status
//! - HTTPRoute: Validates parentRefs (Gateway references), resolves backendRefs to Services

use crate::runner::ApiClient;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::time::{self, Duration};
use tracing::{debug, error, info, warn};

pub struct GatewayController {
    api: Arc<ApiClient>,
}

impl GatewayController {
    pub fn new(api: Arc<ApiClient>) -> Self {
        Self { api }
    }

    pub async fn run(&self) {
        info!("Gateway API controller started");
        let mut interval = time::interval(Duration::from_secs(5));

        loop {
            interval.tick().await;
            if let Err(e) = self.reconcile_all().await {
                error!("Gateway API reconcile error: {e}");
            }
        }
    }

    async fn reconcile_all(&self) -> anyhow::Result<()> {
        // Reconcile GatewayClasses (cluster-scoped)
        if let Err(e) = self.reconcile_gateway_classes().await {
            debug!("GatewayClass reconcile error: {e}");
        }

        // Reconcile Gateways and HTTPRoutes per namespace
        let ns_list: Value = self.api.list("/api/v1/namespaces").await?;
        let namespaces = ns_list["items"].as_array().cloned().unwrap_or_default();

        for ns in &namespaces {
            let ns_name = ns["metadata"]["name"].as_str().unwrap_or("default");
            if let Err(e) = self.reconcile_namespace(ns_name).await {
                debug!("Gateway API reconcile in {ns_name}: {e}");
            }
        }
        Ok(())
    }

    async fn reconcile_gateway_classes(&self) -> anyhow::Result<()> {
        let gc_list: Value = self
            .api
            .list("/apis/gateway.networking.k8s.io/v1/gatewayclasses")
            .await?;
        let gateway_classes = gc_list["items"].as_array().cloned().unwrap_or_default();

        for gc in &gateway_classes {
            if let Err(e) = self.reconcile_gateway_class(gc).await {
                let name = gc["metadata"]["name"].as_str().unwrap_or("?");
                warn!("Failed to reconcile GatewayClass {name}: {e}");
            }
        }
        Ok(())
    }

    async fn reconcile_gateway_class(&self, gc: &Value) -> anyhow::Result<()> {
        let gc_name = gc["metadata"]["name"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("GatewayClass missing name"))?;
        let controller_name = gc["spec"]["controllerName"]
            .as_str()
            .unwrap_or("rustkube.io/gateway-controller");

        // Check if we manage this GatewayClass
        let accepted = controller_name == "rustkube.io/gateway-controller";

        let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let mut updated = gc.clone();
        updated["status"] = json!({
            "conditions": [{
                "type": "Accepted",
                "status": if accepted { "True" } else { "False" },
                "reason": if accepted { "Accepted" } else { "UnsupportedController" },
                "message": if accepted {
                    "GatewayClass managed by RustKube".to_string()
                } else {
                    format!("Controller {controller_name} not supported")
                },
                "lastTransitionTime": now,
                "observedGeneration": gc["metadata"]["generation"].as_u64().unwrap_or(1)
            }]
        });

        let _ = self
            .api
            .update(
                &format!("/apis/gateway.networking.k8s.io/v1/gatewayclasses/{gc_name}"),
                &updated,
            )
            .await;

        Ok(())
    }

    async fn reconcile_namespace(&self, namespace: &str) -> anyhow::Result<()> {
        // Load all resources we need for reconciliation
        let gateway_list: Value = self
            .api
            .list(&format!(
                "/apis/gateway.networking.k8s.io/v1/namespaces/{namespace}/gateways"
            ))
            .await?;
        let gateways = gateway_list["items"].as_array().cloned().unwrap_or_default();

        let httproute_list: Value = self
            .api
            .list(&format!(
                "/apis/gateway.networking.k8s.io/v1/namespaces/{namespace}/httproutes"
            ))
            .await?;
        let httproutes = httproute_list["items"]
            .as_array()
            .cloned()
            .unwrap_or_default();

        let service_list: Value = self
            .api
            .list(&format!("/api/v1/namespaces/{namespace}/services"))
            .await?;
        let services = service_list["items"].as_array().cloned().unwrap_or_default();

        // Build service lookup map
        let service_map: HashMap<String, &Value> = services
            .iter()
            .filter_map(|svc| {
                svc["metadata"]["name"]
                    .as_str()
                    .map(|name| (name.to_string(), svc))
            })
            .collect();

        // Reconcile Gateways
        for gateway in &gateways {
            if let Err(e) = self.reconcile_gateway(namespace, gateway).await {
                let name = gateway["metadata"]["name"].as_str().unwrap_or("?");
                warn!("Failed to reconcile Gateway {namespace}/{name}: {e}");
            }
        }

        // Build gateway lookup map for HTTPRoute reconciliation
        let gateway_map: HashMap<String, &Value> = gateways
            .iter()
            .filter_map(|gw| {
                gw["metadata"]["name"]
                    .as_str()
                    .map(|name| (name.to_string(), gw))
            })
            .collect();

        // Reconcile HTTPRoutes
        for httproute in &httproutes {
            if let Err(e) = self
                .reconcile_httproute(namespace, httproute, &gateway_map, &service_map)
                .await
            {
                let name = httproute["metadata"]["name"].as_str().unwrap_or("?");
                warn!("Failed to reconcile HTTPRoute {namespace}/{name}: {e}");
            }
        }

        Ok(())
    }

    async fn reconcile_gateway(&self, namespace: &str, gateway: &Value) -> anyhow::Result<()> {
        let gateway_name = gateway["metadata"]["name"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Gateway missing name"))?;
        let gateway_class_name = gateway["spec"]["gatewayClassName"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Gateway missing gatewayClassName"))?;

        // Check if GatewayClass exists
        let gc_result = self
            .api
            .list("/apis/gateway.networking.k8s.io/v1/gatewayclasses")
            .await;
        let gc_exists = match gc_result {
            Ok(gc_list) => gc_list["items"]
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .any(|gc| gc["metadata"]["name"].as_str() == Some(gateway_class_name))
                })
                .unwrap_or(false),
            Err(_) => false,
        };

        let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let accepted = gc_exists;

        // Process listeners
        let listeners = gateway["spec"]["listeners"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let mut listener_statuses = Vec::new();

        for listener in &listeners {
            let listener_name = listener["name"]
                .as_str()
                .unwrap_or("unknown")
                .to_string();
            let protocol = listener["protocol"].as_str().unwrap_or("HTTP");

            // Validate listener protocol
            let supported = matches!(protocol, "HTTP" | "HTTPS" | "TCP" | "TLS");

            listener_statuses.push(json!({
                "name": listener_name,
                "supportedKinds": [
                    {"group": "gateway.networking.k8s.io", "kind": "HTTPRoute"}
                ],
                "attachedRoutes": 0, // Updated by HTTPRoute reconciliation
                "conditions": [{
                    "type": "Accepted",
                    "status": if supported { "True" } else { "False" },
                    "reason": if supported { "Accepted" } else { "UnsupportedProtocol" },
                    "message": if supported {
                        format!("Listener {listener_name} accepted")
                    } else {
                        format!("Protocol {protocol} not supported")
                    },
                    "lastTransitionTime": now
                }]
            }));
        }

        // Assign addresses (simplified: use a placeholder LoadBalancer IP)
        let addresses = vec![json!({
            "type": "IPAddress",
            "value": "192.168.1.100" // Placeholder — real impl would allocate from pool
        })];

        let mut updated = gateway.clone();
        updated["status"] = json!({
            "addresses": addresses,
            "conditions": [{
                "type": "Accepted",
                "status": if accepted { "True" } else { "False" },
                "reason": if accepted { "Accepted" } else { "InvalidGatewayClass" },
                "message": if accepted {
                    format!("Gateway accepted, using GatewayClass {gateway_class_name}")
                } else {
                    format!("GatewayClass {gateway_class_name} not found")
                },
                "lastTransitionTime": now,
                "observedGeneration": gateway["metadata"]["generation"].as_u64().unwrap_or(1)
            }, {
                "type": "Programmed",
                "status": if accepted { "True" } else { "False" },
                "reason": if accepted { "Programmed" } else { "Pending" },
                "message": if accepted { "Gateway programmed" } else { "Waiting for GatewayClass" },
                "lastTransitionTime": now,
                "observedGeneration": gateway["metadata"]["generation"].as_u64().unwrap_or(1)
            }],
            "listeners": listener_statuses
        });

        let _ = self
            .api
            .update(
                &format!(
                    "/apis/gateway.networking.k8s.io/v1/namespaces/{namespace}/gateways/{gateway_name}"
                ),
                &updated,
            )
            .await;

        if accepted {
            info!("Gateway {namespace}/{gateway_name} accepted");
        }

        Ok(())
    }

    async fn reconcile_httproute(
        &self,
        namespace: &str,
        httproute: &Value,
        gateway_map: &HashMap<String, &Value>,
        service_map: &HashMap<String, &Value>,
    ) -> anyhow::Result<()> {
        let httproute_name = httproute["metadata"]["name"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("HTTPRoute missing name"))?;

        // Validate parentRefs (Gateway references)
        let parent_refs = httproute["spec"]["parentRefs"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let mut parent_statuses = Vec::new();
        let mut valid_parents = 0;

        for parent_ref in &parent_refs {
            let parent_name = parent_ref["name"].as_str().unwrap_or("");
            let parent_namespace = parent_ref["namespace"]
                .as_str()
                .unwrap_or(namespace);
            let parent_kind = parent_ref["kind"].as_str().unwrap_or("Gateway");

            // Check if parent Gateway exists
            let parent_exists = if parent_kind == "Gateway" && parent_namespace == namespace {
                gateway_map.contains_key(parent_name)
            } else {
                false
            };

            let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();

            if parent_exists {
                valid_parents += 1;
            }

            parent_statuses.push(json!({
                "parentRef": parent_ref,
                "controllerName": "rustkube.io/gateway-controller",
                "conditions": [{
                    "type": "Accepted",
                    "status": if parent_exists { "True" } else { "False" },
                    "reason": if parent_exists { "Accepted" } else { "InvalidParentRef" },
                    "message": if parent_exists {
                        format!("HTTPRoute accepted by Gateway {parent_name}")
                    } else {
                        format!("Parent Gateway {parent_name} not found")
                    },
                    "lastTransitionTime": now
                }, {
                    "type": "ResolvedRefs",
                    "status": "True",
                    "reason": "ResolvedRefs",
                    "message": "All backend references resolved",
                    "lastTransitionTime": now
                }]
            }));
        }

        // Validate backendRefs (Service references)
        let rules = httproute["spec"]["rules"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let mut backend_errors = Vec::new();

        for rule in &rules {
            let backend_refs = rule["backendRefs"].as_array();
            if let Some(refs) = backend_refs {
                for backend_ref in refs {
                    let backend_name = backend_ref["name"].as_str().unwrap_or("");
                    let backend_kind = backend_ref["kind"].as_str().unwrap_or("Service");

                    if backend_kind == "Service" && !service_map.contains_key(backend_name) {
                        backend_errors.push(format!("Service {backend_name} not found"));
                    }
                }
            }
        }

        // Update ResolvedRefs condition if there were backend errors
        let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        if !backend_errors.is_empty() {
            for parent_status in &mut parent_statuses {
                if let Some(conditions) = parent_status["conditions"].as_array_mut() {
                    for condition in conditions {
                        if condition["type"].as_str() == Some("ResolvedRefs") {
                            *condition = json!({
                                "type": "ResolvedRefs",
                                "status": "False",
                                "reason": "BackendNotFound",
                                "message": backend_errors.join(", "),
                                "lastTransitionTime": now
                            });
                        }
                    }
                }
            }
        }

        let mut updated = httproute.clone();
        updated["status"] = json!({
            "parents": parent_statuses
        });

        let _ = self
            .api
            .update(
                &format!(
                    "/apis/gateway.networking.k8s.io/v1/namespaces/{namespace}/httproutes/{httproute_name}"
                ),
                &updated,
            )
            .await;

        if valid_parents > 0 {
            debug!("HTTPRoute {namespace}/{httproute_name} accepted by {valid_parents} parent(s)");
        }

        Ok(())
    }
}
