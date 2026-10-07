//! Who created a `storage.storm.io` object, stamped by the apiserver (#210).
//!
//! stormdrive's controller re-checks the creator of a `DriveOperation` with a
//! SubjectAccessReview before it formats, sanitizes or partitions a drive
//! (stormdrive#45): RBAC decided who may create one, and the controller must
//! not act on an object alone. For that the object has to say who created it
//! in a way no client can forge — `managedFields` managers are the client's
//! choice, and so is anything else in the body.
//!
//! So, as `openshift.io/requester` on a project's namespace
//! (`handlers/project.rs`), on **create** of any object in the
//! `storage.storm.io` group the apiserver sets:
//!
//! - `storage.storm.io/requester` — the authenticated username
//! - `storage.storm.io/requester-groups` — its groups, joined by `,`
//!
//! replacing whatever the client sent; and on **every update** (PUT, PATCH,
//! server-side apply, `/status`) the stored values are carried over, so the
//! stamp keeps meaning "who created it". An object stored before this, with
//! no stamp, stays without one: an update cannot add it either.
//!
//! It runs in [`crate::admission::admit`], after the mutating webhooks (so no
//! webhook can change it), for every write a request makes; writes the
//! apiserver makes for itself carry no request and are not stamped.

use crate::auth::UserInfo;
use serde_json::{json, Value};

/// The group whose objects are stamped.
pub const GROUP: &str = "storage.storm.io";
pub const REQUESTER: &str = "storage.storm.io/requester";
pub const REQUESTER_GROUPS: &str = "storage.storm.io/requester-groups";

/// Stamp a new object with `user`.
pub fn on_create(obj: &mut Value, user: &UserInfo) {
    let Some(annotations) = annotations(obj) else { return };
    annotations.insert(REQUESTER.into(), json!(user.username));
    annotations.insert(REQUESTER_GROUPS.into(), json!(user.groups.join(",")));
}

/// Carry the stamp of `stored` over to its replacement, or remove one the
/// client sent when `stored` has none.
pub fn on_update(obj: &mut Value, stored: &Value) {
    let kept: Vec<(&str, Option<Value>)> = [REQUESTER, REQUESTER_GROUPS]
        .into_iter()
        .map(|k| (k, stored["metadata"]["annotations"].get(k).cloned()))
        .collect();
    let Some(annotations) = annotations(obj) else { return };
    for (k, v) in kept {
        match v {
            Some(v) => {
                annotations.insert(k.into(), v);
            }
            None => {
                annotations.remove(k);
            }
        }
    }
    if annotations.is_empty() {
        if let Some(m) = obj["metadata"].as_object_mut() {
            m.remove("annotations");
        }
    }
}

/// `metadata.annotations` as a map, made one if it is absent or not a map.
/// `None` only for a body that is not an object at all.
fn annotations(obj: &mut Value) -> Option<&mut serde_json::Map<String, Value>> {
    let o = obj.as_object_mut()?;
    let meta = o.entry("metadata").or_insert_with(|| json!({}));
    if !meta.is_object() {
        *meta = json!({});
    }
    let a = meta.as_object_mut()?.entry("annotations").or_insert_with(|| json!({}));
    if !a.is_object() {
        *a = json!({});
    }
    a.as_object_mut()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alice() -> UserInfo {
        UserInfo { username: "alice".into(), groups: vec!["storage-admins".into(), "system:authenticated".into()] }
    }

    #[test]
    fn create_stamps_the_caller_over_what_the_body_says() {
        let mut o = json!({"metadata": {"name": "op", "annotations": {
            REQUESTER: "system:admin", REQUESTER_GROUPS: "system:masters", "other": "kept"}}});
        on_create(&mut o, &alice());
        let a = &o["metadata"]["annotations"];
        assert_eq!(a[REQUESTER], "alice");
        assert_eq!(a[REQUESTER_GROUPS], "storage-admins,system:authenticated");
        assert_eq!(a["other"], "kept");
        // No metadata at all, and annotations that are not a map: stamped.
        let mut bare = json!({"spec": {}});
        on_create(&mut bare, &alice());
        assert_eq!(bare["metadata"]["annotations"][REQUESTER], "alice");
        let mut odd = json!({"metadata": {"annotations": null}});
        on_create(&mut odd, &alice());
        assert_eq!(odd["metadata"]["annotations"][REQUESTER], "alice");
        // No groups: an empty string, not a missing key.
        let mut o = json!({});
        on_create(&mut o, &UserInfo { username: "bob".into(), groups: vec![] });
        assert_eq!(o["metadata"]["annotations"][REQUESTER_GROUPS], "");
    }

    #[test]
    fn update_keeps_the_stored_stamp() {
        let stored = json!({"metadata": {"annotations": {REQUESTER: "alice", REQUESTER_GROUPS: "g1"}}});
        // Forged, dropped, or edited: the stored stamp wins.
        let mut forged = json!({"metadata": {"annotations": {REQUESTER: "mallory", REQUESTER_GROUPS: "system:masters", "x": "1"}}});
        on_update(&mut forged, &stored);
        assert_eq!(forged["metadata"]["annotations"], json!({REQUESTER: "alice", REQUESTER_GROUPS: "g1", "x": "1"}));
        let mut dropped = json!({"metadata": {}});
        on_update(&mut dropped, &stored);
        assert_eq!(dropped["metadata"]["annotations"][REQUESTER], "alice");
        // Stored without a stamp (older than #210): an update cannot add one.
        let mut sneaky = json!({"metadata": {"annotations": {REQUESTER: "mallory"}}});
        on_update(&mut sneaky, &json!({"metadata": {}}));
        assert!(sneaky["metadata"].get("annotations").is_none(), "{sneaky}");
        let mut keeps_others = json!({"metadata": {"annotations": {REQUESTER: "mallory", "x": "1"}}});
        on_update(&mut keeps_others, &json!({"metadata": {}}));
        assert_eq!(keeps_others["metadata"]["annotations"], json!({"x": "1"}));
    }
}
