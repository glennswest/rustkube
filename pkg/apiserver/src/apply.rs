//! Server-side apply (KEP-555): `metadata.managedFields` field-ownership.
//!
//! Each apply records the set of fields its field-manager owns as a FieldsV1
//! tree. A later apply by the same manager updates that set (pruning fields it
//! dropped); an apply that changes a leaf owned by a *different* manager is a
//! conflict (rejected unless `force`). This is what lets operators declaratively
//! own an install and lets `kubectl apply --server-side` prune and detect
//! conflicts.

use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

/// A field-ownership conflict: another `manager` owns `field`.
#[derive(Debug)]
pub struct Conflict {
    pub field: String,
    pub manager: String,
}

/// FieldsV1 tree for an applied intent: objects recurse under `f:<key>`; scalars
/// and arrays are atomic leaves (`{}`). List merge-keys are treated atomically
/// for now (the common apply case is object trees + scalar leaves).
pub fn fields_v1(applied: &Value) -> Value {
    match applied {
        Value::Object(m) if !m.is_empty() => {
            let mut out = Map::new();
            for (k, v) in m {
                out.insert(format!("f:{k}"), fields_v1(v));
            }
            Value::Object(out)
        }
        _ => json!({}),
    }
}

/// The leaf JSON-pointer paths a FieldsV1 tree owns (e.g. `/metadata/labels/app`).
fn leaves(fv1: &Value, prefix: &str, out: &mut Vec<String>) {
    match fv1 {
        Value::Object(m) if !m.is_empty() => {
            for (k, v) in m {
                let key = k.strip_prefix("f:").unwrap_or(k);
                leaves(v, &format!("{prefix}/{key}"), out);
            }
        }
        _ => out.push(prefix.to_string()),
    }
}

/// Remove the value at a `/a/b/c` JSON pointer (pruning a dropped field).
fn remove_pointer(obj: &mut Value, pointer: &str) {
    let parts: Vec<&str> = pointer.trim_start_matches('/').split('/').collect();
    let Some((last, parents)) = parts.split_last() else {
        return;
    };
    let mut cur = obj;
    for p in parents {
        match cur.get_mut(p) {
            Some(v) => cur = v,
            None => return,
        }
    }
    if let Some(m) = cur.as_object_mut() {
        m.remove(*last);
    }
}

/// Apply `applied` for `manager` onto `existing`: reject (unless `force`) a leaf
/// owned by another manager that would change; prune leaves this manager dropped;
/// deep-merge the intent; and record this manager's managedFields entry.
pub fn server_side_apply(
    mut existing: Value,
    applied: &Value,
    manager: &str,
    now: &str,
    force: bool,
) -> Result<Value, Conflict> {
    let api_version = applied["apiVersion"].as_str().unwrap_or("v1").to_string();
    let new_fv1 = fields_v1(applied);
    let mut new_leaves = Vec::new();
    leaves(&new_fv1, "", &mut new_leaves);

    // Ownership from existing managedFields: leaf -> owning manager (other than us),
    // and the leaves we owned last time (to prune what we drop).
    let mut owners: BTreeMap<String, String> = BTreeMap::new();
    let mut prev_self: Vec<String> = Vec::new();
    if let Some(mf) = existing["metadata"]["managedFields"].as_array() {
        for entry in mf {
            let m = entry["manager"].as_str().unwrap_or("");
            let mut ls = Vec::new();
            leaves(&entry["fieldsV1"], "", &mut ls);
            if m == manager {
                prev_self = ls;
            } else {
                for l in ls {
                    owners.entry(l).or_insert_with(|| m.to_string());
                }
            }
        }
    }

    // Conflict: a leaf we set is owned by another manager and its value changes.
    if !force {
        for leaf in &new_leaves {
            if let Some(other) = owners.get(leaf) {
                if existing.pointer(leaf) != applied.pointer(leaf) {
                    return Err(Conflict {
                        field: leaf.trim_start_matches('/').replace('/', "."),
                        manager: other.clone(),
                    });
                }
            }
        }
    }

    // Prune leaves we owned before but no longer apply.
    for leaf in &prev_self {
        if !new_leaves.contains(leaf) {
            remove_pointer(&mut existing, leaf);
        }
    }

    // Merge the applied intent (apply wins on the fields it sets), then record our
    // managedFields entry (replacing any prior one for this manager).
    json_patch::merge(&mut existing, applied);
    let entry = json!({
        "manager": manager,
        "operation": "Apply",
        "apiVersion": api_version,
        "time": now,
        "fieldsType": "FieldsV1",
        "fieldsV1": new_fv1,
    });
    let mut mf: Vec<Value> = existing["metadata"]["managedFields"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|e| e["manager"].as_str() != Some(manager))
        .collect();
    mf.push(entry);
    if !existing["metadata"].is_object() {
        existing["metadata"] = json!({});
    }
    existing["metadata"]["managedFields"] = json!(mf);
    Ok(existing)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_managed_fields_and_prunes() {
        let applied = json!({
            "apiVersion": "v1", "kind": "ConfigMap",
            "metadata": {"name": "cm"}, "data": {"a": "1", "b": "2"}
        });
        let obj = server_side_apply(json!({}), &applied, "op", "T", true).unwrap();
        let mf = obj["metadata"]["managedFields"].as_array().unwrap();
        assert_eq!(mf.len(), 1);
        assert_eq!(mf[0]["manager"], "op");
        assert_eq!(mf[0]["operation"], "Apply");
        assert_eq!(mf[0]["fieldsV1"]["f:data"]["f:a"], json!({}));

        // Re-apply dropping data.b — it must be pruned (manager no longer owns it).
        let applied2 = json!({
            "apiVersion": "v1", "kind": "ConfigMap",
            "metadata": {"name": "cm"}, "data": {"a": "1"}
        });
        let obj2 = server_side_apply(obj, &applied2, "op", "T2", true).unwrap();
        assert_eq!(obj2["data"]["a"], "1");
        assert!(obj2["data"].get("b").is_none(), "dropped field pruned");
    }

    #[test]
    fn conflict_on_foreign_field_unless_forced() {
        let a = server_side_apply(
            json!({}),
            &json!({"apiVersion":"v1","metadata":{"name":"x"},"data":{"k":"1"}}),
            "mgr-a",
            "T",
            true,
        )
        .unwrap();
        // mgr-b changing data.k that mgr-a owns → conflict without force.
        let intent = json!({"apiVersion":"v1","metadata":{"name":"x"},"data":{"k":"2"}});
        assert!(server_side_apply(a.clone(), &intent, "mgr-b", "T", false).is_err());
        // With force it succeeds.
        assert!(server_side_apply(a, &intent, "mgr-b", "T", true).is_ok());
    }
}
