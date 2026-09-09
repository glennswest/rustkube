//! `subresources.kubevirt.io/v1` — the console doors `virtctl` reaches for.
//!
//! `oc get vmi` already works, because `VirtualMachineInstance` is applied as
//! an ordinary CRD and a CRD gets its object and `/status`. What a CRD cannot
//! give is a *subresource that is not stored* — and `console` and `vnc` are
//! exactly that: a WebSocket proxied to whichever node runs the guest.
//! Without them `virtctl console <vm>` has nothing to resolve, and the console
//! is reachable only through stormconsole's own origin (#61).
//!
//! ## Served here rather than through an APIService
//!
//! Upstream KubeVirt registers an aggregated `APIService` pointing at
//! `virt-api`. There is no `virt-api` here — the thing that would answer is
//! stormvm, on the node — so an APIService would be an indirection to a
//! service that does not exist. The cost of serving it directly is that this
//! apiserver knows the word "VirtualMachineInstance"; the alternative was
//! standing up a component whose only job is to forward.
//!
//! ## Why the hop goes through the kubelet
//!
//! stormvm's rule is *loopback, or a token*, and **minting is loopback-only**
//! by design: something that has already authenticated hands out a credential
//! for one attach, so a mint endpoint anyone could call would make the token a
//! formality. The apiserver is off-node, so it can neither reach a
//! loopback-bound stormvm nor mint itself a token for one.
//!
//! The kubelet is on the node and is already an authenticated hop — it
//! validates the apiserver's bearer token by TokenReview, which is what
//! `pods/log` and `exec` use. So the console takes the same route: apiserver →
//! kubelet (TLS, bearer) → stormvm (loopback). No second auth scheme, and
//! nothing new baked into the node image.

use crate::error::ApiError;
use crate::handlers::AppState;
use crate::storage::ResourceStorage;
use axum::extract::{Extension, Path, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::Value;

/// `GET /apis/subresources.kubevirt.io/v1/namespaces/{ns}/virtualmachineinstances/{name}/console`
pub async fn vmi_console(
    state: State<AppState>,
    keys: Extension<crate::auth::SigningKeys>,
    path: Path<(String, String)>,
    req: Request,
) -> Response {
    door(state, keys, path, "serial", req).await
}

/// `GET /apis/subresources.kubevirt.io/v1/namespaces/{ns}/virtualmachineinstances/{name}/vnc`
pub async fn vmi_vnc(
    state: State<AppState>,
    keys: Extension<crate::auth::SigningKeys>,
    path: Path<(String, String)>,
    req: Request,
) -> Response {
    door(state, keys, path, "vnc", req).await
}

/// One door of one VM.
///
/// `door` is stormvm's spelling — the subresource `console` is the *serial*
/// console, which is what `virtctl console` attaches to.
async fn door(
    State(state): State<AppState>,
    Extension(keys): Extension<crate::auth::SigningKeys>,
    Path((namespace, name)): Path<(String, String)>,
    door: &str,
    req: Request,
) -> Response {
    let vmi = match state
        .storage
        .get(&ResourceStorage::namespaced_key(
            "virtualmachineinstances",
            &namespace,
            &name,
        ))
        .await
    {
        Ok(v) => v,
        Err(_) => return ApiError::not_found("virtualmachineinstances", &name).into_response(),
    };

    // A VMI carries its node in `status.nodeName` — the kubelet patches it —
    // where a pod carries it in `spec.nodeName`. That difference is the whole
    // reason the proxy takes a node rather than an object.
    let Some(node) = node_of(&vmi) else {
        return ApiError {
            status: StatusCode::BAD_REQUEST,
            reason: "BadRequest".into(),
            message: format!(
                "VirtualMachineInstance {namespace}/{name} is not running on a node yet"
            ),
        }
        .into_response();
    };

    // GET, not POST: this is a WebSocket handshake, and stormvm is a real
    // WebSocket server that is entitled to refuse anything else.
    let path = format!("/vmConsole/{namespace}/{name}/{door}");
    crate::handlers::streaming::proxy_to_node(
        state,
        keys,
        &node,
        "GET",
        path,
        String::new(),
        req,
    )
    .await
}

/// Which node runs this VMI.
///
/// `status.nodeName` first, because that is where the machine actually is.
/// `spec.nodeName` is the fallback for a VMI that was placed directly rather
/// than scheduled — a request to the node it was assigned to fails with
/// something a reader can act on, which beats "not running yet" for a guest
/// that is plainly running.
fn node_of(vmi: &Value) -> Option<String> {
    for at in [&vmi["status"]["nodeName"], &vmi["spec"]["nodeName"]] {
        if let Some(n) = at.as_str().filter(|s| !s.is_empty()) {
            return Some(n.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_node_comes_from_status_first() {
        // status is where the machine is; spec is only where it was asked to
        // go, and the two differ for the whole of a migration.
        let vmi = json!({"spec": {"nodeName": "n1"}, "status": {"nodeName": "n2"}});
        assert_eq!(node_of(&vmi).as_deref(), Some("n2"));
    }

    #[test]
    fn a_placed_but_unstarted_vmi_still_resolves() {
        let vmi = json!({"spec": {"nodeName": "n1"}, "status": {}});
        assert_eq!(node_of(&vmi).as_deref(), Some("n1"));
    }

    #[test]
    fn a_vmi_on_no_node_resolves_to_nothing() {
        // An empty string is not a node. It reaches this as `"nodeName": ""`
        // from a status patched before the machine was placed, and treating it
        // as a name would send the console at a host called "".
        assert_eq!(node_of(&json!({"status": {"nodeName": ""}})), None);
        assert_eq!(node_of(&json!({})), None);
    }
}
