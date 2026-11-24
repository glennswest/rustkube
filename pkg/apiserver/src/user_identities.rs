//! `user.openshift.io/v1` Users, Identities and UserIdentityMappings (#390, sc#50).
//!
//! What `oc create user|identity|useridentitymapping` writes and `oc get
//! user|identity|useridentitymapping` reads. Ported from
//! openshift/oauth-apiserver `pkg/user` (validation, the registry strategies,
//! `registry/user/etcd` and `registry/useridentitymapping/rest.go`) and
//! apiserver-library-go `apivalidation` (read 2026-11-24).
//!
//! - **Users** and **Identities** are cluster-scoped and stored by the
//!   generic cluster handlers, like Groups (#372); what is particular to them
//!   is here: OpenShift's strategies ([`admit_user`], [`admit_identity`],
//!   [`identity_name`]) and validation.
//!   A User's `groups` is always written `null` (the field is deprecated and
//!   cleared on every write; the `~` user echoes the requester's groups in
//!   it). An Identity is named `<providerName>:<providerUserName>` by the
//!   server, whatever the body says, and its `user` is `{}` until mapped.
//! - **`users/~`** ([`current_user`]) is the requester: the stored User of
//!   that name with the request's groups, or — for a name no User can have,
//!   such as a ServiceAccount's — a User built from the request alone.
//!   `oc whoami` reads it; `basic-user` grants every authenticated user
//!   `get` on `users` named `~`, as OpenShift's does.
//! - **UserIdentityMappings** are not stored. A mapping is the link between
//!   an Identity and a User: `identity.user` names the User (name and uid)
//!   and the User lists the Identity in `identities`. Create and update make
//!   that link (and drop the Identity from the User it was mapped to before),
//!   get reads it, delete undoes it. A mapping exists only while both halves
//!   agree; its name, uid and resourceVersion are the Identity's.
//!
//! Not here: an identity provider. Nothing in rustkube authenticates a login
//! through an Identity (there is no OAuth server); a User or Identity is a
//! record that `oc` and consoles read, and a User's name is what RBAC
//! subjects of kind User already match.

use crate::auth::UserInfo;
use crate::error::ApiError;
use crate::storage::ResourceStorage;
use crate::user_groups::path_segment_reasons;
use axum::http::StatusCode;
use serde_json::{json, Value};

pub const USERS: &str = "users";
pub const IDENTITIES: &str = "identities";
pub const MAPPINGS: &str = "useridentitymappings";
const API_VERSION: &str = "user.openshift.io/v1";
/// The resource OpenShift's mapping errors name (singular, as upstream).
const MAPPING_RESOURCE: &str = "useridentitymapping.user.openshift.io";

/// apiserver-library-go `ValidateUserName`.
pub(crate) fn user_name_reasons(name: &str) -> Vec<&'static str> {
    let reasons = path_segment_reasons(name);
    if !reasons.is_empty() {
        return reasons;
    }
    if name.contains(':') && !name.starts_with("b64:") {
        return vec![r#"usernames that contain ":" must begin with "b64:""#];
    }
    if name == "~" {
        return vec![r#"may not equal "~""#];
    }
    Vec::new()
}

/// oauth-apiserver `ValidateIdentityName`: `<providerName>:<providerUserName>`,
/// or `<providerName>:b64:<base64 user name>`.
fn identity_name_reasons(name: &str) -> Vec<&'static str> {
    let reasons = path_segment_reasons(name);
    if !reasons.is_empty() {
        return reasons;
    }
    let parts: Vec<&str> = name.split(':').collect();
    let user_index = if parts.len() == 3 && parts[1] == "b64" {
        2
    } else if parts.len() != 2 {
        return vec!["must be in the format <providerName>:<providerUserName>"];
    } else {
        1
    };
    if parts[0].is_empty() {
        return vec!["must be in the format <providerName>:<providerUserName> with a non-empty providerName"];
    }
    if parts[user_index].is_empty() {
        return vec!["must be in the format <providerName>:<providerUserName> with a non-empty providerUserName"];
    }
    Vec::new()
}

/// oauth-apiserver `ValidateIdentityProviderName`.
fn provider_name_reasons(name: &str) -> Vec<&'static str> {
    let reasons = path_segment_reasons(name);
    if !reasons.is_empty() {
        return reasons;
    }
    if name.contains(':') {
        return vec![r#"may not contain ":""#];
    }
    Vec::new()
}

/// A field.ErrorList, printed as apimachinery prints one.
#[derive(Default)]
struct Errs(Vec<String>);

impl Errs {
    fn invalid(&mut self, path: &str, value: &str, detail: &str) {
        self.0.push(format!("{path}: Invalid value: {value:?}: {detail}"));
    }
    fn required(&mut self, path: &str, detail: &str) {
        if detail.is_empty() {
            self.0.push(format!("{path}: Required value"));
        } else {
            self.0.push(format!("{path}: Required value: {detail}"));
        }
    }
    /// `ValidateObjectMeta` for a cluster-scoped object: the name (one
    /// error per reason, as upstream) and no namespace.
    fn meta(&mut self, obj: &Value, reasons: fn(&str) -> Vec<&'static str>) {
        let name = s(&obj["metadata"], "name");
        if name.is_empty() {
            self.required("metadata.name", "name or generateName is required");
        } else {
            for r in reasons(name) {
                self.invalid("metadata.name", name, r);
            }
        }
        if !s(&obj["metadata"], "namespace").is_empty() {
            self.0.push("metadata.namespace: Forbidden: not allowed on this type".into());
        }
    }
    /// NewInvalid: `<Kind>.user.openshift.io "<name>" is invalid: …`.
    fn into_result(mut self, kind: &str, name: &str) -> Result<(), ApiError> {
        if self.0.is_empty() {
            return Ok(());
        }
        let what = if self.0.len() == 1 { self.0.remove(0) } else { format!("[{}]", self.0.join(", ")) };
        Err(ApiError::invalid(&format!("{kind}.user.openshift.io {name:?} is invalid: {what}")))
    }
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v[k].as_str().unwrap_or("")
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array().into_iter().flatten().filter_map(|x| x.as_str().map(str::to_string)).collect()
}

/// The User strategy on every write (`PrepareForCreate`/`PrepareForUpdate`
/// clear `groups`) and `ValidateUser`. `identities` is omitted when empty,
/// as OpenShift serializes it.
pub fn admit_user(obj: &mut Value) -> Result<(), ApiError> {
    let Some(map) = obj.as_object_mut() else {
        return Err(ApiError::bad_request("a User must be an object"));
    };
    map.insert("groups".into(), Value::Null);
    if map.get("identities").is_some_and(|i| i.is_null() || i.as_array().is_some_and(|a| a.is_empty())) {
        map.remove("identities");
    }
    if map.get("fullName").is_some_and(|f| f.is_null() || f.as_str() == Some("")) {
        map.remove("fullName");
    }
    let mut errs = Errs::default();
    errs.meta(obj, user_name_reasons);
    if obj.get("fullName").is_some_and(|f| !f.is_string()) {
        errs.0.push(format!("fullName: Invalid value: {}: must be a string", obj["fullName"]));
    }
    match &obj["identities"] {
        Value::Null => {}
        Value::Array(ids) => {
            for (i, id) in ids.iter().enumerate() {
                let Some(id) = id.as_str() else {
                    errs.0.push(format!("identities[{i}]: Invalid value: {id}: must be a string"));
                    continue;
                };
                let reasons = identity_name_reasons(id);
                if !reasons.is_empty() {
                    errs.invalid(&format!("identities[{i}]"), id, &reasons.join(", "));
                }
            }
        }
        other => errs.0.push(format!("identities: Invalid value: {other}: must be a list of identity names")),
    }
    errs.into_result("User", s(&obj["metadata"], "name"))
}

/// The name the Identity strategy gives an Identity on create:
/// `<providerName>:<providerUserName>`, whatever the body said.
pub fn identity_name(obj: &mut Value) {
    let name = format!("{}:{}", s(obj, "providerName"), s(obj, "providerUserName"));
    if !obj["metadata"].is_object() {
        obj["metadata"] = json!({});
    }
    obj["metadata"]["name"] = json!(name);
}

/// `ValidateIdentity`, and `ValidateIdentityUpdate` when `old` is the stored
/// Identity. `user` is written `{}` when the Identity is not mapped.
pub fn admit_identity(obj: &mut Value, old: Option<&Value>) -> Result<(), ApiError> {
    let Some(map) = obj.as_object_mut() else {
        return Err(ApiError::bad_request("an Identity must be an object"));
    };
    if map.get("user").is_none_or(Value::is_null) {
        map.insert("user".into(), json!({}));
    }
    for k in ["providerName", "providerUserName"] {
        map.entry(k).or_insert(json!(""));
    }
    let mut errs = Errs::default();
    errs.meta(obj, identity_name_reasons);
    let provider = s(obj, "providerName");
    if provider.is_empty() {
        errs.required("providerName", "");
    } else {
        let r = provider_name_reasons(provider);
        if !r.is_empty() {
            errs.invalid("providerName", provider, &r.join(", "));
        }
    }
    let login = s(obj, "providerUserName");
    if login.is_empty() {
        errs.required("providerUserName", "");
    } else {
        let r = user_name_reasons(login);
        if !r.is_empty() {
            errs.invalid("providerUserName", login, &r.join(", "));
        }
    }
    let name = s(&obj["metadata"], "name");
    if !provider.is_empty() && !login.is_empty() {
        let want = format!("{provider}:{login}");
        if name != want {
            errs.invalid("metadata.name", name, &format!("must be {want}"));
        }
    }
    let user = &obj["user"];
    if !user.is_object() {
        errs.0.push(format!("user: Invalid value: {user}: must be an object reference"));
    } else {
        let (uname, uid) = (s(user, "name"), s(user, "uid"));
        let r = user_name_reasons(uname);
        if !uname.is_empty() && !r.is_empty() {
            errs.invalid("user.name", uname, &r.join(", "));
        }
        if uname.is_empty() && !uid.is_empty() {
            errs.required("user.username", "username is required when uid is provided");
        }
        if !uname.is_empty() && uid.is_empty() {
            errs.required("user.uid", "uid is required when username is provided");
        }
    }
    if let Some(old) = old {
        if s(old, "providerName") != provider {
            errs.invalid("providerName", provider, "may not change providerName");
        }
        if s(old, "providerUserName") != login {
            errs.invalid("providerUserName", login, "may not change providerUserName");
        }
    }
    errs.into_result("Identity", name)
}

/// GET `users/~`: the requester, as OpenShift's user registry answers it.
pub async fn current_user(storage: &ResourceStorage, user: &UserInfo) -> Result<Value, ApiError> {
    if user.username.is_empty() {
        return Err(ApiError::forbidden(
            r#"user.user.openshift.io "~" is forbidden: requests to ~ must be authenticated"#,
        ));
    }
    let mut groups = user.groups.clone();
    groups.sort();
    groups.dedup();
    let virtual_user = json!({
        "kind": "User",
        "apiVersion": API_VERSION,
        "metadata": {"name": user.username, "creationTimestamp": null},
        "groups": groups,
    });
    // A name no stored User can have (a ServiceAccount, `system:admin`).
    if !user_name_reasons(&user.username).is_empty() {
        return Ok(virtual_user);
    }
    match storage.get(&ResourceStorage::cluster_key(USERS, &user.username)).await {
        Ok(mut stored) => {
            stored["groups"] = virtual_user["groups"].clone();
            Ok(stored)
        }
        // Impersonation, a token or certificate user: no User object.
        Err(e) if e.status == StatusCode::NOT_FOUND => Ok(virtual_user),
        Err(e) => Err(ApiError::internal(&e.message)),
    }
}

/// GET of a User by name: one no User can have is a 422, as OpenShift
/// answers it rather than looking.
pub fn check_user_get(name: &str) -> Result<(), ApiError> {
    let reasons = user_name_reasons(name);
    if reasons.is_empty() {
        return Ok(());
    }
    let mut errs = Errs::default();
    for r in reasons {
        errs.invalid("metadata.name", name, r);
    }
    errs.into_result("User", name)
}

fn rev_of(obj: &Value) -> Option<u64> {
    obj["metadata"]["resourceVersion"].as_str().and_then(|r| r.parse().ok())
}

async fn get_opt(storage: &ResourceStorage, key: &str) -> Result<Option<Value>, ApiError> {
    match storage.get(key).await {
        Ok(v) => Ok(Some(v)),
        Err(e) if e.status == StatusCode::NOT_FOUND => Ok(None),
        Err(e) => Err(e),
    }
}

/// The Identity named `name`, the User it names, and the mapping when both
/// halves agree (`getRelatedObjects`).
struct Related {
    identity: Option<Value>,
    user: Option<Value>,
    mapping: Option<Value>,
}

async fn related(storage: &ResourceStorage, name: &str) -> Result<Related, ApiError> {
    let mut r = Related { identity: None, user: None, mapping: None };
    r.identity = get_opt(storage, &ResourceStorage::cluster_key(IDENTITIES, name)).await?;
    let Some(identity) = &r.identity else { return Ok(r) };
    let uname = s(&identity["user"], "name");
    if uname.is_empty() {
        return Ok(r);
    }
    r.user = get_opt(storage, &ResourceStorage::cluster_key(USERS, uname)).await?;
    if let Some(user) = &r.user {
        let references_user = s(&user["metadata"], "name") == uname
            && s(&user["metadata"], "uid") == s(&identity["user"], "uid");
        if references_user && strings(&user["identities"]).iter().any(|i| i == name) {
            r.mapping = Some(mapping_for(user, identity));
        }
    }
    Ok(r)
}

/// `mappingFor`: the mapping's name, uid, resourceVersion and creation time
/// are the Identity's.
fn mapping_for(user: &Value, identity: &Value) -> Value {
    let im = &identity["metadata"];
    json!({
        "kind": "UserIdentityMapping",
        "apiVersion": API_VERSION,
        "metadata": {
            "name": im["name"],
            "uid": im["uid"],
            "resourceVersion": im["resourceVersion"],
            "creationTimestamp": im["creationTimestamp"],
        },
        "identity": {"name": im["name"], "uid": im["uid"]},
        "user": {"name": user["metadata"]["name"], "uid": user["metadata"]["uid"]},
    })
}

fn mapping_not_found(name: &str) -> ApiError {
    ApiError::not_found(MAPPING_RESOURCE, name)
}

/// GET `useridentitymappings/{name}`.
pub async fn get_mapping(storage: &ResourceStorage, name: &str) -> Result<Value, ApiError> {
    related(storage, name).await?.mapping.ok_or_else(|| mapping_not_found(name))
}

/// The mapping strategy's `PrepareForCreate` / `PrepareForUpdate`: the name
/// defaults to the identity's; namespaces, kinds and the user's uid (and,
/// on create, the resourceVersion; on update, the identity's uid) are not
/// the client's to set.
fn prepare(body: &mut Value, create: bool) -> Result<(), ApiError> {
    if !body.is_object() {
        return Err(ApiError::bad_request("a UserIdentityMapping must be an object"));
    }
    if !body["metadata"].is_object() {
        body["metadata"] = json!({});
    }
    for k in ["identity", "user"] {
        if body[k].is_null() {
            body[k] = json!({});
        }
        if !body[k].is_object() {
            return Err(ApiError::bad_request(&format!("{k} must be an object reference")));
        }
    }
    if s(&body["metadata"], "name").is_empty() {
        body["metadata"]["name"] = body["identity"]["name"].clone();
    }
    let meta = body["metadata"].as_object_mut().expect("an object");
    meta.remove("namespace");
    if create {
        meta.remove("resourceVersion");
    }
    let id = body["identity"].as_object_mut().expect("an object");
    id.remove("namespace");
    id.remove("kind");
    if !create {
        id.remove("uid");
    }
    let user = body["user"].as_object_mut().expect("an object");
    for k in ["namespace", "kind", "uid"] {
        user.remove(k);
    }
    Ok(())
}

/// `ValidateUserIdentityMapping`.
fn validate_mapping(m: &Value) -> Result<(), ApiError> {
    let mut errs = Errs::default();
    errs.meta(m, identity_name_reasons);
    let name = s(&m["metadata"], "name");
    let identity = s(&m["identity"], "name");
    if identity.is_empty() {
        errs.required("identity.name", "");
    } else if identity != name {
        errs.invalid("identity.name", identity, "must match metadata.name");
    }
    let user = s(&m["user"], "name");
    if user.is_empty() {
        errs.required("user.name", "");
    } else {
        let r = user_name_reasons(user);
        if !r.is_empty() {
            errs.invalid("user.name", user, &r.join(", "));
        }
    }
    errs.into_result("UserIdentityMapping", name)
}

/// A mapping's `<Kind>.user.openshift.io "<name>" is invalid` for a missing
/// Identity or User it names.
fn missing_reference(name: &str, path: &str, value: &str, what: &str) -> ApiError {
    let mut errs = Errs::default();
    errs.invalid(path, value, &format!("referenced {what} does not exist"));
    errs.into_result("UserIdentityMapping", name).unwrap_err()
}

/// `identities` with `name` added or removed, sorted (upstream's string
/// set); `None` when it was already so.
fn with_identity(user: &Value, name: &str, add: bool) -> Option<Vec<String>> {
    let mut ids = strings(&user["identities"]);
    let has = ids.iter().any(|i| i == name);
    if has == add {
        return None;
    }
    if add {
        ids.push(name.to_string());
    } else {
        ids.retain(|i| i != name);
    }
    ids.sort();
    ids.dedup();
    Some(ids)
}

async fn write_user_identities(storage: &ResourceStorage, mut user: Value, ids: Vec<String>) -> Result<Value, ApiError> {
    let key = ResourceStorage::cluster_key(USERS, s(&user["metadata"], "name"));
    if ids.is_empty() {
        if let Some(m) = user.as_object_mut() {
            m.remove("identities");
        }
    } else {
        user["identities"] = json!(ids);
    }
    let rev = rev_of(&user);
    storage.update(&key, user, rev).await
}

async fn write_identity_user(storage: &ResourceStorage, mut identity: Value, user: Value) -> Result<Value, ApiError> {
    let key = ResourceStorage::cluster_key(IDENTITIES, s(&identity["metadata"], "name"));
    identity["user"] = user;
    let rev = rev_of(&identity);
    storage.update(&key, identity, rev).await
}

/// POST (`create`) or PUT (`!create`) of a mapping: `createOrUpdate`.
/// Returns the mapping and whether it was created. A dry run checks
/// everything and writes nothing.
pub async fn create_or_update(
    storage: &ResourceStorage,
    mut mapping: Value,
    create: bool,
    dry_run: bool,
) -> Result<(Value, bool), ApiError> {
    prepare(&mut mapping, create)?;
    let name = s(&mapping["metadata"], "name").to_string();
    let rel = related(storage, &name).await?;
    if create && rel.mapping.is_some() {
        return Err(ApiError::already_exists(MAPPING_RESOURCE, &name));
    }
    let creating = create || rel.mapping.is_none();
    validate_mapping(&mapping)?;
    let requested_rv = s(&mapping["metadata"], "resourceVersion");
    if creating {
        // An update that names a version of a mapping that does not exist.
        if !requested_rv.is_empty() {
            return Err(mapping_not_found(&name));
        }
    } else {
        let old = rel.mapping.as_ref().expect("a mapping");
        let old_rv = s(&old["metadata"], "resourceVersion");
        if !requested_rv.is_empty() && requested_rv != old_rv {
            return Err(ApiError::conflict(&format!(
                "Operation cannot be fulfilled on {MAPPING_RESOURCE} {name:?}: the resource was updated to {old_rv}"
            )));
        }
        if s(&mapping["user"], "name") == s(&old["user"], "name") {
            return Ok((old.clone(), false));
        }
    }
    let Some(mut identity) = rel.identity else {
        return Err(missing_reference(&name, "identity.name", s(&mapping["identity"], "name"), "identity"));
    };
    let uname = s(&mapping["user"], "name").to_string();
    let Some(mut user) = get_opt(storage, &ResourceStorage::cluster_key(USERS, &uname)).await? else {
        return Err(missing_reference(&name, "user.name", &uname, "user"));
    };
    let user_ref = json!({"name": user["metadata"]["name"], "uid": user["metadata"]["uid"]});
    if dry_run {
        identity["user"] = user_ref;
        if let Some(ids) = with_identity(&user, &name, true) {
            user["identities"] = json!(ids);
        }
        return Ok((mapping_for(&user, &identity), creating));
    }
    // The new User lists the Identity first: a failure here leaves nothing
    // half-done, and a retry picks up where this stopped.
    if let Some(ids) = with_identity(&user, &name, true) {
        user = write_user_identities(storage, user, ids).await?;
    }
    if identity["user"] != user_ref {
        identity = write_identity_user(storage, identity, user_ref).await?;
    }
    // Cleanup: the User the Identity was mapped to before no longer lists it.
    if let Some(old) = rel.user.filter(|o| s(&o["metadata"], "name") != uname) {
        if let Some(ids) = with_identity(&old, &name, false) {
            let old_name = s(&old["metadata"], "name").to_string();
            if let Err(e) = write_user_identities(storage, old, ids).await {
                tracing::warn!("useridentitymappings: error removing identity reference {name} from user {old_name}: {}", e.message);
            }
        }
    }
    Ok((mapping_for(&user, &identity), creating))
}

/// DELETE `useridentitymappings/{name}`: the User stops listing the
/// Identity, then the Identity stops naming the User.
pub async fn delete_mapping(storage: &ResourceStorage, name: &str, dry_run: bool) -> Result<Value, ApiError> {
    let rel = related(storage, name).await?;
    if rel.mapping.is_none() {
        return Err(mapping_not_found(name));
    }
    let (identity, user) = (rel.identity.expect("mapped"), rel.user.expect("mapped"));
    if !dry_run {
        let uname = s(&user["metadata"], "name").to_string();
        if let Some(ids) = with_identity(&user, name, false) {
            write_user_identities(storage, user, ids).await?;
        }
        if let Err(e) = write_identity_user(storage, identity, json!({})).await {
            tracing::warn!("useridentitymappings: error removing user reference {uname} from identity {name}: {}", e.message);
        }
    }
    Ok(json!({"kind": "Status", "apiVersion": "v1", "metadata": {}, "status": "Success"}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn storage() -> ResourceStorage {
        ResourceStorage::new(Arc::new(crate::test_store::MemStore::default()))
    }

    fn err(r: Result<(), ApiError>) -> String {
        r.unwrap_err().message
    }

    #[test]
    fn a_user_is_written_as_openshift_writes_it() {
        let mut u = json!({"apiVersion": API_VERSION, "kind": "User", "metadata": {"name": "u1"},
                           "fullName": "U", "identities": [], "groups": ["devs"]});
        admit_user(&mut u).unwrap();
        assert_eq!(u["groups"], Value::Null, "groups is cleared and written null");
        assert!(u.as_object().unwrap().contains_key("groups"));
        assert!(!u.as_object().unwrap().contains_key("identities"), "omitted when empty");
        assert_eq!(u["fullName"], "U");
        let mut u = json!({"metadata": {"name": "b64:YTpi"}, "identities": ["p:login", "p:b64:YTpi"]});
        admit_user(&mut u).unwrap();
    }

    #[test]
    fn user_names_and_identities_are_refused_as_openshift_refuses_them() {
        let msg = |name: &str| err(admit_user(&mut json!({"metadata": {"name": name}})));
        assert_eq!(
            msg("a:b"),
            r#"User.user.openshift.io "a:b" is invalid: metadata.name: Invalid value: "a:b": usernames that contain ":" must begin with "b64:""#
        );
        assert!(msg("system:serviceaccount:ns:sa").contains("must begin with \"b64:\""), "no User for a ServiceAccount");
        assert!(msg("~").contains(r#"may not equal "~""#));
        assert!(msg("a/b").contains("may not contain '/'"));
        assert!(msg("").contains("metadata.name: Required value: name or generateName is required"));
        let ids = |ids: Value| err(admit_user(&mut json!({"metadata": {"name": "u"}, "identities": ids})));
        assert_eq!(
            ids(json!(["login"])),
            r#"User.user.openshift.io "u" is invalid: identities[0]: Invalid value: "login": must be in the format <providerName>:<providerUserName>"#
        );
        assert!(ids(json!([":x"])).contains("with a non-empty providerName"));
        assert!(ids(json!(["p:"])).contains("with a non-empty providerUserName"));
        assert!(ids(json!(["p:b64:"])).contains("with a non-empty providerUserName"));
        assert!(ids(json!(["p:a:b"])).contains("must be in the format"));
        assert!(err(admit_user(&mut json!({"metadata": {"name": "u", "namespace": "x"}})))
            .contains("metadata.namespace: Forbidden"));
    }

    #[test]
    fn an_identity_is_named_by_its_provider_and_login() {
        let mut i = json!({"apiVersion": API_VERSION, "kind": "Identity", "metadata": {"name": "ignored"},
                           "providerName": "p", "providerUserName": "login"});
        identity_name(&mut i);
        assert_eq!(i["metadata"]["name"], "p:login");
        admit_identity(&mut i, None).unwrap();
        assert_eq!(i["user"], json!({}), "unmapped: user is {{}}");
        // As `oc create identity` sends it: no metadata at all.
        let mut i = json!({"providerName": "p", "providerUserName": "login"});
        identity_name(&mut i);
        admit_identity(&mut i, None).unwrap();
        assert_eq!(i["metadata"]["name"], "p:login");
    }

    #[test]
    fn identities_are_refused_as_openshift_refuses_them() {
        let check = |body: Value| {
            let mut i = body;
            identity_name(&mut i);
            err(admit_identity(&mut i, None))
        };
        assert_eq!(
            check(json!({"providerName": "", "providerUserName": "login"})),
            r#"Identity.user.openshift.io ":login" is invalid: [metadata.name: Invalid value: ":login": must be in the format <providerName>:<providerUserName> with a non-empty providerName, providerName: Required value]"#
        );
        assert!(check(json!({"providerName": "p", "providerUserName": "a:b"}))
            .contains(r#"providerUserName: Invalid value: "a:b": usernames that contain ":" must begin with "b64:""#));
        assert!(check(json!({"providerName": "p/q", "providerUserName": "x"})).contains("providerName: Invalid value: \"p/q\": may not contain '/'"));
        assert!(check(json!({"providerName": "p", "providerUserName": "x", "user": {"name": "u"}}))
            .contains("user.uid: Required value: uid is required when username is provided"));
        assert!(check(json!({"providerName": "p", "providerUserName": "x", "user": {"uid": "1"}}))
            .contains("user.username: Required value: username is required when uid is provided"));
        // A body that names itself otherwise (server-side apply to a URL name).
        let mut i = json!({"metadata": {"name": "p:other"}, "providerName": "p", "providerUserName": "x"});
        assert!(err(admit_identity(&mut i, None)).contains(r#"metadata.name: Invalid value: "p:other": must be p:x"#));
        // Update: provider and login are fixed.
        let old = json!({"providerName": "p", "providerUserName": "x"});
        let mut i = json!({"metadata": {"name": "q:x"}, "providerName": "q", "providerUserName": "x"});
        assert!(err(admit_identity(&mut i, Some(&old))).contains("providerName: Invalid value: \"q\": may not change providerName"));
    }

    async fn put_user(st: &ResourceStorage, name: &str) -> Value {
        let mut u = json!({"apiVersion": API_VERSION, "kind": "User", "metadata": {"name": name, "uid": format!("uid-{name}")}});
        admit_user(&mut u).unwrap();
        st.create(&ResourceStorage::cluster_key(USERS, name), u).await.unwrap()
    }

    async fn put_identity(st: &ResourceStorage, provider: &str, login: &str) -> Value {
        let mut i = json!({"apiVersion": API_VERSION, "kind": "Identity", "providerName": provider, "providerUserName": login});
        identity_name(&mut i);
        i["metadata"]["uid"] = json!(format!("uid-{provider}:{login}"));
        admit_identity(&mut i, None).unwrap();
        let key = ResourceStorage::cluster_key(IDENTITIES, s(&i["metadata"], "name"));
        st.create(&key, i).await.unwrap()
    }

    async fn get(st: &ResourceStorage, resource: &str, name: &str) -> Value {
        st.get(&ResourceStorage::cluster_key(resource, name)).await.unwrap()
    }

    /// As `oc create useridentitymapping p:login u1` sends it.
    fn oc_mapping(identity: &str, user: &str) -> Value {
        json!({"apiVersion": API_VERSION, "kind": "UserIdentityMapping", "metadata": {"creationTimestamp": null},
               "identity": {"name": identity}, "user": {"name": user}})
    }

    #[tokio::test]
    async fn a_mapping_links_both_halves_and_is_read_back() {
        let st = storage();
        put_user(&st, "u1").await;
        put_identity(&st, "p", "login").await;
        assert_eq!(get_mapping(&st, "p:login").await.unwrap_err().message,
            r#"useridentitymapping.user.openshift.io "p:login" not found"#);

        let (m, created) = create_or_update(&st, oc_mapping("p:login", "u1"), true, false).await.unwrap();
        assert!(created);
        assert_eq!(m["metadata"]["name"], "p:login");
        assert_eq!(m["identity"], json!({"name": "p:login", "uid": "uid-p:login"}));
        assert_eq!(m["user"], json!({"name": "u1", "uid": "uid-u1"}));
        assert_eq!(get(&st, USERS, "u1").await["identities"], json!(["p:login"]));
        let identity = get(&st, IDENTITIES, "p:login").await;
        assert_eq!(identity["user"], json!({"name": "u1", "uid": "uid-u1"}));
        assert_eq!(m["metadata"]["resourceVersion"], identity["metadata"]["resourceVersion"], "the Identity's version");
        assert_eq!(get_mapping(&st, "p:login").await.unwrap(), m);

        let e = create_or_update(&st, oc_mapping("p:login", "u1"), true, false).await.unwrap_err();
        assert_eq!(e.message, r#"useridentitymapping.user.openshift.io "p:login" already exists"#);
        assert_eq!(e.status, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn a_mapping_to_nothing_is_refused() {
        let st = storage();
        put_user(&st, "u1").await;
        let e = create_or_update(&st, oc_mapping("p:login", "u1"), true, false).await.unwrap_err();
        assert_eq!(e.message, r#"UserIdentityMapping.user.openshift.io "p:login" is invalid: identity.name: Invalid value: "p:login": referenced identity does not exist"#);
        put_identity(&st, "p", "login").await;
        let e = create_or_update(&st, oc_mapping("p:login", "ghost"), true, false).await.unwrap_err();
        assert_eq!(e.message, r#"UserIdentityMapping.user.openshift.io "p:login" is invalid: user.name: Invalid value: "ghost": referenced user does not exist"#);
        let e = create_or_update(&st, oc_mapping("p:login", ""), true, false).await.unwrap_err();
        assert!(e.message.contains("user.name: Required value"), "{}", e.message);
        let mut m = oc_mapping("p:login", "u1");
        m["metadata"]["name"] = json!("p:other");
        let e = create_or_update(&st, m, true, false).await.unwrap_err();
        assert!(e.message.contains(r#"identity.name: Invalid value: "p:login": must match metadata.name"#), "{}", e.message);
        assert!(get_opt(&st, &ResourceStorage::cluster_key(USERS, "u1")).await.unwrap().unwrap().get("identities").is_none());
    }

    #[tokio::test]
    async fn an_update_moves_the_identity_and_a_delete_undoes_it() {
        let st = storage();
        put_user(&st, "u1").await;
        put_user(&st, "u2").await;
        put_identity(&st, "p", "login").await;
        create_or_update(&st, oc_mapping("p:login", "u1"), true, false).await.unwrap();
        // The same user: nothing to do, not created.
        let (m, created) = create_or_update(&st, oc_mapping("p:login", "u1"), false, false).await.unwrap();
        assert!(!created);
        assert_eq!(m["user"]["name"], "u1");
        // A stale version is a conflict.
        let mut stale = oc_mapping("p:login", "u2");
        stale["metadata"]["resourceVersion"] = json!("1");
        assert_eq!(create_or_update(&st, stale, false, false).await.unwrap_err().status, StatusCode::CONFLICT);
        let (m, created) = create_or_update(&st, oc_mapping("p:login", "u2"), false, false).await.unwrap();
        assert!(!created);
        assert_eq!(m["user"], json!({"name": "u2", "uid": "uid-u2"}));
        assert!(get(&st, USERS, "u1").await.get("identities").is_none(), "the old user lets it go");
        assert_eq!(get(&st, USERS, "u2").await["identities"], json!(["p:login"]));

        // Dry run: answered, nothing written.
        let st_before = get(&st, IDENTITIES, "p:login").await;
        delete_mapping(&st, "p:login", true).await.unwrap();
        assert_eq!(get(&st, IDENTITIES, "p:login").await, st_before);

        let status = delete_mapping(&st, "p:login", false).await.unwrap();
        assert_eq!(status["status"], "Success");
        assert!(get(&st, USERS, "u2").await.get("identities").is_none());
        assert_eq!(get(&st, IDENTITIES, "p:login").await["user"], json!({}));
        assert_eq!(delete_mapping(&st, "p:login", false).await.unwrap_err().status, StatusCode::NOT_FOUND);
        // PUT of a mapping that does not exist creates it (AllowCreateOnUpdate)…
        let (_, created) = create_or_update(&st, oc_mapping("p:login", "u1"), false, false).await.unwrap();
        assert!(created);
        // …but not when it names a version of one.
        delete_mapping(&st, "p:login", false).await.unwrap();
        let mut m = oc_mapping("p:login", "u1");
        m["metadata"]["resourceVersion"] = json!("7");
        assert_eq!(create_or_update(&st, m, false, false).await.unwrap_err().status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_half_link_is_no_mapping() {
        let st = storage();
        let u = put_user(&st, "u1").await;
        let mut i = put_identity(&st, "p", "login").await;
        // The Identity names the User, the User does not list it.
        i["user"] = json!({"name": "u1", "uid": u["metadata"]["uid"]});
        let rev = rev_of(&i);
        st.update(&ResourceStorage::cluster_key(IDENTITIES, "p:login"), i, rev).await.unwrap();
        assert_eq!(get_mapping(&st, "p:login").await.unwrap_err().status, StatusCode::NOT_FOUND);
        // Create completes it.
        create_or_update(&st, oc_mapping("p:login", "u1"), true, false).await.unwrap();
        assert_eq!(get_mapping(&st, "p:login").await.unwrap()["user"]["name"], "u1");
        // A User recreated under the same name (another uid) is not mapped.
        st.delete(&ResourceStorage::cluster_key(USERS, "u1"), None).await.unwrap();
        let mut u = json!({"metadata": {"name": "u1", "uid": "other"}, "identities": ["p:login"]});
        admit_user(&mut u).unwrap();
        st.create(&ResourceStorage::cluster_key(USERS, "u1"), u).await.unwrap();
        assert_eq!(get_mapping(&st, "p:login").await.unwrap_err().status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_dry_run_create_writes_nothing() {
        let st = storage();
        put_user(&st, "u1").await;
        put_identity(&st, "p", "login").await;
        let (m, created) = create_or_update(&st, oc_mapping("p:login", "u1"), true, true).await.unwrap();
        assert!(created);
        assert_eq!(m["user"]["name"], "u1");
        assert!(get(&st, USERS, "u1").await.get("identities").is_none());
        assert_eq!(get(&st, IDENTITIES, "p:login").await["user"], json!({}));
    }

    #[tokio::test]
    async fn tilde_is_the_requester() {
        let st = storage();
        let mut u = json!({"metadata": {"name": "alice", "uid": "a1"}, "fullName": "Alice", "identities": ["p:alice"]});
        admit_user(&mut u).unwrap();
        st.create(&ResourceStorage::cluster_key(USERS, "alice"), u).await.unwrap();
        let who = |name: &str, groups: &[&str]| UserInfo {
            username: name.into(),
            groups: groups.iter().map(|g| g.to_string()).collect(),
        };
        let me = current_user(&st, &who("alice", &["system:authenticated", "devs", "devs"])).await.unwrap();
        assert_eq!(me["fullName"], "Alice");
        assert_eq!(me["identities"], json!(["p:alice"]));
        assert_eq!(me["groups"], json!(["devs", "system:authenticated"]), "sorted, once each");
        // No User object: built from the request.
        let me = current_user(&st, &who("bob", &["system:authenticated"])).await.unwrap();
        assert_eq!(me["metadata"]["name"], "bob");
        assert_eq!(me["kind"], "User");
        // A name no User can have.
        let sa = "system:serviceaccount:storm-users:carol";
        let me = current_user(&st, &who(sa, &["system:serviceaccounts"])).await.unwrap();
        assert_eq!(me["metadata"]["name"], sa);
        assert_eq!(current_user(&st, &who("", &[])).await.unwrap_err().status, StatusCode::FORBIDDEN);
        assert!(check_user_get("alice").is_ok());
        assert!(check_user_get("a:b").unwrap_err().message.contains("must begin with \"b64:\""));
    }
}
