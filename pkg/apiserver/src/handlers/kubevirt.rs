//! `subresources.kubevirt.io/v1` — the console doors `virtctl` reaches for,
//! the VirtualMachine `start`/`stop`/`restart` subresources, which set
//! `spec.running`, and `migrate`, which creates a
//! VirtualMachineInstanceMigration (#184).
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

/// VirtualMachines and their instances are custom resources of this group, and
/// are stored under it (#76).
const KUBEVIRT: &str = "kubevirt.io";

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
            &ResourceStorage::custom_resource(KUBEVIRT, "virtualmachineinstances"),
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
            continue_token: None,
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

/// `PUT /apis/subresources.kubevirt.io/v1/namespaces/{ns}/virtualmachines/{name}/start`
pub async fn vm_start(state: State<AppState>, path: Path<(String, String)>) -> Response {
    set_running(state, path, true).await
}

/// `PUT .../virtualmachines/{name}/stop`
pub async fn vm_stop(state: State<AppState>, path: Path<(String, String)>) -> Response {
    set_running(state, path, false).await
}

/// `PUT .../virtualmachines/{name}/restart`
///
/// Deletes the instance and leaves the VirtualMachine controller to make
/// another. Upstream does the same thing, and for the same reason: a restart
/// that stopped and started would have to hold state across two requests to
/// know it owed a start, and a controller-manager restart in between would
/// leave the machine off.
pub async fn vm_restart(
    State(state): State<AppState>,
    Path((namespace, name)): Path<(String, String)>,
) -> Response {
    let vm = match load_vm(&state, &namespace, &name).await {
        Ok(v) => v,
        Err(e) => return e.into_response(),
    };
    if !apimachinery::kubevirt::wants_running(&vm) {
        return ApiError {
            status: StatusCode::CONFLICT,
            reason: "Conflict".into(),
            message: format!("VirtualMachine {namespace}/{name} is not running"),
            continue_token: None,
        }
        .into_response();
    }
    let key = ResourceStorage::namespaced_key(
        &ResourceStorage::custom_resource(KUBEVIRT, "virtualmachineinstances"),
        &namespace,
        &name,
    );
    match state.storage.delete(&key, None).await {
        // Already gone is success: the controller will create one, which is
        // the state the caller asked for.
        Ok(()) => ok(&namespace, &name, "restart"),
        Err(e) if e.status == StatusCode::NOT_FOUND => ok(&namespace, &name, "restart"),
        Err(e) => e.into_response(),
    }
}

/// Set `spec.running`, which is all `start` and `stop` do.
///
/// The verbs do no work themselves — that is the point of the object. They
/// state intent, and the VirtualMachine controller reconciles it (#62). A
/// `start` that created the VMI here would race the controller into creating
/// two.
async fn set_running(
    State(state): State<AppState>,
    Path((namespace, name)): Path<(String, String)>,
    running: bool,
) -> Response {
    let mut vm = match load_vm(&state, &namespace, &name).await {
        Ok(v) => v,
        Err(e) => return e.into_response(),
    };
    // A `runStrategy` of Halted or Always contradicts the boolean, and
    // silently leaving it would make the verb appear to do nothing. Upstream
    // rejects the combination; this clears it, because the caller has just
    // said plainly what they want.
    if vm["spec"]["runStrategy"].is_string() {
        if let Some(spec) = vm["spec"].as_object_mut() {
            spec.remove("runStrategy");
        }
    }
    vm["spec"]["running"] = Value::Bool(running);

    let key = ResourceStorage::namespaced_key(
        &ResourceStorage::custom_resource(KUBEVIRT, "virtualmachines"),
        &namespace,
        &name,
    );
    match state.storage.update(&key, vm, None).await {
        Ok(_) => ok(&namespace, &name, if running { "start" } else { "stop" }),
        Err(e) => e.into_response(),
    }
}

async fn load_vm(state: &AppState, namespace: &str, name: &str) -> Result<Value, ApiError> {
    state
        .storage
        .get(&ResourceStorage::namespaced_key(
            &ResourceStorage::custom_resource(KUBEVIRT, "virtualmachines"),
            namespace,
            name,
        ))
        .await
        .map_err(|_| ApiError::not_found("virtualmachines", name))
}

/// `PUT /apis/subresources.kubevirt.io/v1/namespaces/{ns}/virtualmachines/{name}/migrate`
///
/// What `virtctl migrate <vm>` calls. Like upstream's virt-api it does no
/// moving itself: it creates a `VirtualMachineInstanceMigration` for the
/// VM's instance, and the migration controller, the scheduler and the two
/// kubelets do the rest (#184).
pub async fn vm_migrate(
    State(state): State<AppState>,
    Path((namespace, name)): Path<(String, String)>,
    body: axum::body::Bytes,
) -> Response {
    if let Err(e) = load_vm(&state, &namespace, &name).await {
        return e.into_response();
    }
    migrate(state, namespace, name, body, "VM").await
}

/// `PUT .../virtualmachineinstances/{name}/migrate` — the same for an
/// instance that has no VirtualMachine.
pub async fn vmi_migrate(
    State(state): State<AppState>,
    Path((namespace, name)): Path<(String, String)>,
    body: axum::body::Bytes,
) -> Response {
    migrate(state, namespace, name, body, "VMI").await
}

async fn migrate(state: AppState, namespace: String, name: String, body: axum::body::Bytes, what: &str) -> Response {
    let conflict = |message: String| {
        ApiError { status: StatusCode::CONFLICT, reason: "Conflict".into(), message, continue_token: None }.into_response()
    };
    if state
        .crd_registry
        .lookup(KUBEVIRT, "v1", "virtualmachineinstancemigrations")
        .await
        .is_none()
    {
        return ApiError {
            status: StatusCode::NOT_FOUND,
            reason: "NotFound".into(),
            message: "VirtualMachineInstanceMigration is not served: its CRD \
                      (virtualmachineinstancemigrations.kubevirt.io) is not installed"
                .into(),
            continue_token: None,
        }
        .into_response();
    }
    // MigrateOptions: dryRun and addedNodeSelector.
    let opts: Value = if body.iter().all(u8::is_ascii_whitespace) {
        Value::Null
    } else {
        match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(e) => return ApiError::bad_request(&format!("MigrateOptions: {e}")).into_response(),
        }
    };
    if opts["addedNodeSelector"].as_object().is_some_and(|m| !m.is_empty()) {
        // The scheduler does not read it; taking it and ignoring it would
        // place the machine somewhere the caller excluded.
        return ApiError::invalid("addedNodeSelector is not supported").into_response();
    }
    let vmi = match state
        .storage
        .get(&ResourceStorage::namespaced_key(
            &ResourceStorage::custom_resource(KUBEVIRT, "virtualmachineinstances"),
            &namespace,
            &name,
        ))
        .await
    {
        Ok(v) => v,
        Err(_) => return conflict(format!("{what} is not running")),
    };
    if vmi["status"]["phase"].as_str() != Some("Running") || node_of(&vmi).is_none() {
        return conflict(format!("{what} is not running"));
    }
    if let Some(m) = apimachinery::kubevirt::active_migration(&vmi) {
        return conflict(format!(
            "VirtualMachineInstance {namespace}/{name} is already migrating (migration uid {})",
            m["migrationUid"].as_str().unwrap_or("")
        ));
    }
    let migration = serde_json::json!({
        "apiVersion": "kubevirt.io/v1",
        "kind": "VirtualMachineInstanceMigration",
        "metadata": {"generateName": format!("kubevirt-migrate-{}-", what.to_lowercase()), "namespace": namespace},
        "spec": {"vmiName": name},
    });
    let dry_run = opts["dryRun"]
        .as_array()
        .is_some_and(|d| d.iter().any(|v| v == "All"));
    if dry_run {
        return ok_message(&format!("migration of {namespace}/{name} would be created (dry run)"));
    }
    match crate::crd::crd_create_ns(
        State(state),
        Path((KUBEVIRT.into(), "v1".into(), namespace.clone(), "virtualmachineinstancemigrations".into())),
        axum::extract::RawQuery(None),
        axum::body::Bytes::from(serde_json::to_vec(&migration).unwrap_or_default()),
    )
    .await
    {
        Ok(created) => {
            let resp = created.into_response();
            let (parts, body) = resp.into_parts();
            if !parts.status.is_success() {
                return Response::from_parts(parts, body);
            }
            let bytes = axum::body::to_bytes(body, 1 << 20).await.unwrap_or_default();
            let created: Value = serde_json::from_slice(&bytes).unwrap_or_default();
            ok_message(&format!(
                "migration {} of {namespace}/{name} created",
                created["metadata"]["name"].as_str().unwrap_or("")
            ))
        }
        Err(e) => e.into_response(),
    }
}

fn ok_message(message: &str) -> Response {
    axum::Json(serde_json::json!({
        "kind": "Status",
        "apiVersion": "v1",
        "metadata": {},
        "status": "Success",
        "message": message,
    }))
    .into_response()
}

/// What KubeVirt's subresource verbs answer with: a plain `Status` success.
fn ok(namespace: &str, name: &str, what: &str) -> Response {
    axum::Json(serde_json::json!({
        "kind": "Status",
        "apiVersion": "v1",
        "metadata": {},
        "status": "Success",
        "message": format!("{what} requested for VirtualMachine {namespace}/{name}"),
    }))
    .into_response()
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

    mod migrate {
        use super::*;
        use crate::crd::CrdRegistry;
        use crate::test_store::MemStore;
        use std::sync::Arc;

        fn crd(plural: &str, kind: &str) -> Value {
            json!({
                "metadata": {"name": format!("{plural}.kubevirt.io")},
                "spec": {"group": "kubevirt.io", "scope": "Namespaced",
                         "names": {"plural": plural, "kind": kind},
                         "versions": [{"name": "v1", "served": true, "storage": true}]}
            })
        }

        async fn state(with_crd: bool) -> AppState {
            let registry = Arc::new(CrdRegistry::new());
            registry.register(&crd("virtualmachineinstances", "VirtualMachineInstance")).await;
            if with_crd {
                registry
                    .register(&crd("virtualmachineinstancemigrations", "VirtualMachineInstanceMigration"))
                    .await;
            }
            AppState {
                storage: Arc::new(ResourceStorage::new(Arc::new(MemStore::default()))),
                crd_registry: registry,
                service_cidr: "10.96.0.0/12".into(),
                admission: Default::default(),
            }
        }

        fn vmi_key() -> String {
            ResourceStorage::namespaced_key(
                &ResourceStorage::custom_resource(KUBEVIRT, "virtualmachineinstances"),
                "ns",
                "vm",
            )
        }

        async fn put_vmi(state: &AppState, status: Value) {
            state
                .storage
                .create(&vmi_key(), json!({"metadata": {"name": "vm", "namespace": "ns", "uid": "v1"}, "status": status}))
                .await
                .unwrap();
        }

        async fn call(state: &AppState, body: &str) -> (StatusCode, Value) {
            let resp = vmi_migrate(
                State(state.clone()),
                Path(("ns".into(), "vm".into())),
                axum::body::Bytes::from(body.to_string()),
            )
            .await;
            let code = resp.status();
            let bytes = axum::body::to_bytes(resp.into_body(), 1 << 16).await.unwrap();
            (code, serde_json::from_slice(&bytes).unwrap_or_default())
        }

        #[tokio::test]
        async fn migrate_creates_a_migration_for_the_vmi() {
            let st = state(true).await;
            put_vmi(&st, json!({"phase": "Running", "nodeName": "a"})).await;
            let (code, out) = call(&st, "{}").await;
            assert_eq!(code, StatusCode::OK, "{out}");
            let name = out["message"].as_str().unwrap().split(' ').nth(1).unwrap().to_string();
            assert!(name.starts_with("kubevirt-migrate-vmi-"), "{name}");
            let key = ResourceStorage::namespaced_key(
                &ResourceStorage::custom_resource(KUBEVIRT, "virtualmachineinstancemigrations"),
                "ns",
                &name,
            );
            let m = st.storage.get(&key).await.unwrap();
            assert_eq!(m["spec"]["vmiName"], "vm");
            assert_eq!(m["kind"], "VirtualMachineInstanceMigration");
            assert!(m["metadata"]["uid"].as_str().is_some_and(|u| !u.is_empty()));
        }

        #[tokio::test]
        async fn migrate_refuses_what_cannot_move() {
            // No CRD: not served, said so.
            let st = state(false).await;
            put_vmi(&st, json!({"phase": "Running", "nodeName": "a"})).await;
            assert_eq!(call(&st, "").await.0, StatusCode::NOT_FOUND);
            // Not running.
            let st = state(true).await;
            assert_eq!(call(&st, "").await.0, StatusCode::CONFLICT);
            put_vmi(&st, json!({"phase": "Scheduling"})).await;
            assert_eq!(call(&st, "").await.0, StatusCode::CONFLICT);
            // Already migrating.
            let st = state(true).await;
            put_vmi(&st, json!({"phase": "Running", "nodeName": "a",
                                "migrationState": {"migrationUid": "m0", "sourceNode": "a"}})).await;
            let (code, out) = call(&st, "").await;
            assert_eq!(code, StatusCode::CONFLICT);
            assert!(out["message"].as_str().unwrap().contains("already migrating"));
            // A selector the scheduler would not honour.
            let st = state(true).await;
            put_vmi(&st, json!({"phase": "Running", "nodeName": "a"})).await;
            let (code, _) = call(&st, r#"{"addedNodeSelector": {"zone": "b"}}"#).await;
            assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);
            // Dry run: answered, nothing created.
            let (code, out) = call(&st, r#"{"dryRun": ["All"]}"#).await;
            assert_eq!(code, StatusCode::OK);
            assert!(out["message"].as_str().unwrap().contains("dry run"));
        }
    }
}
