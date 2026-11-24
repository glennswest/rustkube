//! `user.openshift.io/v1` Groups (#372, sc#151).
//!
//! A Group is a cluster-scoped list of user names, the object `oc adm groups
//! new|add-users|remove-users|sync|prune` and `oc adm prune groups` act on.
//! It is stored and served by the generic cluster handlers, like a
//! PriorityClass; what is particular to it lives here:
//!
//! - **Validation**, as openshift/apiserver's `ValidateGroup`: the name is a
//!   path segment (not `.` or `..`, no `/` or `%`), has no `:` and is not
//!   `~`; every user is non-empty and the same shape. The one difference: a
//!   ServiceAccount's user name, `system:serviceaccount:<namespace>:<name>`,
//!   may be listed. OpenShift refuses it (users there are `User` objects),
//!   but the users of StormCOS are ServiceAccounts in `storm-users` (sc#27),
//!   so without it no Group could hold one of them.
//! - **`users: null`** when a Group is written without users, as OpenShift's
//!   serializer writes an empty `OptionalNames` (the field has no
//!   `omitempty`).
//! - **Membership at authentication** ([`GroupIndex`]): a request from a
//!   user a Group lists carries that Group's name among its groups, as
//!   OpenShift's group cache adds it, so a RoleBinding to the Group grants
//!   its members. The index is rebuilt from the watch cache's view of
//!   `/registry/groups/` only when that view has moved on (the datastore is
//!   read when there is no cache).
//!
//! A Group name cannot contain `:`, so no Group can put a user into a
//! `system:` group (`system:masters`, `system:nodes`, …).

use crate::error::ApiError;
use crate::storage::ResourceStorage;
use crate::watch_cache::SnapshotVersion;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// The stored plural.
pub const RESOURCE: &str = "groups";

/// Why `name` is not a path segment, as apimachinery's
/// `ValidatePathSegmentName`.
pub(crate) fn path_segment_reasons(name: &str) -> Vec<&'static str> {
    let mut reasons = Vec::new();
    match name {
        "." => reasons.push("may not be '.'"),
        ".." => reasons.push("may not be '..'"),
        _ => {}
    }
    if name.contains('/') {
        reasons.push("may not contain '/'");
    }
    if name.contains('%') {
        reasons.push("may not contain '%'");
    }
    reasons
}

/// Why `name` is not a valid Group or user name, as OpenShift's
/// `ValidateGroupName` / `ValidateUserName` (path segment, then `:` and `~`).
fn name_reasons(name: &str) -> Vec<&'static str> {
    let mut reasons = path_segment_reasons(name);
    if !reasons.is_empty() {
        return reasons;
    }
    if name.contains(':') {
        reasons.push("may not contain \":\"");
    }
    if name == "~" {
        reasons.push("may not equal \"~\"");
    }
    reasons
}

/// A DNS-1123 label (a Namespace's name).
pub(crate) fn is_dns_label(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 63
        && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !s.starts_with('-')
        && !s.ends_with('-')
}

/// A DNS-1123 subdomain (a ServiceAccount's name).
pub(crate) fn is_dns_subdomain(s: &str) -> bool {
    !s.is_empty() && s.len() <= 253 && s.split('.').all(|l| {
        !l.is_empty()
            && l.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            && !l.starts_with('-')
            && !l.ends_with('-')
    })
}

/// `system:serviceaccount:<namespace>:<name>`, both parts valid.
fn is_serviceaccount_user(user: &str) -> bool {
    match user.strip_prefix("system:serviceaccount:").map(|r| r.split(':').collect::<Vec<_>>()) {
        Some(parts) => parts.len() == 2 && is_dns_label(parts[0]) && is_dns_subdomain(parts[1]),
        None => false,
    }
}

/// The 422 for a Group that OpenShift's validation would refuse; also
/// writes `users: null` when the Group has none.
pub fn admit(obj: &mut Value) -> Result<(), ApiError> {
    if let Some(map) = obj.as_object_mut() {
        map.entry("users").or_insert(Value::Null);
    }
    let name = obj["metadata"]["name"].as_str().unwrap_or("").to_string();
    let mut errs: Vec<String> = Vec::new();
    if name.is_empty() {
        errs.push("metadata.name: Required value: name or generateName is required".into());
    } else {
        let reasons = name_reasons(&name);
        if !reasons.is_empty() {
            errs.push(format!("metadata.name: Invalid value: {name:?}: {}", reasons.join(", ")));
        }
    }
    if obj["metadata"]["namespace"].as_str().is_some_and(|n| !n.is_empty()) {
        errs.push("metadata.namespace: Forbidden: not allowed on this type".into());
    }
    match &obj["users"] {
        Value::Null => {}
        Value::Array(users) => {
            for (i, user) in users.iter().enumerate() {
                let Some(user) = user.as_str() else {
                    errs.push(format!("users[{i}]: Invalid value: {user}: must be a string"));
                    continue;
                };
                // `user[i]`, not `users[i]`: OpenShift's field path.
                if user.is_empty() {
                    errs.push(format!("user[{i}]: Invalid value: \"\": may not be empty"));
                    continue;
                }
                if is_serviceaccount_user(user) {
                    continue;
                }
                let reasons = name_reasons(user);
                if !reasons.is_empty() {
                    errs.push(format!("user[{i}]: Invalid value: {user:?}: {}", reasons.join(", ")));
                }
            }
        }
        other => errs.push(format!("users: Invalid value: {other}: must be a list of user names")),
    }
    if errs.is_empty() {
        return Ok(());
    }
    let what = if errs.len() == 1 { errs.remove(0) } else { format!("[{}]", errs.join(", ")) };
    Err(ApiError::invalid(&format!("Group.user.openshift.io {name:?} is invalid: {what}")))
}

/// User name → the Groups listing it.
type Members = HashMap<String, Vec<String>>;

fn members_of<'a>(groups: impl Iterator<Item = &'a Value>) -> Members {
    let mut m: Members = HashMap::new();
    for g in groups {
        let Some(name) = g["metadata"]["name"].as_str() else { continue };
        for user in g["users"].as_array().into_iter().flatten().filter_map(Value::as_str) {
            let list = m.entry(user.to_string()).or_default();
            if !list.iter().any(|n| n == name) {
                list.push(name.to_string());
            }
        }
    }
    for list in m.values_mut() {
        list.sort();
    }
    m
}

/// The Groups each user is in, from the watch cache (OpenShift's group
/// cache). Shared by every request; rebuilt when a Group changes.
pub struct GroupIndex {
    storage: Arc<ResourceStorage>,
    view: Mutex<Option<(SnapshotVersion, Arc<Members>)>>,
}

impl GroupIndex {
    pub fn new(storage: Arc<ResourceStorage>) -> Self {
        Self { storage, view: Mutex::new(None) }
    }

    /// From the watch cache, rebuilt only when it has moved on; read from
    /// the datastore when the cache cannot be had, as the RBAC authorizer
    /// does. `None` only when neither answers.
    async fn members(&self) -> Option<Arc<Members>> {
        let prefix = ResourceStorage::cluster_prefix(RESOURCE);
        let cache = self.storage.watch_cache();
        if let Ok(version) = cache.version(&prefix).await {
            if let Some((v, m)) = self.view.lock().unwrap().as_ref() {
                if *v == version {
                    return Some(m.clone());
                }
            }
            if let Ok((version, items)) = cache.snapshot(&prefix).await {
                let objects: Vec<Value> = items.iter().filter_map(|(_, b)| serde_json::from_slice(b).ok()).collect();
                let members = Arc::new(members_of(objects.iter()));
                *self.view.lock().unwrap() = Some((version, members.clone()));
                return Some(members);
            }
        }
        let mut objects = Vec::new();
        let mut token: Option<String> = None;
        loop {
            match self.storage.list(&prefix, 1000, token.as_deref()).await {
                Ok((items, next, _)) => {
                    objects.extend(items);
                    match next {
                        Some(t) => token = Some(t),
                        None => break,
                    }
                }
                Err(e) => {
                    tracing::warn!("groups: cannot read Groups, no Group memberships added: {}", e.message);
                    return None;
                }
            }
        }
        Some(Arc::new(members_of(objects.iter())))
    }

    /// The Groups that list `username`, sorted by name.
    pub async fn groups_of(&self, username: &str) -> Vec<String> {
        match self.members().await {
            Some(m) => m.get(username).cloned().unwrap_or_default(),
            None => Vec::new(),
        }
    }

    /// `groups` with every Group listing `username` added once.
    pub async fn add_to(&self, username: &str, groups: &mut Vec<String>) {
        for g in self.groups_of(username).await {
            if !groups.contains(&g) {
                groups.push(g);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn group(name: &str, users: Value) -> Value {
        json!({"apiVersion": "user.openshift.io/v1", "kind": "Group", "metadata": {"name": name}, "users": users})
    }

    #[test]
    fn a_group_of_users_and_serviceaccounts_is_valid() {
        let mut g = group("devs", json!(["alice", "bob@example.com", "system:serviceaccount:storm-users:carol"]));
        admit(&mut g).unwrap();
    }

    #[test]
    fn a_group_without_users_has_users_null() {
        let mut g = json!({"metadata": {"name": "empty"}});
        admit(&mut g).unwrap();
        assert!(g.as_object().unwrap().contains_key("users"));
        assert_eq!(g["users"], Value::Null);
    }

    #[test]
    fn names_are_refused_as_openshift_refuses_them() {
        let msg = |name: &str| admit(&mut group(name, Value::Null)).unwrap_err().message;
        assert_eq!(
            msg("a:b"),
            "Group.user.openshift.io \"a:b\" is invalid: metadata.name: Invalid value: \"a:b\": may not contain \":\""
        );
        assert!(msg("system:masters").contains("may not contain \":\""), "no Group grants a system: group");
        assert!(msg("~").contains("may not equal \"~\""));
        assert!(msg("a/b").contains("may not contain '/'"));
        assert!(msg("a%b").contains("may not contain '%'"));
        assert!(msg("..").contains("may not be '..'"));
        assert_eq!(admit(&mut group("", Value::Null)).unwrap_err().status, axum::http::StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[test]
    fn users_are_refused_as_openshift_refuses_them() {
        let msg = |users: Value| admit(&mut group("g", users)).unwrap_err().message;
        assert_eq!(
            msg(json!(["ok", ""])),
            "Group.user.openshift.io \"g\" is invalid: user[1]: Invalid value: \"\": may not be empty"
        );
        assert!(msg(json!(["a:b"])).contains("user[0]: Invalid value: \"a:b\": may not contain \":\""));
        assert!(msg(json!(["~"])).contains("may not equal \"~\""));
        // Only a well-formed ServiceAccount name gets past the `:` rule.
        assert!(msg(json!(["system:serviceaccount:storm-users"])).contains("may not contain \":\""));
        assert!(msg(json!(["system:serviceaccount:Bad_NS:x"])).contains("may not contain \":\""));
        assert!(msg(json!(["system:serviceaccount:ns:a:b"])).contains("may not contain \":\""));
        assert!(msg(json!(["system:admin"])).contains("may not contain \":\""));
        assert!(msg(json!("alice")).contains("users: Invalid value"));
    }

    #[test]
    fn membership_is_indexed_by_user() {
        let gs = [
            group("b", json!(["alice", "bob"])),
            group("a", json!(["alice", "alice"])),
            group("c", Value::Null),
        ];
        let m = members_of(gs.iter());
        assert_eq!(m["alice"], ["a", "b"]);
        assert_eq!(m["bob"], ["b"]);
        assert!(!m.contains_key("carol"));
    }

    #[tokio::test]
    async fn the_index_follows_the_stored_groups() {
        let storage = Arc::new(ResourceStorage::new(Arc::new(crate::test_store::MemStore::default())));
        let key = ResourceStorage::cluster_key(RESOURCE, "devs");
        storage.create(&key, group("devs", json!(["alice"]))).await.unwrap();
        let index = GroupIndex::new(storage.clone());
        // MemStore has no watch, so this is the datastore path.
        assert_eq!(index.groups_of("alice").await, ["devs"]);
        let mut stored = storage.get(&key).await.unwrap();
        stored["users"] = json!(["bob"]);
        storage.update(&key, stored, None).await.unwrap();
        assert!(index.groups_of("alice").await.is_empty());
        assert_eq!(index.groups_of("bob").await, ["devs"]);
        let mut groups = vec!["system:authenticated".to_string()];
        index.add_to("bob", &mut groups).await;
        index.add_to("bob", &mut groups).await;
        assert_eq!(groups, ["system:authenticated", "devs"]);
    }
}
