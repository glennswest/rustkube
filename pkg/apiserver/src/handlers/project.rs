//! `project.openshift.io/v1` — a Project is a Namespace, seen by its members
//! (#97).
//!
//! Nothing is stored as a Project. Every Project is the Namespace of the same
//! name, translated on the way out, so there is one object and one lifecycle:
//! deleting a Project is deleting its Namespace, and the namespace controller's
//! cascade (#28) takes everything in it.
//!
//! What a Project adds over a Namespace is **ownership**:
//!
//! - `ProjectRequest` (`oc new-project`) lets an ordinary user make one. The
//!   Namespace is annotated with who asked, and the requester is bound to the
//!   `admin` ClusterRole in it by a RoleBinding named `admin`. Who may request
//!   is RBAC: the `self-provisioners` ClusterRoleBinding grants it to
//!   `system:authenticated`; emptying its subjects turns self-service off.
//! - `Project` lists (`oc projects`) show only the namespaces the caller is a
//!   member of — holds any RoleBinding in — unless the caller may list
//!   Namespaces cluster-wide, in which case they see all of them.
//!
//! Get, update and delete of `projects/{name}` are authorized in the project's
//! own namespace (see `rbac_engine::parse_path_segments`), which is why the
//! `admin` binding a request creates is enough to delete the project it made.

use crate::auth::UserInfo;
use crate::error::ApiError;
use crate::handlers::resource::{self, parse_delete_options};
use crate::handlers::AppState;
use crate::rbac_engine::{subjects_match, AuthorizationRequest, RbacEngine};
use crate::storage::ResourceStorage;
use crate::watch::{self, WatchParams};
use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde_json::{json, Value};
use std::sync::Arc;

pub const API_VERSION: &str = "project.openshift.io/v1";
/// Who asked for the project. Set by the server, never taken from the body.
pub const REQUESTER: &str = "openshift.io/requester";
pub const DISPLAY_NAME: &str = "openshift.io/display-name";
pub const DESCRIPTION: &str = "openshift.io/description";
/// The RoleBinding a ProjectRequest makes, as OpenShift names it.
pub const ADMIN_BINDING: &str = "admin";

/// A Namespace, as the Project of the same name.
pub fn namespace_to_project(ns: Value) -> Value {
    let mut metadata = ns["metadata"].clone();
    if let Some(m) = metadata.as_object_mut() {
        m.remove("namespace");
    }
    json!({
        "apiVersion": API_VERSION,
        "kind": "Project",
        "metadata": metadata,
        "spec": { "finalizers": ns["spec"]["finalizers"].clone() },
        "status": ns["status"].clone(),
    })
}

/// The Namespace a Project (or ProjectRequest) body describes.
///
/// Only what a Project may say about its Namespace is carried over: name,
/// labels and annotations. `requester` is set by the caller from the
/// authenticated identity; any value in the body is dropped, because an
/// annotation that says who owns a project is worth nothing if the owner can
/// be written in by anyone.
fn project_namespace(
    name: &str,
    labels: &Value,
    annotations: &Value,
    display_name: Option<&str>,
    description: Option<&str>,
    requester: Option<&str>,
) -> Value {
    let mut ann = annotations.as_object().cloned().unwrap_or_default();
    ann.remove(REQUESTER);
    if let Some(d) = display_name.filter(|s| !s.is_empty()) {
        ann.insert(DISPLAY_NAME.into(), json!(d));
    }
    if let Some(d) = description.filter(|s| !s.is_empty()) {
        ann.insert(DESCRIPTION.into(), json!(d));
    }
    if let Some(r) = requester {
        ann.insert(REQUESTER.into(), json!(r));
    }
    let mut meta = json!({ "name": name });
    if let Some(l) = labels.as_object().filter(|l| !l.is_empty()) {
        meta["labels"] = Value::Object(l.clone());
    }
    if !ann.is_empty() {
        meta["annotations"] = Value::Object(ann);
    }
    json!({ "apiVersion": "v1", "kind": "Namespace", "metadata": meta, "spec": {} })
}

/// The RoleBinding that makes `user` the admin of `namespace`.
///
/// A ServiceAccount is bound as a ServiceAccount, so the binding reads the way
/// `oc adm policy add-role-to-user` would have written it; anyone else is a
/// User, which matches on the exact username.
fn admin_binding(namespace: &str, user: &UserInfo) -> Value {
    let subject = match user
        .username
        .strip_prefix("system:serviceaccount:")
        .and_then(|r| r.split_once(':'))
    {
        Some((ns, name)) => json!({ "kind": "ServiceAccount", "name": name, "namespace": ns }),
        None => json!({
            "kind": "User",
            "apiGroup": "rbac.authorization.k8s.io",
            "name": user.username,
        }),
    };
    json!({
        "apiVersion": "rbac.authorization.k8s.io/v1",
        "kind": "RoleBinding",
        "metadata": { "name": ADMIN_BINDING, "namespace": namespace },
        "roleRef": {
            "apiGroup": "rbac.authorization.k8s.io",
            "kind": "ClusterRole",
            "name": "admin",
        },
        "subjects": [subject],
    })
}

/// A Namespace name must be a DNS-1123 label.
///
/// Checked here because this path creates the Namespace itself; a bad name
/// would otherwise become a key nothing else can address.
fn validate_name(name: &str) -> Result<(), ApiError> {
    let ok = !name.is_empty()
        && name.len() <= 63
        && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-');
    if ok {
        Ok(())
    } else {
        Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Invalid",
            &format!(
                "Project \"{name}\" is invalid: metadata.name: must be a lowercase RFC 1123 \
                 label (a-z, 0-9 and '-', starting and ending alphanumeric, at most 63 characters)"
            ),
        ))
    }
}

/// Names a user may not request: the system's own namespaces and the
/// prefixes it will use for more, as upstream reserves them. A cluster-admin
/// can still create such a namespace directly.
fn is_reserved(name: &str) -> bool {
    matches!(name, "default" | "openshift")
        || name.starts_with("kube-")
        || name.starts_with("openshift-")
}

/// Create the Namespace behind a project, through the same admission a
/// Namespace created directly gets.
async fn create_namespace(state: &AppState, mut ns: Value, name: &str) -> Result<Value, ApiError> {
    validate_name(name)?;
    resource::ensure_metadata_pub(&mut ns, name, None);
    crate::builtin_admission::admit_create(
        &state.storage, "namespaces", None, &mut ns, &state.service_cidr,
    )
    .await?;
    let key = ResourceStorage::cluster_key("namespaces", name);
    state.storage.create(&key, ns).await.map_err(|e| {
        if e.is_already_exists() {
            ApiError::already_exists("projects.project.openshift.io", name)
        } else {
            e
        }
    })
}

/// `POST /apis/project.openshift.io/v1/projectrequests` — `oc new-project`.
pub async fn create_project_request(
    State(state): State<AppState>,
    Extension(user): Extension<UserInfo>,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    let name = body["metadata"]["name"]
        .as_str()
        .ok_or_else(|| ApiError::invalid("metadata.name is required"))?
        .to_string();
    if is_reserved(&name) {
        return Err(ApiError::forbidden(&format!(
            "cannot request a project with the name \"{name}\": names starting kube- or \
             openshift-, and default, are reserved for the system"
        )));
    }
    let ns = project_namespace(
        &name,
        &body["metadata"]["labels"],
        &body["metadata"]["annotations"],
        body["displayName"].as_str(),
        body["description"].as_str(),
        Some(&user.username),
    );
    let created = create_namespace(&state, ns, &name).await?;

    // The namespace is made; now the ownership. If this write fails the
    // project exists and nobody but a cluster-admin can use it, so the
    // namespace is taken back rather than left as an orphan the requester
    // cannot see, delete or retry (the name is still taken).
    let mut binding = admin_binding(&name, &user);
    resource::ensure_metadata_pub(&mut binding, ADMIN_BINDING, Some(&name));
    let bkey = ResourceStorage::namespaced_key("rolebindings", &name, ADMIN_BINDING);
    if let Err(e) = state.storage.create(&bkey, binding).await {
        if !e.is_already_exists() {
            let nkey = ResourceStorage::cluster_key("namespaces", &name);
            let _ = state.storage.delete(&nkey, None).await;
            return Err(ApiError::internal(&format!(
                "project \"{name}\" could not be given its admin binding, and was removed: {}",
                e.message
            )));
        }
    }
    tracing::info!("project {name} requested by {}", user.username);
    Ok((StatusCode::CREATED, Json(namespace_to_project(created))).into_response())
}

/// `GET /apis/project.openshift.io/v1/projectrequests`
///
/// Upstream answers with a success Status when the caller may request a
/// project — `oc` uses it to ask before trying. RBAC has already said yes by
/// the time this runs (`list projectrequests`).
pub async fn list_project_requests() -> Json<Value> {
    Json(json!({
        "apiVersion": "v1", "kind": "Status", "metadata": {}, "status": "Success",
    }))
}

/// `POST /apis/project.openshift.io/v1/projects` — a Project made directly.
///
/// Upstream reserves this for cluster administrators (it is `create
/// projects`, which only `cluster-admin` holds), and does not bind anyone:
/// an administrator making a project for someone else then grants it.
pub async fn create_project(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    let name = body["metadata"]["name"]
        .as_str()
        .ok_or_else(|| ApiError::invalid("metadata.name is required"))?
        .to_string();
    let ns = project_namespace(
        &name,
        &body["metadata"]["labels"],
        &body["metadata"]["annotations"],
        None,
        None,
        None,
    );
    let created = create_namespace(&state, ns, &name).await?;
    Ok((StatusCode::CREATED, Json(namespace_to_project(created))).into_response())
}

/// `GET /apis/project.openshift.io/v1/projects/{name}`
pub async fn get_project(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: axum::http::HeaderMap,
) -> Result<Response, ApiError> {
    let ns = get_namespace(&state, &name).await?;
    let project = namespace_to_project(ns);
    if crate::table::wants_table(&headers) {
        return Ok(Json(crate::table::to_table("projects", project)).into_response());
    }
    Ok(Json(project).into_response())
}

async fn get_namespace(state: &AppState, name: &str) -> Result<Value, ApiError> {
    let key = ResourceStorage::cluster_key("namespaces", name);
    state.storage.get(&key).await.map_err(|e| {
        if e.reason == "NotFound" {
            ApiError::not_found("projects.project.openshift.io", name)
        } else {
            e
        }
    })
}

/// `PUT /apis/project.openshift.io/v1/projects/{name}` — the display name
/// and description, and nothing else.
///
/// Update is authorized in the project's own namespace, so anyone holding
/// `admin` there may make it — including someone who bound themselves
/// cluster-admin in their own project, which nothing here prevents. Labels
/// are therefore not writable this way: `pod-security.kubernetes.io/enforce`
/// is a label, and a project owner who could set it could run privileged pods.
/// Everything else — labels, other annotations, the requester — changes only
/// through the Namespace, which is cluster-scoped to write.
pub async fn update_project(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    let key = ResourceStorage::cluster_key("namespaces", &name);
    let mut ns = get_namespace(&state, &name).await?;
    if let Some(rv) = body["metadata"]["resourceVersion"].as_str().filter(|s| !s.is_empty()) {
        if ns["metadata"]["resourceVersion"].as_str() != Some(rv) {
            return Err(ApiError::conflict(&format!(
                "Operation cannot be fulfilled on projects.project.openshift.io \"{name}\": \
                 the object has been modified; please apply your changes to the latest version \
                 and try again"
            )));
        }
    }
    apply_project_update(&mut ns, &body);
    let rev = ns["metadata"]["resourceVersion"].as_str().and_then(|r| r.parse().ok());
    let updated = state.storage.update(&key, ns, rev).await?;
    Ok(Json(namespace_to_project(updated)).into_response())
}

/// Carry a Project update's display name and description onto its Namespace.
fn apply_project_update(ns: &mut Value, project: &Value) {
    if !ns["metadata"]["annotations"].is_object() {
        ns["metadata"]["annotations"] = json!({});
    }
    for key in [DISPLAY_NAME, DESCRIPTION] {
        let want = project["metadata"]["annotations"][key].as_str().filter(|s| !s.is_empty());
        let Some(ann) = ns["metadata"]["annotations"].as_object_mut() else { return };
        match want {
            Some(v) => {
                ann.insert(key.into(), json!(v));
            }
            None => {
                ann.remove(key);
            }
        }
    }
}

/// `DELETE /apis/project.openshift.io/v1/projects/{name}` — deletes the
/// Namespace, which terminates and takes everything in it (#28).
pub async fn delete_project(
    State(state): State<AppState>,
    Path(name): Path<String>,
    body: axum::body::Bytes,
) -> Result<Response, ApiError> {
    let ns = get_namespace(&state, &name).await?;
    let opts = parse_delete_options(&body);
    resource::terminate_namespace(&state, &name, ns, &opts).await?;
    Ok(Json(json!({
        "apiVersion": "v1", "kind": "Status", "metadata": {}, "status": "Success",
        "details": { "name": name, "group": "project.openshift.io", "kind": "projects" },
    }))
    .into_response())
}

/// May this user see every namespace? The test is upstream's: `list
/// namespaces` at cluster scope. Not `list projects`, which every
/// authenticated user holds so that the filtered list can be asked for at all.
async fn sees_all(rbac: &RbacEngine, user: &UserInfo) -> bool {
    rbac.authorize(
        user,
        &AuthorizationRequest {
            verb: "list".into(),
            resource: "namespaces".into(),
            subresource: None,
            api_group: String::new(),
            namespace: None,
            name: None,
        },
    )
    .await
}

/// Is `user` a member of `namespace` — named by any RoleBinding in it?
///
/// "Any role", not a particular one: a user given only `view` sees the project
/// in `oc projects`, as does one bound to a Role the project's admin wrote.
/// A listing that fails is not membership — the project is left out, which
/// hides it rather than showing a project to someone who may not own it.
async fn is_member(storage: &ResourceStorage, user: &UserInfo, namespace: &str) -> bool {
    let prefix = ResourceStorage::namespace_prefix("rolebindings", namespace);
    match storage.list(&prefix, 0, None).await {
        Ok((bindings, _, _)) => bindings.iter().any(|b| subjects_match(b, user)),
        Err(_) => false,
    }
}

/// Every namespace, each page followed.
async fn all_namespaces(storage: &ResourceStorage) -> Result<(Vec<Value>, u64), ApiError> {
    let prefix = ResourceStorage::cluster_prefix("namespaces");
    let mut out = Vec::new();
    let mut token: Option<String> = None;
    let mut rev = 0;
    loop {
        let (items, next, r) = storage.list(&prefix, 500, token.as_deref()).await?;
        out.extend(items);
        rev = rev.max(r);
        match next {
            Some(t) => token = Some(t),
            None => return Ok((out, rev)),
        }
    }
}

/// `GET /apis/project.openshift.io/v1/projects` — the caller's projects.
///
/// Not paginated: the answer is filtered per namespace, so a page of
/// namespaces is not a page of projects, and a `continue` token over the
/// filtered set would have to be re-derived on every call. A cluster with
/// more namespaces than one response can carry is some way off (#66).
///
/// Watch is served only to a caller who sees every namespace. For anyone
/// else the set being watched changes as bindings do, and a stream that
/// quietly showed or hid other people's projects is worse than none.
pub async fn list_projects(
    State(state): State<AppState>,
    Extension(user): Extension<UserInfo>,
    Extension(rbac): Extension<Arc<RbacEngine>>,
    headers: axum::http::HeaderMap,
    RawQuery(query): RawQuery,
) -> Result<Response, ApiError> {
    let params = WatchParams::from_query(query.as_deref().unwrap_or(""));
    let all = sees_all(&rbac, &user).await;

    if params.watch {
        if !all {
            return Err(ApiError::forbidden(&format!(
                "{} may not watch projects: watching is limited to users who may list every \
                 namespace; list projects instead",
                user.username
            )));
        }
        let (initial, live_rev) = if params.send_initial_events {
            let (items, rev) = all_namespaces(&state.storage).await?;
            (Some((items, rev)), rev)
        } else {
            (None, params.resource_version.unwrap_or(0))
        };
        let prefix = ResourceStorage::cluster_prefix("namespaces");
        let rx = state.storage.watch(&prefix, live_rev).await?;
        return Ok(watch::watch_response(
            rx,
            watch::WatchResponseOpts {
                label_selector: params.label_selector.clone(),
                field_selector: params.field_selector.clone(),
                api_version: API_VERSION.into(),
                kind: "Project".into(),
                allow_bookmarks: params.allow_watch_bookmarks,
                metadata_only: false,
                transform: Some(namespace_to_project),
                initial,
            },
        ));
    }

    let (namespaces, rev) = all_namespaces(&state.storage).await?;
    let mut items = Vec::new();
    for ns in namespaces {
        let name = ns["metadata"]["name"].as_str().unwrap_or_default().to_string();
        if all || is_member(&state.storage, &user, &name).await {
            items.push(namespace_to_project(ns));
        }
    }
    let items =
        crate::selector::filter_objects(items, &params.label_selector, &params.field_selector);
    let list = json!({
        "apiVersion": API_VERSION,
        "kind": "ProjectList",
        "metadata": { "resourceVersion": rev.to_string() },
        "items": items,
    });
    if crate::table::wants_table(&headers) {
        return Ok(Json(crate::table::to_table("projects", list)).into_response());
    }
    Ok(Json(list).into_response())
}

// ---------------------------------------------------------------------------
// The roles projects are shared with.
// ---------------------------------------------------------------------------

const READ: &[&str] = &["get", "list", "watch"];
const WRITE: &[&str] = &["create", "delete", "deletecollection", "patch", "update"];
const ALL: &[&str] = &[
    "get", "list", "watch", "create", "delete", "deletecollection", "patch", "update",
];

fn rule(groups: &[&str], resources: &[&str], verbs: &[&str]) -> Value {
    json!({ "apiGroups": groups, "resources": resources, "verbs": verbs })
}

/// What a project member works with, by API group: the namespaced kinds a
/// project holds. Secrets are not here — `view` does not read them.
const WORKLOADS: &[(&str, &[&str])] = &[
    ("", &[
        "pods", "services", "endpoints", "configmaps", "persistentvolumeclaims",
        "replicationcontrollers", "replicationcontrollers/scale", "serviceaccounts",
    ]),
    ("apps", &[
        "deployments", "deployments/scale", "replicasets", "replicasets/scale",
        "statefulsets", "statefulsets/scale", "daemonsets",
    ]),
    ("batch", &["jobs", "cronjobs"]),
    ("autoscaling", &["horizontalpodautoscalers"]),
    ("policy", &["poddisruptionbudgets"]),
    ("networking.k8s.io", &["ingresses", "networkpolicies"]),
    ("gateway.networking.k8s.io", &["gateways", "httproutes"]),
    ("route.openshift.io", &["routes"]),
    ("kubevirt.io", &["virtualmachines", "virtualmachineinstances"]),
    ("snapshot.storage.k8s.io", &["volumesnapshots"]),
];

/// Read-only on top of [`WORKLOADS`]: status, logs and what a namespace
/// reports about itself.
const OBSERVED: &[(&str, &[&str])] = &[
    ("", &[
        "pods/log", "pods/status", "services/status", "persistentvolumeclaims/status",
        "replicationcontrollers/status", "events", "limitranges", "resourcequotas",
        "resourcequotas/status", "bindings", "namespaces", "namespaces/status",
    ]),
    ("apps", &[
        "deployments/status", "replicasets/status", "statefulsets/status",
        "daemonsets/status", "controllerrevisions",
    ]),
    ("batch", &["jobs/status", "cronjobs/status"]),
    ("autoscaling", &["horizontalpodautoscalers/status"]),
    ("policy", &["poddisruptionbudgets/status"]),
    ("networking.k8s.io", &["ingresses/status"]),
    ("discovery.k8s.io", &["endpointslices"]),
    ("events.k8s.io", &["events"]),
    ("gateway.networking.k8s.io", &["gateways/status", "httproutes/status"]),
    ("route.openshift.io", &["routes/status"]),
];

fn view_rules() -> Vec<Value> {
    let mut rules: Vec<Value> = WORKLOADS
        .iter()
        .chain(OBSERVED)
        .map(|(g, rs)| rule(&[*g], rs, READ))
        .collect();
    rules.push(rule(&["project.openshift.io"], &["projects"], &["get"]));
    rules
}

fn edit_rules() -> Vec<Value> {
    let mut rules = view_rules();
    rules.extend(WORKLOADS.iter().map(|(g, rs)| rule(&[*g], rs, WRITE)));
    rules.push(rule(&[""], &["secrets"], ALL));
    // A shell in a pod, and the pod's ports — edit, not view, as upstream.
    rules.push(rule(
        &[""],
        &["pods/exec", "pods/attach", "pods/portforward"],
        &["get", "create"],
    ));
    rules.push(rule(&[""], &["pods/eviction"], &["create"]));
    rules.push(rule(
        &["subresources.kubevirt.io"],
        &["virtualmachines/start", "virtualmachines/stop", "virtualmachines/restart"],
        &["update"],
    ));
    rules.push(rule(
        &["subresources.kubevirt.io"],
        &["virtualmachineinstances/console", "virtualmachineinstances/vnc"],
        &["get"],
    ));
    rules
}

fn admin_rules() -> Vec<Value> {
    let mut rules = edit_rules();
    // Sharing the project: `oc adm policy add-role-to-user edit bob -n demo`.
    rules.push(rule(&["rbac.authorization.k8s.io"], &["roles", "rolebindings"], ALL));
    rules.push(rule(&["authorization.k8s.io"], &["localsubjectaccessreviews"], &["create"]));
    // The project itself. Authorized in its own namespace, so this reaches
    // this project and no other.
    rules.push(rule(&["project.openshift.io"], &["projects"], &["delete", "update"]));
    rules
}

fn cluster_role(name: &str, description: &str, rules: Vec<Value>) -> Value {
    json!({
        "apiVersion": "rbac.authorization.k8s.io/v1",
        "kind": "ClusterRole",
        "metadata": {
            "name": name,
            "annotations": { "openshift.io/description": description },
        },
        "rules": rules,
    })
}

/// The ClusterRoles projects are built on, reconciled at every boot.
///
/// `admin`, `edit` and `view` are the roles a project is shared with —
/// bound per project by a RoleBinding, never cluster-wide. `basic-user` lets
/// anyone who authenticated ask for their own projects; `self-provisioner`
/// lets them request one.
pub fn bootstrap_cluster_roles() -> Vec<Value> {
    vec![
        cluster_role(
            "admin",
            "A project's owner: everything in edit, plus sharing the project and deleting it.",
            admin_rules(),
        ),
        cluster_role(
            "edit",
            "Create, change and delete the workloads in a project; read its secrets.",
            edit_rules(),
        ),
        cluster_role(
            "view",
            "Read everything in a project except its secrets.",
            view_rules(),
        ),
        cluster_role(
            "basic-user",
            "List one's own projects (the server filters) and ask whether one may request one.",
            vec![
                rule(&["project.openshift.io"], &["projects"], &["list", "watch"]),
                rule(&["project.openshift.io"], &["projectrequests"], &["list"]),
            ],
        ),
        cluster_role(
            "self-provisioner",
            "Request a new project, becoming its admin.",
            vec![rule(&["project.openshift.io"], &["projectrequests"], &["create"])],
        ),
    ]
}

/// The ClusterRoleBindings that give every authenticated user `basic-user`
/// and `self-provisioner`. Created once and never rewritten: emptying the
/// subjects of `self-provisioners` turns self-service projects off, and a
/// boot must not turn it back on.
pub fn bootstrap_cluster_role_bindings() -> Vec<Value> {
    [("basic-users", "basic-user"), ("self-provisioners", "self-provisioner")]
        .iter()
        .map(|(name, role)| {
            json!({
                "apiVersion": "rbac.authorization.k8s.io/v1",
                "kind": "ClusterRoleBinding",
                "metadata": { "name": name },
                "roleRef": {
                    "apiGroup": "rbac.authorization.k8s.io",
                    "kind": "ClusterRole",
                    "name": role,
                },
                "subjects": [{
                    "kind": "Group",
                    "apiGroup": "rbac.authorization.k8s.io",
                    "name": "system:authenticated",
                }],
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(name: &str) -> UserInfo {
        UserInfo { username: name.into(), groups: vec!["system:authenticated".into()] }
    }

    #[test]
    fn a_project_is_its_namespace() {
        let ns = json!({
            "apiVersion": "v1", "kind": "Namespace",
            "metadata": {
                "name": "demo", "uid": "u1", "resourceVersion": "7",
                "annotations": { REQUESTER: "alice", DISPLAY_NAME: "Demo" },
            },
            "spec": { "finalizers": ["kubernetes"] },
            "status": { "phase": "Active" },
        });
        let p = namespace_to_project(ns);
        assert_eq!(p["apiVersion"], API_VERSION);
        assert_eq!(p["kind"], "Project");
        assert_eq!(p["metadata"]["name"], "demo");
        assert_eq!(p["metadata"]["uid"], "u1");
        assert_eq!(p["metadata"]["resourceVersion"], "7");
        assert_eq!(p["metadata"]["annotations"][REQUESTER], "alice");
        assert_eq!(p["spec"]["finalizers"], json!(["kubernetes"]));
        assert_eq!(p["status"]["phase"], "Active");
    }

    #[test]
    fn a_request_cannot_name_its_own_requester() {
        let ns = project_namespace(
            "demo",
            &json!({ "team": "a" }),
            &json!({ REQUESTER: "mallory", "x": "y" }),
            Some("Demo"),
            Some("a project"),
            Some("alice"),
        );
        let ann = &ns["metadata"]["annotations"];
        assert_eq!(ann[REQUESTER], "alice");
        assert_eq!(ann[DISPLAY_NAME], "Demo");
        assert_eq!(ann[DESCRIPTION], "a project");
        assert_eq!(ann["x"], "y");
        assert_eq!(ns["metadata"]["labels"]["team"], "a");

        // A directly created Project names no requester, whatever it says.
        let ns = project_namespace("demo", &Value::Null, &json!({ REQUESTER: "m" }), None, None, None);
        assert!(ns["metadata"]["annotations"].is_null());
    }

    #[test]
    fn the_requester_is_bound_admin_and_matches_as_themselves() {
        let alice = user("alice");
        let b = admin_binding("demo", &alice);
        assert_eq!(b["metadata"]["name"], ADMIN_BINDING);
        assert_eq!(b["metadata"]["namespace"], "demo");
        assert_eq!(b["roleRef"]["kind"], "ClusterRole");
        assert_eq!(b["roleRef"]["name"], "admin");
        assert!(subjects_match(&b, &alice));
        assert!(!subjects_match(&b, &user("bob")));

        let sa = user("system:serviceaccount:ci:builder");
        let b = admin_binding("demo", &sa);
        assert_eq!(b["subjects"][0]["kind"], "ServiceAccount");
        assert_eq!(b["subjects"][0]["namespace"], "ci");
        assert_eq!(b["subjects"][0]["name"], "builder");
        assert!(subjects_match(&b, &sa));
    }

    #[test]
    fn system_names_cannot_be_requested() {
        for r in ["default", "openshift", "kube-system", "kube-anything", "openshift-infra"] {
            assert!(is_reserved(r), "{r}");
        }
        for ok in ["demo", "kubefoo", "my-kube-app", "openshifty"] {
            assert!(!is_reserved(ok), "{ok}");
        }
    }

    #[test]
    fn project_names_are_dns_labels() {
        for ok in ["demo", "a", "team-1", &"a".repeat(63)] {
            assert!(validate_name(ok).is_ok(), "{ok}");
        }
        for bad in ["", "Demo", "-demo", "demo-", "de.mo", "de_mo", &"a".repeat(64)] {
            assert!(validate_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_project_update_changes_only_its_names() {
        let mut ns = json!({
            "metadata": {
                "name": "demo",
                "labels": { "pod-security.kubernetes.io/enforce": "restricted" },
                "annotations": { REQUESTER: "alice", DESCRIPTION: "old", "keep": "me" },
            },
        });
        let body = json!({
            "metadata": {
                "labels": { "pod-security.kubernetes.io/enforce": "privileged" },
                "annotations": { REQUESTER: "mallory", DISPLAY_NAME: "Demo", "keep": "changed" },
            },
        });
        apply_project_update(&mut ns, &body);
        let m = &ns["metadata"];
        assert_eq!(m["labels"]["pod-security.kubernetes.io/enforce"], "restricted");
        assert_eq!(m["annotations"][REQUESTER], "alice");
        assert_eq!(m["annotations"]["keep"], "me");
        assert_eq!(m["annotations"][DISPLAY_NAME], "Demo");
        assert!(m["annotations"][DESCRIPTION].is_null(), "an omitted description is cleared");
    }

    fn allows(role: &str, verb: &str, group: &str, resource: &str, sub: Option<&str>) -> bool {
        let r = bootstrap_cluster_roles()
            .into_iter()
            .find(|r| r["metadata"]["name"] == role)
            .expect("role exists");
        let req = AuthorizationRequest {
            verb: verb.into(),
            resource: resource.into(),
            subresource: sub.map(str::to_string),
            api_group: group.into(),
            namespace: Some("demo".into()),
            name: None,
        };
        r["rules"]
            .as_array()
            .unwrap()
            .iter()
            .any(|rule| crate::rbac_engine::rule_matches(rule, &req))
    }

    #[test]
    fn the_project_roles_nest() {
        // view reads, but not secrets, and cannot write or exec.
        assert!(allows("view", "list", "", "pods", None));
        assert!(allows("view", "get", "", "pods", Some("log")));
        assert!(allows("view", "get", "", "namespaces", None));
        assert!(allows("view", "get", "project.openshift.io", "projects", None));
        assert!(!allows("view", "get", "", "secrets", None));
        assert!(!allows("view", "create", "", "pods", None));
        assert!(!allows("view", "create", "", "pods", Some("exec")));

        // edit writes workloads and reads secrets, but cannot share the project.
        assert!(allows("edit", "create", "apps", "deployments", None));
        assert!(allows("edit", "get", "", "secrets", None));
        assert!(allows("edit", "create", "", "pods", Some("exec")));
        assert!(allows("edit", "update", "subresources.kubevirt.io", "virtualmachines", Some("start")));
        assert!(!allows("edit", "create", "rbac.authorization.k8s.io", "rolebindings", None));
        assert!(!allows("edit", "delete", "project.openshift.io", "projects", None));

        // admin shares and deletes the project — and still cannot write the
        // Namespace object, whose labels hold the pod-security level.
        assert!(allows("admin", "create", "rbac.authorization.k8s.io", "rolebindings", None));
        assert!(allows("admin", "delete", "project.openshift.io", "projects", None));
        assert!(!allows("admin", "update", "", "namespaces", None));
        assert!(!allows("admin", "delete", "", "namespaces", None));

        // Everyone: list own projects and request one; nothing else.
        assert!(allows("basic-user", "list", "project.openshift.io", "projects", None));
        assert!(!allows("basic-user", "get", "project.openshift.io", "projects", None));
        assert!(allows("self-provisioner", "create", "project.openshift.io", "projectrequests", None));
    }

    #[test]
    fn self_provisioning_is_granted_to_the_authenticated() {
        let bindings = bootstrap_cluster_role_bindings();
        let sp = bindings
            .iter()
            .find(|b| b["metadata"]["name"] == "self-provisioners")
            .expect("self-provisioners");
        assert_eq!(sp["roleRef"]["name"], "self-provisioner");
        assert!(subjects_match(sp, &user("anyone")));
        let nobody = UserInfo { username: "system:anonymous".into(), groups: vec![] };
        assert!(!subjects_match(sp, &nobody));
    }
}
