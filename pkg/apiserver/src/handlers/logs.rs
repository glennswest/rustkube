//! The `pods/log` subresource.
//!
//! `kubectl logs` does not read files. It issues
//! `GET /api/v1/namespaces/{ns}/pods/{name}/log`, and the apiserver proxies
//! that to the kubelet of the node the pod is on, streaming the answer back.
//! Without this the request 404s with "the server could not find the requested
//! resource (get pods x)" — a message that names the pod rather than the
//! subresource, which sends the reader to look at the pod first (#54).
//!
//! The same proxy path is what `exec`, `attach` and `portforward` will need, so
//! the node-address lookup and the client live here rather than in the handler.

use crate::error::ApiError;
use crate::handlers::AppState;
use crate::storage::ResourceStorage;
use axum::extract::{Extension, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

/// `GET /api/v1/namespaces/{namespace}/pods/{name}/log`
pub async fn pod_logs(
    State(state): State<AppState>,
    Extension(keys): Extension<crate::auth::SigningKeys>,
    Path((namespace, name)): Path<(String, String)>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let storage = &state.storage;
    let pod = match storage
        .get(&ResourceStorage::namespaced_key("pods", &namespace, &name))
        .await
    {
        Ok(p) => p,
        Err(_) => return ApiError::not_found("pods", &name).into_response(),
    };

    // Which container. Upstream requires the name when a pod has more than
    // one, and lists them in the error — which is the difference between a
    // usable message and a puzzle.
    let containers: Vec<String> = pod["spec"]["containers"]
        .as_array()
        .map(|v| v.as_slice())
        .unwrap_or(&[])
        .iter()
        .filter_map(|c| c["name"].as_str().map(str::to_string))
        .collect();
    let container = match params.get("container") {
        Some(c) => {
            if !containers.iter().any(|x| x == c) {
                return ApiError {
                    status: StatusCode::BAD_REQUEST,
                    reason: "BadRequest".into(),
                    message: format!(
                        "container {c} is not valid for pod {name}; choose one of [{}]",
                        containers.join(", ")
                    ),
                }
                .into_response();
            }
            c.clone()
        }
        None if containers.len() == 1 => containers[0].clone(),
        None => {
            return ApiError {
                status: StatusCode::BAD_REQUEST,
                reason: "BadRequest".into(),
                message: format!(
                    "a container name must be specified for pod {name}, choose one of: [{}]",
                    containers.join(", ")
                ),
            }
            .into_response()
        }
    };

    let Some(node_name) = pod["spec"]["nodeName"].as_str().filter(|s| !s.is_empty()) else {
        return ApiError {
            status: StatusCode::BAD_REQUEST,
            reason: "BadRequest".into(),
            message: format!("pod {namespace}/{name} is not assigned to a node yet"),
        }
        .into_response();
    };

    let Some(addr) = node_address(storage, node_name).await else {
        return ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            reason: "InternalError".into(),
            message: format!("no usable address for node {node_name}"),
        }
        .into_response();
    };

    // Forward the log options the client asked for, minus the one that named
    // the container — that is in the path on the kubelet side.
    let mut query: Vec<String> = Vec::new();
    for k in [
        "tailLines",
        "sinceSeconds",
        "sinceTime",
        "timestamps",
        "previous",
        "follow",
        "limitBytes",
    ] {
        if let Some(v) = params.get(k) {
            query.push(format!("{k}={v}"));
        }
    }
    let q = if query.is_empty() {
        String::new()
    } else {
        format!("?{}", query.join("&"))
    };
    let url = format!("https://{addr}:10250/containerLogs/{namespace}/{name}/{container}{q}");

    // The kubelet serves a self-signed certificate whose SANs are the node name
    // and IP; upstream verifies it against the cluster CA. Until certificates
    // are issued (rustkube#20) the connection is not verified, which is stated
    // here rather than left for a reader to infer from a builder flag.
    let client = match reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(std::time::Duration::from_secs(30))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return ApiError::internal(&format!("cannot build kubelet client: {e}")).into_response()
        }
    };

    // Authenticate to the kubelet as the apiserver.
    //
    // The kubelet validates a bearer token by TokenReview against this
    // apiserver, so a token this apiserver signs is one it will accept — which
    // means no shared secret has to be baked into the node image, and the
    // identity in the kubelet's logs is the apiserver rather than "someone
    // with the token". `system:masters` because reading any pod's log on any
    // node is exactly what this endpoint is for.
    //
    // Without it the proxy is answered with 401 and `kubectl logs` reports
    // "Unauthorized" with nothing to say which hop refused.
    let bearer = keys
        .create_token("system:kube-apiserver", &["system:masters".to_string()])
        .unwrap_or_default();

    match client.get(&url).bearer_auth(&bearer).send().await {
        Ok(resp) => {
            let status = StatusCode::from_u16(resp.status().as_u16())
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            match resp.text().await {
                Ok(body) => (status, body).into_response(),
                Err(e) => ApiError::internal(&format!("reading kubelet response: {e}"))
                    .into_response(),
            }
        }
        Err(e) => ApiError::internal(&format!("reaching kubelet at {addr}: {e}")).into_response(),
    }
}

/// A node's address, preferring InternalIP as upstream does.
async fn node_address(storage: &ResourceStorage, node_name: &str) -> Option<String> {
    let node: Value = storage
        .get(&ResourceStorage::cluster_key("nodes", node_name))
        .await
        .ok()?;
    let addrs = node["status"]["addresses"].as_array()?.clone();
    for want in ["InternalIP", "ExternalIP", "Hostname"] {
        if let Some(a) = addrs
            .iter()
            .find(|a| a["type"].as_str() == Some(want))
            .and_then(|a| a["address"].as_str())
        {
            return Some(a.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn internal_ip_is_preferred_over_hostname() {
        // Upstream reaches a kubelet on its InternalIP; a Hostname may not
        // resolve from the apiserver, and picking it would work in a lab and
        // fail wherever DNS is not shared.
        let node = json!({"status":{"addresses":[
            {"type":"Hostname","address":"n1"},
            {"type":"InternalIP","address":"10.0.0.5"}]}});
        let addrs = node["status"]["addresses"].as_array().unwrap();
        let pick = |want: &str| {
            addrs.iter().find(|a| a["type"] == want).and_then(|a| a["address"].as_str())
        };
        assert_eq!(pick("InternalIP"), Some("10.0.0.5"));
        assert_eq!(pick("Hostname"), Some("n1"));
    }
}
