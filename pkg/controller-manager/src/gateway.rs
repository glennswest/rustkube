//! Gateway API controller.
//!
//! A **status-only** reconciler for the Gateway API
//! (gateway.networking.k8s.io/v1): it periodically lists GatewayClass,
//! Gateway and HTTPRoute objects and writes their conditions. It programs no
//! proxy, so no traffic is routed (#70, #91). Execution uses indexed object workers.
//!
//! It acts only on what is its own, as upstream controllers do (#91): a
//! GatewayClass whose `controllerName` is [`CONTROLLER`], the Gateways of
//! such a class, and its own entries in an HTTPRoute's `status.parents`.
//! Classes, Gateways and route entries of other controllers (Cilium's, say)
//! are left alone.
//!
//! - GatewayClass: `Accepted=True`
//! - Gateway: `Accepted=True`, listeners' `Accepted` by protocol, and
//!   `Programmed=False` (`Pending`) with no `status.addresses` — there is no
//!   data plane, so it neither listens nor has an address (#70)
//! - HTTPRoute: per parent Gateway of an own class, `Accepted` and
//!   `ResolvedRefs` (backend Services exist); other controllers' parents kept

use crate::owned::{self, Controller, Dependency, Deps};
use crate::runner::ApiClient;
use apimachinery::informer::Index;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tracing::{debug, info};

/// The `controllerName` this controller answers to.
pub const CONTROLLER: &str = "rustkube.io/gateway-controller";

/// Whether a GatewayClass is this controller's. One without a
/// `controllerName` is nobody's (the field is required).
fn owns(class: &Value) -> bool {
    class["spec"]["controllerName"].as_str() == Some(CONTROLLER)
}

pub struct GatewayController {
    api: Arc<ApiClient>,
}

impl GatewayController {
    pub fn new(api: Arc<ApiClient>) -> Self {
        Self { api }
    }

    pub async fn run(&self) {
        let class = GatewayWorker {
            controller: self,
            kind: "gatewayclasses",
        };
        let gateway = GatewayWorker {
            controller: self,
            kind: "gateways",
        };
        let route = GatewayWorker {
            controller: self,
            kind: "httproutes",
        };
        tokio::join!(
            owned::run(&self.api, &class),
            owned::run(&self.api, &gateway),
            owned::run(&self.api, &route)
        );
    }

    async fn reconcile_gateway_class(&self, gc: &Value) -> anyhow::Result<()> {
        let gc_name = gc["metadata"]["name"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("GatewayClass missing name"))?;
        // Another controller's class: its status is that controller's (#91).
        if !owns(gc) {
            return Ok(());
        }
        let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let mut updated = gc.clone();
        updated["status"] = json!({
            "conditions": [{
                "type": "Accepted",
                "status": "True",
                "reason": "Accepted",
                "message": "GatewayClass managed by RustKube",
                "lastTransitionTime": now,
                "observedGeneration": gc["metadata"]["generation"].as_u64().unwrap_or(1)
            }]
        });

        owned::preserve_transition_times(&gc["status"], &mut updated["status"]);
        if gc["status"] == updated["status"] {
            return Ok(());
        }
        self.api
            .update_status(
                &format!("/apis/gateway.networking.k8s.io/v1/gatewayclasses/{gc_name}"),
                &updated,
            )
            .await?;

        Ok(())
    }

    async fn reconcile_gateway(
        &self,
        namespace: &str,
        gateway: &Value,
        deps: &Deps,
    ) -> anyhow::Result<()> {
        let gateway_name = gateway["metadata"]["name"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Gateway missing name"))?;
        let gateway_class_name = gateway["spec"]["gatewayClassName"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Gateway missing gatewayClassName"))?;

        // Only a Gateway of an own class. One of another controller's class
        // is that controller's; one naming no known class is nobody's yet.
        let class = deps
            .feed(0)
            .select(&Index::Name("".into(), gateway_class_name.into()))?;
        if !class.first().is_some_and(owns) {
            return Ok(());
        }
        let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let mut updated = gateway.clone();
        updated["status"] = gateway_status(gateway, gateway_class_name, &now);
        owned::preserve_transition_times(&gateway["status"], &mut updated["status"]);
        if gateway["status"] == updated["status"] {
            return Ok(());
        }
        self
            .api
            .update_status(
                &format!(
                    "/apis/gateway.networking.k8s.io/v1/namespaces/{namespace}/gateways/{gateway_name}"
                ),
                &updated,
            )
            .await?;

        info!("Gateway {namespace}/{gateway_name} accepted");

        Ok(())
    }

    async fn reconcile_httproute(
        &self,
        namespace: &str,
        httproute: &Value,
        gateway_map: &HashMap<String, &Value>,
        own_classes: &HashSet<String>,
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
            let parent_namespace = parent_ref["namespace"].as_str().unwrap_or(namespace);
            let parent_kind = parent_ref["kind"].as_str().unwrap_or("Gateway");

            // Only a parent Gateway of an own class gets an entry from
            // this controller; any other is another controller's to answer.
            if parent_kind != "Gateway" || parent_namespace != namespace {
                continue;
            }
            let Some(parent) = gateway_map.get(parent_name) else { continue };
            if !parent["spec"]["gatewayClassName"].as_str().is_some_and(|c| own_classes.contains(c)) {
                continue;
            }
            let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();

            valid_parents += 1;

            parent_statuses.push(json!({
                "parentRef": parent_ref,
                "controllerName": CONTROLLER,
                "conditions": [{
                    "type": "Accepted",
                    "status": "True",
                    "reason": "Accepted",
                    "message": format!("HTTPRoute accepted by Gateway {parent_name}"),
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

        // Nothing of ours to say, and nothing of ours to take back: a route
        // of other controllers' Gateways is not touched.
        let had_ours = httproute["status"]["parents"]
            .as_array()
            .is_some_and(|p| p.iter().any(|p| p["controllerName"].as_str() == Some(CONTROLLER)));
        if parent_statuses.is_empty() && !had_ours {
            return Ok(());
        }
        let mut updated = httproute.clone();
        updated["status"] = json!({
            "parents": merge_parents(&httproute["status"]["parents"], parent_statuses)
        });

        owned::preserve_transition_times(&httproute["status"], &mut updated["status"]);
        if httproute["status"] == updated["status"] {
            return Ok(());
        }
        self
            .api
            .update_status(
                &format!(
                    "/apis/gateway.networking.k8s.io/v1/namespaces/{namespace}/httproutes/{httproute_name}"
                ),
                &updated,
            )
            .await?;

        if valid_parents > 0 {
            debug!("HTTPRoute {namespace}/{httproute_name} accepted by {valid_parents} parent(s)");
        }

        Ok(())
    }
}

/// An own Gateway's status: accepted, listeners by protocol, and not
/// programmed — no data plane, so no address (#70, #91).
fn gateway_status(gateway: &Value, class: &str, now: &str) -> Value {
    let generation = gateway["metadata"]["generation"].as_u64().unwrap_or(1);
    let listeners: Vec<Value> = gateway["spec"]["listeners"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|l| {
            let name = l["name"].as_str().unwrap_or("unknown");
            let protocol = l["protocol"].as_str().unwrap_or("HTTP");
            let supported = matches!(protocol, "HTTP" | "HTTPS" | "TCP" | "TLS");
            json!({
                "name": name,
                "supportedKinds": [{"group": "gateway.networking.k8s.io", "kind": "HTTPRoute"}],
                "attachedRoutes": 0,
                "conditions": [{
                    "type": "Accepted",
                    "status": if supported { "True" } else { "False" },
                    "reason": if supported { "Accepted" } else { "UnsupportedProtocol" },
                    "message": if supported { format!("Listener {name} accepted") } else { format!("Protocol {protocol} not supported") },
                    "lastTransitionTime": now,
                    "observedGeneration": generation
                }, {
                    "type": "Programmed",
                    "status": "False",
                    "reason": "Pending",
                    "message": "no data plane: rustkube programs no proxy (rustkube#70)",
                    "lastTransitionTime": now,
                    "observedGeneration": generation
                }]
            })
        })
        .collect();
    json!({
        "conditions": [{
            "type": "Accepted",
            "status": "True",
            "reason": "Accepted",
            "message": format!("Gateway accepted, using GatewayClass {class}"),
            "lastTransitionTime": now,
            "observedGeneration": generation
        }, {
            "type": "Programmed",
            "status": "False",
            "reason": "Pending",
            "message": "no data plane: rustkube programs no proxy and assigns no address (rustkube#70)",
            "lastTransitionTime": now,
            "observedGeneration": generation
        }],
        "listeners": listeners
    })
}

/// An HTTPRoute's `status.parents`: other controllers' entries as they
/// were, this controller's replaced by `ours`.
fn merge_parents(existing: &Value, ours: Vec<Value>) -> Vec<Value> {
    let mut out: Vec<Value> = existing
        .as_array()
        .into_iter()
        .flatten()
        .filter(|p| p["controllerName"].as_str() != Some(CONTROLLER))
        .cloned()
        .collect();
    out.extend(ours);
    out
}

struct GatewayWorker<'a> {
    controller: &'a GatewayController,
    kind: &'static str,
}
#[async_trait::async_trait]
impl Controller for GatewayWorker<'_> {
    fn name(&self) -> &'static str {
        self.kind
    }
    fn primary(&self) -> &'static str {
        match self.kind {
            "gatewayclasses" => "/apis/gateway.networking.k8s.io/v1/gatewayclasses",
            "gateways" => "/apis/gateway.networking.k8s.io/v1/gateways",
            _ => "/apis/gateway.networking.k8s.io/v1/httproutes",
        }
    }
    fn dependencies(&self) -> Vec<Dependency> {
        let paths: &[&str] = match self.kind {
            "gatewayclasses" => &[],
            "gateways" => &["/apis/gateway.networking.k8s.io/v1/gatewayclasses"],
            _ => &[
                "/apis/gateway.networking.k8s.io/v1/gateways",
                "/api/v1/services",
                "/apis/gateway.networking.k8s.io/v1/gatewayclasses",
            ],
        };
        paths
            .iter()
            .map(|path| Dependency {
                path: (*path).into(),
                route: Arc::new(|delta, primary| {
                    delta
                        .old
                        .iter()
                        .chain(delta.new.iter())
                        .flat_map(|o| {
                            owned::keys_at(
                                primary,
                                Index::Reference(
                                    o["metadata"]["namespace"].as_str().unwrap_or("").into(),
                                    o["kind"].as_str().unwrap_or("").into(),
                                    o["metadata"]["name"].as_str().unwrap_or("").into(),
                                ),
                            )
                        })
                        .collect()
                }),
            })
            .collect()
    }
    async fn reconcile(&self, object: &Value, _: &[Value], deps: &Deps) -> anyhow::Result<()> {
        let ns = object["metadata"]["namespace"]
            .as_str()
            .unwrap_or("default");
        match self.kind {
            "gatewayclasses" => self.controller.reconcile_gateway_class(object).await,
            "gateways" => self.controller.reconcile_gateway(ns, object, deps).await,
            _ => {
                let mut gateways = Vec::new();
                let mut services = Vec::new();
                for parent in object["spec"]["parentRefs"]
                    .as_array()
                    .into_iter()
                    .flatten()
                {
                    if let Some(name) = parent["name"].as_str() {
                        gateways.extend(deps.feed(0).select(&Index::Name(ns.into(), name.into()))?);
                    }
                }
                for rule in object["spec"]["rules"].as_array().into_iter().flatten() {
                    for backend in rule["backendRefs"].as_array().into_iter().flatten() {
                        if let Some(name) = backend["name"].as_str() {
                            services
                                .extend(deps.feed(1).select(&Index::Name(ns.into(), name.into()))?);
                        }
                    }
                }
                let own_classes: HashSet<String> = gateways
                    .iter()
                    .filter_map(|g| g["spec"]["gatewayClassName"].as_str())
                    .filter(|c| {
                        deps.feed(2)
                            .select(&Index::Name("".into(), (*c).into()))
                            .ok()
                            .and_then(|v| v.first().map(owns))
                            .unwrap_or(false)
                    })
                    .map(str::to_string)
                    .collect();
                let gateway_map = gateways
                    .iter()
                    .filter_map(|o| o["metadata"]["name"].as_str().map(|n| (n.to_string(), o)))
                    .collect();
                let service_map = services
                    .iter()
                    .filter_map(|o| o["metadata"]["name"].as_str().map(|n| (n.to_string(), o)))
                    .collect();
                self.controller
                    .reconcile_httproute(ns, object, &gateway_map, &own_classes, &service_map)
                    .await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_own_class_is_owned() {
        assert!(owns(&json!({"spec": {"controllerName": CONTROLLER}})));
        assert!(!owns(&json!({"spec": {"controllerName": "io.cilium/gateway-controller"}})));
        assert!(!owns(&json!({"spec": {}})), "no controllerName: nobody's");
    }

    #[test]
    fn a_gateway_gets_no_invented_address_and_is_not_programmed() {
        let gw = json!({"metadata": {"generation": 2}, "spec": {"listeners": [
            {"name": "web", "protocol": "HTTP", "port": 80}, {"name": "udp", "protocol": "UDP", "port": 53}]}});
        let s = gateway_status(&gw, "rk", "t");
        assert!(s.get("addresses").is_none(), "no address is assigned, so none is written");
        assert_eq!(s["conditions"][0]["status"], "True");
        assert_eq!((s["conditions"][1]["type"].as_str(), s["conditions"][1]["status"].as_str(), s["conditions"][1]["reason"].as_str()),
                   (Some("Programmed"), Some("False"), Some("Pending")));
        assert_eq!(s["listeners"][0]["conditions"][0]["status"], "True");
        assert_eq!(s["listeners"][1]["conditions"][0]["reason"], "UnsupportedProtocol");
        assert_eq!(s["listeners"][0]["conditions"][1]["status"], "False");
        assert_eq!(s["conditions"][0]["observedGeneration"], 2);
    }

    #[test]
    fn another_controllers_route_parents_are_kept() {
        let existing = json!([
            {"parentRef": {"name": "cilium-gw"}, "controllerName": "io.cilium/gateway-controller", "conditions": [{"type": "Accepted"}]},
            {"parentRef": {"name": "old"}, "controllerName": CONTROLLER, "conditions": []}
        ]);
        let ours = vec![json!({"parentRef": {"name": "rk-gw"}, "controllerName": CONTROLLER})];
        let merged = merge_parents(&existing, ours);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0]["controllerName"], "io.cilium/gateway-controller");
        assert_eq!(merged[1]["parentRef"]["name"], "rk-gw", "our stale entry replaced");
        assert!(merge_parents(&Value::Null, vec![]).is_empty());
    }
}
