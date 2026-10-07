//! Admission webhooks: `MutatingWebhookConfiguration` and
//! `ValidatingWebhookConfiguration`, called on every write (#82).
//!
//! This module used to be a webhook client that no handler called, so a
//! stored configuration was accepted and had no effect: a validating policy
//! that should refuse a request let it through, and a mutating one (cert-
//! manager's, Kyverno's, a defaulting webhook) never ran.
//!
//! **How a write reaches it.** The RBAC middleware, once a request is
//! authorized, runs the rest of the request inside [`in_request`], which
//! records who is asking and what for (verb, group/version/resource,
//! subresource, namespace, name, dryRun) in a task-local. The write paths
//! call [`admit`] with the object they are about to store, after the built-in
//! admission (`builtin_admission`), as upstream runs its built-in plugins
//! before the webhooks:
//!
//! - CREATE: built-in and custom-resource POST, server-side apply's upsert,
//!   `pods/eviction`;
//! - UPDATE: PUT (`put_object`), and everything under `guaranteed_update` —
//!   PATCH of an object, PUT and PATCH of `/status` (with `subResource`
//!   `status`), approval;
//! - DELETE: DELETE of one object, and each object of a `deletecollection`.
//!
//! A write a handler makes that the request did not name — the Namespace a
//! ProjectRequest creates, the Pod an eviction deletes — is not admitted
//! under the request's attributes: [`admit`] checks the verb and the name.
//! Writes the apiserver makes for itself (bootstrap, manifests) have no
//! request and are not admitted, as upstream's are not.
//!
//! **What is honoured**, as upstream's dispatcher does: configurations in
//! name order, webhooks in their order; `rules` (operations, apiGroups,
//! apiVersions, resources with subresources and wildcards, scope);
//! `namespaceSelector` (the namespace's labels, or a Namespace's own);
//! `objectSelector` (new or old object); `failurePolicy` (default `Fail`);
//! `timeoutSeconds` (default 10, 1–30); `sideEffects` against a dry run;
//! `reinvocationPolicy: IfNeeded`; JSONPatch responses; `warnings` as
//! `Warning` headers. Validating webhooks are called in parallel; the first
//! refusal in configuration order is the answer. A webhook is reached by
//! `clientConfig.url`, or by `service`, through the Service's ClusterIP with
//! TLS verified for `<name>.<namespace>.svc` against `caBundle` (upstream's
//! default resolver).
//!
//! **Not honoured:** `matchConditions` are CEL, which this apiserver does not
//! evaluate; a webhook that has them is called as if they all matched (its
//! own handler still sees the request), never skipped. `matchPolicy` is
//! moot while each resource is served at one version. Only AdmissionReview
//! `v1` is spoken. Objects in `admissionregistration.k8s.io` are never sent
//! to webhooks (upstream exempts them, so a broken webhook cannot lock out
//! its own repair), nor are `events.k8s.io` writes, which are stored in
//! their core/v1 form.

use crate::auth::UserInfo;
use crate::error::ApiError;
use crate::handlers::AppState;
use crate::storage::ResourceStorage;
use crate::watch_cache::SnapshotVersion;
use axum::http::StatusCode;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::{debug, warn};

tokio::task_local! {
    static REQUEST: Arc<RequestAttrs>;
}

/// Who asked for a write, and what for: upstream's admission attributes.
#[derive(Debug)]
pub struct RequestAttrs {
    pub user: UserInfo,
    /// `create`, `update`, `patch` or `delete`.
    pub verb: &'static str,
    pub group: String,
    pub version: String,
    pub resource: String,
    pub subresource: Option<String>,
    pub namespace: Option<String>,
    pub name: Option<String>,
    pub dry_run: bool,
    /// The authorizer, for escalation prevention (#98). `None` where the
    /// request was not authorized by one (tests).
    pub rbac: Option<Arc<crate::rbac_engine::RbacEngine>>,
    warnings: Mutex<Vec<String>>,
}

impl RequestAttrs {
    /// The attributes of a write to the API, or `None` for anything else.
    pub fn of(
        path: &str,
        method: &axum::http::Method,
        query: Option<&str>,
        user: UserInfo,
    ) -> Option<Self> {
        let verb = match method.as_str() {
            "POST" => "create",
            "PUT" => "update",
            "PATCH" => "patch",
            "DELETE" => "delete",
            _ => return None,
        };
        let segments: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        let version = match segments.as_slice() {
            ["api", v, ..] => v.to_string(),
            ["apis", _, v, ..] => v.to_string(),
            _ => return None,
        };
        let (group, resource, namespace, name, subresource) =
            crate::rbac_engine::parse_path_segments(&segments)?;
        // `/api/v1/namespaces/{name}` is read as the namespace itself, for
        // RBAC; a Namespace is cluster-scoped and has no namespace here.
        let namespace = namespace.filter(|_| resource != "namespaces");
        let dry_run = form_urlencoded::parse(query.unwrap_or("").as_bytes())
            .any(|(k, v)| k == "dryRun" && v == "All");
        Some(Self {
            user,
            verb,
            group,
            version,
            resource,
            subresource,
            namespace,
            name,
            dry_run,
            rbac: None,
            warnings: Mutex::new(Vec::new()),
        })
    }
}

/// Add a `Warning` header to the current request's response (#121: the
/// fields `fieldValidation=Warn` pruned). Nothing outside a request.
pub fn warn(message: String) {
    let _ = REQUEST.try_with(|r| r.warnings.lock().unwrap().push(message));
}

/// Run a request's handler with its admission attributes, and return the
/// webhooks' warnings as `Warning` headers, as upstream does.
pub async fn in_request(
    attrs: Option<RequestAttrs>,
    handler: impl Future<Output = axum::response::Response>,
) -> axum::response::Response {
    let Some(attrs) = attrs else {
        return handler.await;
    };
    let attrs = Arc::new(attrs);
    let mut response = REQUEST.scope(attrs.clone(), handler).await;
    let warnings = std::mem::take(&mut *attrs.warnings.lock().unwrap());
    for warning in warnings {
        if let Ok(value) = axum::http::HeaderValue::from_str(&warning_header(&warning)) {
            response.headers_mut().append(axum::http::header::WARNING, value);
        }
    }
    response
}

/// `299 - "<text>"`, the text quoted and kept to printable ASCII.
fn warning_header(text: &str) -> String {
    let clean: String = text
        .chars()
        .map(|c| if c.is_ascii() && !c.is_ascii_control() { c } else { ' ' })
        .collect::<String>()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    format!("299 - \"{clean}\"")
}

/// What a write does, as webhooks' `rules.operations` name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    Create,
    Update,
    Delete,
}

impl Operation {
    fn as_str(self) -> &'static str {
        match self {
            Operation::Create => "CREATE",
            Operation::Update => "UPDATE",
            Operation::Delete => "DELETE",
        }
    }
    fn options_kind(self) -> &'static str {
        match self {
            Operation::Create => "CreateOptions",
            Operation::Update => "UpdateOptions",
            Operation::Delete => "DeleteOptions",
        }
    }
}

/// Run the mutating then the validating webhooks over a write the current
/// request is making. `object` is what will be stored (absent for DELETE),
/// and a mutating webhook's patch is applied to it; `old` is what is stored
/// now (absent for CREATE).
///
/// Does nothing outside a request, for a write the request did not name, or
/// when no webhook is configured.
pub async fn admit(
    state: &AppState,
    op: Operation,
    mut object: Option<&mut Value>,
    old: Option<&Value>,
) -> Result<(), ApiError> {
    let Ok(request) = REQUEST.try_with(Arc::clone) else {
        return Ok(());
    };
    let verb_matches = match op {
        // Server-side apply creates a missing object from a PATCH.
        Operation::Create => matches!(request.verb, "create" | "patch"),
        Operation::Update => matches!(request.verb, "update" | "patch"),
        Operation::Delete => request.verb == "delete",
    };
    if !verb_matches {
        return Ok(());
    }
    if let Some(name) = &request.name {
        let named = object
            .as_deref()
            .or(old)
            .and_then(|o| o["metadata"]["name"].as_str());
        if named.is_some_and(|n| n != name) {
            return Ok(());
        }
    }
    if matches!(
        request.group.as_str(),
        "admissionregistration.k8s.io" | "events.k8s.io"
    ) {
        return Ok(());
    }
    let mutating = state.admission.hooks(&state.storage, MUTATING).await;
    let validating = state.admission.hooks(&state.storage, VALIDATING).await;
    if mutating.is_empty() && validating.is_empty() {
        if let Some(object) = object.as_deref_mut() {
            after_mutating(&request, op, object, old).await?;
        }
        return Ok(());
    }

    let mut current = object.as_deref().cloned().unwrap_or(Value::Null);
    let identity = (
        current["metadata"]["name"].clone(),
        current["metadata"]["namespace"].clone(),
    );
    let mut call = Call {
        state,
        request: &request,
        op,
        old,
        namespace_labels: None,
    };

    // Mutating, in order; then once more for each `IfNeeded` webhook whose
    // output a later webhook changed.
    let mut reinvoke: Vec<(usize, Value)> = Vec::new();
    for (i, hook) in mutating.iter().enumerate() {
        if call.matches(hook, &current).await {
            call.mutate(hook, &mut current).await?;
            if hook.reinvoke_if_needed {
                reinvoke.push((i, current.clone()));
            }
        }
    }
    for (i, after) in reinvoke {
        if after != current {
            let hook = &mutating[i];
            if call.matches(hook, &current).await {
                call.mutate(hook, &mut current).await?;
            }
        }
    }
    if object.is_some() {
        after_mutating(&request, op, &mut current, old).await?;
    }
    if let Some(object) = object {
        // A patch cannot move the object: its key was chosen by its name.
        if current["metadata"].is_object() {
            for (field, value) in [("name", &identity.0), ("namespace", &identity.1)] {
                if value.is_string() {
                    current["metadata"][field] = value.clone();
                }
            }
        }
        *object = current.clone();
    }

    // Validating, in parallel; the first refusal in order is the answer.
    let mut matched = Vec::new();
    for hook in validating.iter() {
        if call.matches(hook, &current).await {
            matched.push(hook);
        }
    }
    let call = &call;
    let current = &current;
    let results = futures::future::join_all(
        matched
            .iter()
            .map(|hook| async move { (*hook, call.invoke(hook, current).await) }),
    )
    .await;
    for (hook, result) in results {
        match result {
            Ok(response) => {
                call.take_warnings(&response);
                if response["allowed"].as_bool() != Some(true) {
                    return Err(denied(&hook.name, &response["status"]));
                }
            }
            Err(e) => call.failed(hook, e)?,
        }
    }
    Ok(())
}

/// What the apiserver itself decides about an object once the mutating
/// webhooks are done with it, before the validating ones: the
/// `storage.storm.io` requester stamp (#210), then RBAC escalation
/// prevention (#98).
async fn after_mutating(
    request: &RequestAttrs,
    op: Operation,
    object: &mut Value,
    old: Option<&Value>,
) -> Result<(), ApiError> {
    if request.group == crate::requester::GROUP {
        match (op, old) {
            (Operation::Create, _) => crate::requester::on_create(object, &request.user),
            (Operation::Update, Some(stored)) => crate::requester::on_update(object, stored),
            _ => {}
        }
    }
    escalation(request, object, old).await
}

/// RBAC escalation prevention (#98) on a Role, ClusterRole or binding about
/// to be stored: judged after the mutating webhooks, on what will be stored,
/// and before the validating ones, as upstream's RBAC storage does.
async fn escalation(request: &RequestAttrs, object: &Value, old: Option<&Value>) -> Result<(), ApiError> {
    let Some(rbac) = &request.rbac else { return Ok(()) };
    if !crate::escalation::applies(&request.group, &request.resource, request.subresource.as_deref()) {
        return Ok(());
    }
    crate::escalation::confirm(
        rbac,
        &request.user,
        &request.resource,
        request.namespace.as_deref(),
        object,
        old,
    )
    .await
}

const MUTATING: &str = "/registry/mutatingwebhookconfigurations/";
const VALIDATING: &str = "/registry/validatingwebhookconfigurations/";

/// One admission check in flight.
struct Call<'a> {
    state: &'a AppState,
    request: &'a RequestAttrs,
    op: Operation,
    old: Option<&'a Value>,
    /// The request namespace's labels, read once when a selector needs them.
    namespace_labels: Option<Value>,
}

impl Call<'_> {
    /// Does `hook` want this request? Rules, then the selectors.
    async fn matches(&mut self, hook: &Hook, object: &Value) -> bool {
        let r = self.request;
        let namespaced = r.namespace.is_some();
        if !hook.rules.iter().any(|rule| {
            rule.matches(
                self.op,
                &r.group,
                &r.version,
                &r.resource,
                r.subresource.as_deref().unwrap_or(""),
                namespaced,
            )
        }) {
            return false;
        }
        if let Some(selector) = &hook.object_selector {
            let objects = [Some(object).filter(|o| !o.is_null()), self.old];
            if !objects
                .iter()
                .flatten()
                .any(|o| apimachinery::selector::matches(selector, &labels_of(o)))
            {
                return false;
            }
        }
        if let Some(selector) = &hook.namespace_selector {
            let labels = if r.resource == "namespaces" && r.namespace.is_none() {
                // A Namespace is matched by its own labels.
                labels_of(if object.is_null() { self.old.unwrap_or(object) } else { object })
            } else if let Some(ns) = &r.namespace {
                self.namespace_labels(ns).await
            } else {
                // Other cluster-scoped objects are never filtered by it.
                return true;
            };
            if !apimachinery::selector::matches(selector, &labels) {
                return false;
            }
        }
        true
    }

    async fn namespace_labels(&mut self, ns: &str) -> Value {
        if let Some(labels) = &self.namespace_labels {
            return labels.clone();
        }
        let key = ResourceStorage::cluster_key("namespaces", ns);
        let cached = self
            .state
            .storage
            .watch_cache()
            .get("/registry/namespaces/", &key)
            .await
            .ok()
            .flatten()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok());
        let namespace = match cached {
            Some(n) => Some(n),
            None => self.state.storage.get(&key).await.ok(),
        };
        let labels = namespace.map(|n| labels_of(&n)).unwrap_or_else(|| json!({}));
        self.namespace_labels = Some(labels.clone());
        labels
    }

    /// Call a mutating webhook and apply its patch.
    async fn mutate(&self, hook: &Hook, object: &mut Value) -> Result<(), ApiError> {
        let response = match self.invoke(hook, object).await {
            Ok(r) => r,
            Err(e) => return self.failed(hook, e),
        };
        self.take_warnings(&response);
        if response["allowed"].as_bool() != Some(true) {
            return Err(denied(&hook.name, &response["status"]));
        }
        let Some(patch) = response["patch"].as_str().filter(|p| !p.is_empty()) else {
            return Ok(());
        };
        if self.op == Operation::Delete {
            return Ok(());
        }
        // A patch that cannot be applied is an error whatever the failure
        // policy, as upstream: the webhook answered, and answered wrongly.
        let bad = |why: String| {
            ApiError::internal(&format!(
                "Internal error occurred: admission webhook \"{}\" returned an invalid patch: {why}",
                hook.name
            ))
        };
        match response["patchType"].as_str() {
            Some("JSONPatch") => {}
            other => return Err(bad(format!("unsupported patchType {other:?}"))),
        }
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(patch)
            .map_err(|e| bad(e.to_string()))?;
        let patch: json_patch::Patch =
            serde_json::from_slice(&bytes).map_err(|e| bad(e.to_string()))?;
        let mut patched = object.clone();
        json_patch::patch(&mut patched, &patch).map_err(|e| bad(e.to_string()))?;
        *object = patched;
        Ok(())
    }

    /// A call that did not answer: refused under `failurePolicy: Fail`,
    /// passed over under `Ignore`.
    fn failed(&self, hook: &Hook, error: CallError) -> Result<(), ApiError> {
        match error {
            CallError::Refused(e) => Err(e),
            CallError::Failed(why) if hook.fail_closed => Err(ApiError::internal(&format!(
                "Internal error occurred: failed calling webhook \"{}\": {why}",
                hook.name
            ))),
            CallError::Failed(why) => {
                warn!(webhook = %hook.name, %why, "admission webhook failed; ignored (failurePolicy Ignore)");
                Ok(())
            }
        }
    }

    fn take_warnings(&self, response: &Value) {
        if let Some(ws) = response["warnings"].as_array() {
            let mut out = self.request.warnings.lock().unwrap();
            out.extend(ws.iter().filter_map(Value::as_str).map(str::to_owned));
        }
    }

    /// Send the AdmissionReview and return its `response`.
    async fn invoke(&self, hook: &Hook, object: &Value) -> Result<Value, CallError> {
        let r = self.request;
        // Only DELETE honours dryRun in this apiserver; telling a webhook
        // a create is a dry run when it will be stored would be a lie.
        let dry_run = r.dry_run && self.op == Operation::Delete;
        if dry_run && !matches!(hook.side_effects.as_str(), "None" | "NoneOnDryRun") {
            return Err(CallError::Refused(ApiError::bad_request(&format!(
                "admission webhook \"{}\" does not support dry run",
                hook.name
            ))));
        }
        if !hook.review_versions.is_empty() && !hook.review_versions.iter().any(|v| v == "v1") {
            return Err(CallError::Failed(format!(
                "webhook does not accept admission.k8s.io/v1 AdmissionReview (accepts {:?})",
                hook.review_versions
            )));
        }
        let uid = uuid::Uuid::new_v4().to_string();
        let review = json!({
            "apiVersion": "admission.k8s.io/v1",
            "kind": "AdmissionReview",
            "request": review_request(r, self.op, &uid, object, self.old, dry_run),
        });
        let (url, client) = self
            .state
            .admission
            .endpoint(&self.state.storage, hook)
            .await
            .map_err(CallError::Failed)?;
        debug!(webhook = %hook.name, %url, op = self.op.as_str(), "calling admission webhook");
        let answer = client
            .post(&url)
            .timeout(hook.timeout)
            .json(&review)
            .send()
            .await
            .map_err(|e| CallError::Failed(format!("Post \"{url}\": {}", error_chain(&e))))?;
        let status = answer.status();
        if !status.is_success() {
            return Err(CallError::Failed(format!("webhook answered HTTP {status}")));
        }
        let body: Value = answer
            .json()
            .await
            .map_err(|e| CallError::Failed(format!("reading the AdmissionReview answer: {e}")))?;
        let response = body["response"].clone();
        if !response.is_object() {
            return Err(CallError::Failed("the AdmissionReview answer has no response".into()));
        }
        if response["uid"].as_str() != Some(uid.as_str()) {
            return Err(CallError::Failed(format!(
                "expected response.uid={uid:?}, got {}",
                response["uid"]
            )));
        }
        Ok(response)
    }
}

/// Why a webhook gave no answer.
enum CallError {
    /// Not called at all, whatever the failure policy (a dry run it cannot do).
    Refused(ApiError),
    /// Called and failed: unreachable, timed out, a bad answer.
    Failed(String),
}

/// The `request` half of an AdmissionReview.
fn review_request(
    r: &RequestAttrs,
    op: Operation,
    uid: &str,
    object: &Value,
    old: Option<&Value>,
    dry_run: bool,
) -> Value {
    // The kind is the object's own: a `/status` write is of the parent kind,
    // a `pods/eviction` of an Eviction.
    let typed = Some(object).filter(|o| !o.is_null()).or(old);
    let (kind_group, kind_version, kind) = match typed {
        Some(o) => {
            let api_version = o["apiVersion"].as_str().unwrap_or("");
            let (g, v) = api_version.rsplit_once('/').unwrap_or(("", api_version));
            let kind = o["kind"].as_str().map(str::to_owned).unwrap_or_else(|| {
                crate::handlers::resource::resource_to_kind(&r.resource)
            });
            let (g, v) = if v.is_empty() { (r.group.as_str(), r.version.as_str()) } else { (g, v) };
            (g.to_string(), v.to_string(), kind)
        }
        None => (
            r.group.clone(),
            r.version.clone(),
            crate::handlers::resource::resource_to_kind(&r.resource),
        ),
    };
    let gvk = json!({"group": kind_group, "version": kind_version, "kind": kind});
    let gvr = json!({"group": r.group, "version": r.version, "resource": r.resource});
    let name = r
        .name
        .clone()
        .or_else(|| typed.and_then(|o| o["metadata"]["name"].as_str()).map(str::to_owned));
    let mut req = json!({
        "uid": uid,
        "kind": gvk,
        "resource": gvr,
        "requestKind": gvk,
        "requestResource": gvr,
        "operation": op.as_str(),
        "userInfo": {"username": r.user.username, "groups": r.user.groups},
        "object": object,
        "oldObject": old.cloned().unwrap_or(Value::Null),
        "dryRun": dry_run,
        "options": {"apiVersion": "meta.k8s.io/v1", "kind": op.options_kind()},
    });
    if let Some(sub) = &r.subresource {
        req["subResource"] = json!(sub);
        req["requestSubResource"] = json!(sub);
    }
    if let Some(name) = name {
        req["name"] = json!(name);
    }
    if let Some(ns) = &r.namespace {
        req["namespace"] = json!(ns);
    }
    req
}

/// A refusal, worded and coded as upstream's `ToStatusErr`.
fn denied(webhook: &str, status: &Value) -> ApiError {
    let by = format!("admission webhook \"{webhook}\" denied the request");
    let message = status["message"].as_str().filter(|m| !m.is_empty());
    let reason = status["reason"].as_str().filter(|m| !m.is_empty());
    let message = match (message, reason) {
        (Some(m), _) => format!("{by}: {m}"),
        (None, Some(r)) => format!("{by}: {r}"),
        (None, None) => format!("{by} without explanation"),
    };
    let code = status["code"]
        .as_u64()
        .and_then(|c| u16::try_from(c).ok())
        .filter(|c| *c >= 400)
        .and_then(|c| StatusCode::from_u16(c).ok())
        .unwrap_or(StatusCode::BAD_REQUEST);
    ApiError {
        status: code,
        reason: reason.map(str::to_owned).unwrap_or_else(|| match code {
            StatusCode::FORBIDDEN => "Forbidden".into(),
            StatusCode::UNPROCESSABLE_ENTITY => "Invalid".into(),
            StatusCode::CONFLICT => "Conflict".into(),
            _ => "BadRequest".into(),
        }),
        message,
        continue_token: None,
    }
}

fn labels_of(object: &Value) -> Value {
    match &object["metadata"]["labels"] {
        Value::Object(m) => Value::Object(m.clone()),
        _ => json!({}),
    }
}

fn error_chain(e: &dyn std::error::Error) -> String {
    let mut out = e.to_string();
    let mut source = e.source();
    while let Some(s) = source {
        out.push_str(": ");
        out.push_str(&s.to_string());
        source = s.source();
    }
    out
}

/// One webhook of a configuration, as admission uses it.
#[derive(Debug, Clone)]
struct Hook {
    name: String,
    url: Option<String>,
    service: Option<ServiceRef>,
    /// PEM, decoded from `caBundle`.
    ca_bundle: Option<Vec<u8>>,
    rules: Vec<Rule>,
    fail_closed: bool,
    timeout: Duration,
    namespace_selector: Option<Value>,
    object_selector: Option<Value>,
    side_effects: String,
    review_versions: Vec<String>,
    reinvoke_if_needed: bool,
}

#[derive(Debug, Clone)]
struct ServiceRef {
    namespace: String,
    name: String,
    path: String,
    port: u16,
}

#[derive(Debug, Clone, Default)]
struct Rule {
    operations: Vec<String>,
    api_groups: Vec<String>,
    api_versions: Vec<String>,
    resources: Vec<String>,
    scope: String,
}

impl Rule {
    fn matches(
        &self,
        op: Operation,
        group: &str,
        version: &str,
        resource: &str,
        subresource: &str,
        namespaced: bool,
    ) -> bool {
        let has = |list: &[String], want: &str| list.iter().any(|x| x == "*" || x == want);
        has(&self.operations, op.as_str())
            && has(&self.api_groups, group)
            && has(&self.api_versions, version)
            // `pods` is the resource alone, `pods/status` one subresource,
            // `*` every resource and `*/*` everything; as upstream's Matcher.
            && self.resources.iter().any(|r| {
                let (res, sub) = r.split_once('/').unwrap_or((r.as_str(), ""));
                (res == "*" || res == resource) && (sub == "*" || sub == subresource)
            })
            && match self.scope.as_str() {
                "Cluster" => !namespaced,
                "Namespaced" => namespaced,
                _ => true,
            }
    }
}

/// Parse every webhook of a stored configuration, in its order.
fn parse_configuration(config: &Value, mutating: bool) -> Vec<Hook> {
    let strings = |v: &Value| -> Vec<String> {
        v.as_array()
            .map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect())
            .unwrap_or_default()
    };
    let selector = |v: &Value| -> Option<Value> {
        // Absent and `{}` both select everything.
        v.as_object().filter(|m| !m.is_empty()).map(|_| v.clone())
    };
    let mut out = Vec::new();
    for w in config["webhooks"].as_array().into_iter().flatten() {
        let Some(name) = w["name"].as_str() else { continue };
        let client = &w["clientConfig"];
        let service = client["service"].as_object().map(|s| ServiceRef {
            namespace: s.get("namespace").and_then(Value::as_str).unwrap_or("").into(),
            name: s.get("name").and_then(Value::as_str).unwrap_or("").into(),
            path: s.get("path").and_then(Value::as_str).unwrap_or("").into(),
            port: s
                .get("port")
                .and_then(Value::as_u64)
                .and_then(|p| u16::try_from(p).ok())
                .unwrap_or(443),
        });
        let ca_bundle = client["caBundle"].as_str().and_then(|b| {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD.decode(b).ok()
        });
        let rules = w["rules"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|r| Rule {
                operations: strings(&r["operations"]),
                api_groups: strings(&r["apiGroups"]),
                api_versions: strings(&r["apiVersions"]),
                resources: strings(&r["resources"]),
                scope: r["scope"].as_str().unwrap_or("*").into(),
            })
            .collect();
        out.push(Hook {
            name: name.into(),
            url: client["url"].as_str().map(str::to_owned),
            service,
            ca_bundle,
            rules,
            fail_closed: w["failurePolicy"].as_str() != Some("Ignore"),
            timeout: Duration::from_secs(w["timeoutSeconds"].as_u64().unwrap_or(10).clamp(1, 30)),
            namespace_selector: selector(&w["namespaceSelector"]),
            object_selector: selector(&w["objectSelector"]),
            side_effects: w["sideEffects"].as_str().unwrap_or("Unknown").into(),
            review_versions: strings(&w["admissionReviewVersions"]),
            reinvoke_if_needed: mutating && w["reinvocationPolicy"].as_str() == Some("IfNeeded"),
        });
    }
    out
}

/// The webhook configurations, parsed once per change, and the HTTP clients
/// that reach them. Shared by every request (`AppState::admission`).
#[derive(Default)]
pub struct Webhooks {
    views: Mutex<HashMap<&'static str, (SnapshotVersion, Arc<Vec<Hook>>)>>,
    clients: Mutex<HashMap<ClientKey, reqwest::Client>>,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct ClientKey {
    ca_bundle: Option<Vec<u8>>,
    resolve: Option<(String, SocketAddr)>,
}

impl Webhooks {
    /// The webhooks under `prefix`, configurations in name order — from the
    /// watch cache, which every apiserver replica keeps current, so a
    /// configuration applies to the next write after it is stored (the
    /// cache trails the write by its pump's few milliseconds, as upstream's
    /// informer does). The store answers when the cache cannot.
    async fn hooks(&self, storage: &ResourceStorage, prefix: &'static str) -> Arc<Vec<Hook>> {
        let mutating = prefix == MUTATING;
        let cache = storage.watch_cache();
        if let Ok(version) = cache.version(prefix).await {
            if let Some((v, hooks)) = self.views.lock().unwrap().get(prefix) {
                if *v == version {
                    return hooks.clone();
                }
            }
            if let Ok((version, items)) = cache.snapshot(prefix).await {
                let mut configs: Vec<(String, Value)> = items
                    .into_iter()
                    .filter_map(|(k, b)| serde_json::from_slice(&b).ok().map(|v| (k, v)))
                    .collect();
                configs.sort_by(|a, b| a.0.cmp(&b.0));
                let hooks: Arc<Vec<Hook>> = Arc::new(
                    configs.iter().flat_map(|(_, c)| parse_configuration(c, mutating)).collect(),
                );
                self.views.lock().unwrap().insert(prefix, (version, hooks.clone()));
                return hooks;
            }
        }
        let mut configs = Vec::new();
        let mut token: Option<String> = None;
        loop {
            match storage.list(prefix, 500, token.as_deref()).await {
                Ok((items, next, _)) => {
                    configs.extend(items);
                    match next {
                        Some(t) => token = Some(t),
                        None => break,
                    }
                }
                Err(e) => {
                    // No configuration can be read: nothing to call. Logged,
                    // because a webhook that should refuse is passed over.
                    warn!(%prefix, error = %e, "admission webhook configurations unreadable");
                    break;
                }
            }
        }
        configs.sort_by(|a, b| a["metadata"]["name"].as_str().cmp(&b["metadata"]["name"].as_str()));
        Arc::new(configs.iter().flat_map(|c| parse_configuration(c, mutating)).collect())
    }

    /// Where a webhook is, and a client that verifies it.
    async fn endpoint(
        &self,
        storage: &ResourceStorage,
        hook: &Hook,
    ) -> Result<(String, reqwest::Client), String> {
        let (url, resolve) = match (&hook.url, &hook.service) {
            (Some(url), _) => (url.clone(), None),
            (None, Some(s)) => {
                let key = ResourceStorage::namespaced_key("services", &s.namespace, &s.name);
                let svc = storage
                    .get(&key)
                    .await
                    .map_err(|e| format!("service {}/{}: {}", s.namespace, s.name, e.message))?;
                let ip: IpAddr = svc["spec"]["clusterIP"]
                    .as_str()
                    .and_then(|ip| ip.parse().ok())
                    .ok_or_else(|| {
                        format!("service {}/{} has no ClusterIP", s.namespace, s.name)
                    })?;
                let host = format!("{}.{}.svc", s.name, s.namespace);
                (
                    format!("https://{host}:{}{}", s.port, s.path),
                    Some((host, SocketAddr::new(ip, s.port))),
                )
            }
            (None, None) => return Err("clientConfig names neither url nor service".into()),
        };
        let key = ClientKey { ca_bundle: hook.ca_bundle.clone(), resolve };
        if let Some(client) = self.clients.lock().unwrap().get(&key) {
            return Ok((url, client.clone()));
        }
        let mut builder = reqwest::Client::builder().no_proxy();
        if let Some(pem) = &key.ca_bundle {
            builder = builder.tls_built_in_root_certs(false);
            let certs = reqwest::Certificate::from_pem_bundle(pem)
                .map_err(|e| format!("caBundle: {e}"))?;
            if certs.is_empty() {
                return Err("caBundle holds no certificate".into());
            }
            for cert in certs {
                builder = builder.add_root_certificate(cert);
            }
        }
        if let Some((host, addr)) = &key.resolve {
            builder = builder.resolve(host, *addr);
        }
        let client = builder.build().map_err(|e| format!("webhook client: {e}"))?;
        self.clients.lock().unwrap().insert(key, client.clone());
        Ok((url, client))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(ops: &[&str], resources: &[&str], scope: &str) -> Rule {
        Rule {
            operations: ops.iter().map(|s| s.to_string()).collect(),
            api_groups: vec!["".into()],
            api_versions: vec!["v1".into()],
            resources: resources.iter().map(|s| s.to_string()).collect(),
            scope: scope.into(),
        }
    }

    #[test]
    fn resources_match_as_upstream_with_subresources_apart() {
        let pods = rule(&["CREATE", "UPDATE"], &["pods"], "*");
        assert!(pods.matches(Operation::Create, "", "v1", "pods", "", true));
        assert!(pods.matches(Operation::Update, "", "v1", "pods", "", true));
        assert!(!pods.matches(Operation::Delete, "", "v1", "pods", "", true));
        assert!(!pods.matches(Operation::Update, "", "v1", "pods", "status", true));
        assert!(!pods.matches(Operation::Create, "", "v1", "services", "", true));
        assert!(!pods.matches(Operation::Create, "apps", "v1", "pods", "", true));

        let status = rule(&["*"], &["pods/status"], "*");
        assert!(status.matches(Operation::Update, "", "v1", "pods", "status", true));
        assert!(!status.matches(Operation::Update, "", "v1", "pods", "", true));

        let all = rule(&["*"], &["*"], "*");
        assert!(all.matches(Operation::Delete, "", "v1", "secrets", "", true));
        assert!(!all.matches(Operation::Update, "", "v1", "pods", "status", true));
        let everything = rule(&["*"], &["*/*"], "*");
        assert!(everything.matches(Operation::Update, "", "v1", "pods", "status", true));
        let any_status = rule(&["*"], &["*/status"], "*");
        assert!(any_status.matches(Operation::Update, "", "v1", "nodes", "status", false));
        assert!(!any_status.matches(Operation::Update, "", "v1", "nodes", "", false));
    }

    #[test]
    fn scope_separates_cluster_from_namespaced() {
        let cluster = rule(&["*"], &["*"], "Cluster");
        assert!(cluster.matches(Operation::Create, "", "v1", "nodes", "", false));
        assert!(!cluster.matches(Operation::Create, "", "v1", "pods", "", true));
        let namespaced = rule(&["*"], &["*"], "Namespaced");
        assert!(namespaced.matches(Operation::Create, "", "v1", "pods", "", true));
        assert!(!namespaced.matches(Operation::Create, "", "v1", "nodes", "", false));
    }

    #[test]
    fn a_configuration_parses_with_upstream_defaults() {
        let config = json!({"webhooks": [
            {"name": "a.example.com", "clientConfig": {"url": "https://x/a"},
             "rules": [{"operations": ["CREATE"], "apiGroups": [""], "apiVersions": ["v1"],
                        "resources": ["configmaps"]}],
             "sideEffects": "None", "admissionReviewVersions": ["v1"]},
            {"name": "b.example.com", "failurePolicy": "Ignore", "timeoutSeconds": 99,
             "reinvocationPolicy": "IfNeeded", "namespaceSelector": {},
             "objectSelector": {"matchLabels": {"x": "y"}},
             "clientConfig": {"service": {"namespace": "ns", "name": "svc", "path": "/m"},
                              "caBundle": "LS0tLS0="}}
        ]});
        let hooks = parse_configuration(&config, true);
        assert_eq!(hooks.len(), 2);
        let (a, b) = (&hooks[0], &hooks[1]);
        assert!(a.fail_closed);
        assert_eq!(a.timeout, Duration::from_secs(10));
        assert_eq!(a.rules[0].scope, "*");
        assert!(!a.reinvoke_if_needed);
        assert!(!b.fail_closed);
        assert_eq!(b.timeout, Duration::from_secs(30));
        assert!(b.reinvoke_if_needed);
        assert!(b.namespace_selector.is_none(), "{{}} selects everything");
        assert!(b.object_selector.is_some());
        let s = b.service.as_ref().unwrap();
        assert_eq!((s.namespace.as_str(), s.name.as_str(), s.path.as_str(), s.port), ("ns", "svc", "/m", 443));
        assert_eq!(b.ca_bundle.as_deref(), Some(&b"-----"[..]));
        // Validating webhooks have no reinvocation.
        assert!(!parse_configuration(&config, false)[1].reinvoke_if_needed);
    }

    #[test]
    fn a_refusal_reads_and_codes_as_upstream() {
        let e = denied("deny.example.com", &json!({"code": 403, "message": "no thanks"}));
        assert_eq!(e.status, StatusCode::FORBIDDEN);
        assert_eq!(e.message, "admission webhook \"deny.example.com\" denied the request: no thanks");
        let e = denied("d", &json!({"code": 200}));
        assert_eq!(e.status, StatusCode::BAD_REQUEST);
        assert_eq!(e.message, "admission webhook \"d\" denied the request without explanation");
        let e = denied("d", &Value::Null);
        assert_eq!(e.status, StatusCode::BAD_REQUEST);
        let e = denied("d", &json!({"reason": "Because"}));
        assert_eq!(e.message, "admission webhook \"d\" denied the request: Because");
    }

    #[test]
    fn write_attributes_come_from_the_path() {
        let user = || UserInfo { username: "u".into(), groups: vec![] };
        let post = axum::http::Method::POST;
        let a = RequestAttrs::of("/api/v1/namespaces/demo/configmaps", &post, None, user()).unwrap();
        assert_eq!((a.verb, a.group.as_str(), a.version.as_str(), a.resource.as_str()), ("create", "", "v1", "configmaps"));
        assert_eq!(a.namespace.as_deref(), Some("demo"));
        assert!(a.name.is_none() && !a.dry_run);
        let put = axum::http::Method::PUT;
        let a = RequestAttrs::of("/apis/apps/v1/namespaces/demo/deployments/web/status", &put, None, user()).unwrap();
        assert_eq!((a.group.as_str(), a.version.as_str(), a.resource.as_str()), ("apps", "v1", "deployments"));
        assert_eq!((a.name.as_deref(), a.subresource.as_deref()), (Some("web"), Some("status")));
        let del = axum::http::Method::DELETE;
        let a = RequestAttrs::of("/api/v1/namespaces/demo", &del, Some("dryRun=All"), user()).unwrap();
        assert_eq!(a.resource, "namespaces");
        assert!(a.namespace.is_none() && a.dry_run);
        assert!(RequestAttrs::of("/api/v1/pods", &axum::http::Method::GET, None, user()).is_none());
        assert!(RequestAttrs::of("/healthz", &post, None, user()).is_none());
    }

    #[test]
    fn the_review_names_the_object_kind_and_the_request_resource() {
        let attrs = RequestAttrs::of(
            "/api/v1/namespaces/demo/pods/web/eviction",
            &axum::http::Method::POST,
            None,
            UserInfo { username: "alice".into(), groups: vec!["devs".into()] },
        )
        .unwrap();
        let eviction = json!({"apiVersion": "policy/v1", "kind": "Eviction", "metadata": {"name": "web"}});
        let r = review_request(&attrs, Operation::Create, "u1", &eviction, None, false);
        assert_eq!(r["kind"], json!({"group": "policy", "version": "v1", "kind": "Eviction"}));
        assert_eq!(r["resource"], json!({"group": "", "version": "v1", "resource": "pods"}));
        assert_eq!(r["subResource"], "eviction");
        assert_eq!((r["name"].as_str(), r["namespace"].as_str()), (Some("web"), Some("demo")));
        assert_eq!(r["operation"], "CREATE");
        assert_eq!(r["userInfo"], json!({"username": "alice", "groups": ["devs"]}));
        assert_eq!(r["options"]["kind"], "CreateOptions");
        assert!(r["oldObject"].is_null());
    }

    #[test]
    fn warnings_are_quoted_printable_ascii() {
        assert_eq!(warning_header("say \"hi\"\n"), "299 - \"say \\\"hi\\\" \"");
    }
}
