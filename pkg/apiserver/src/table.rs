//! `Accept: application/json;as=Table` — what `kubectl get` and `oc get` ask for.
//!
//! Without it a client gets a plain List and falls back to printing NAME and
//! AGE, which is why every `oc get` looked broken while the objects were fine
//! (#53). The server, not the client, decides the columns: that is the whole
//! point of the content negotiation, and it is why a resource the server does
//! not have an opinion about still prints something sensible.
//!
//! Columns here follow upstream's `printers/internalversion` for the resources
//! that have a conventional shape, because a person reading `oc get pvc` has
//! seen those columns before and will read them without thinking. Where a
//! resource has no upstream printer, the fallback is NAME + AGE, which is what
//! `kubectl` would have printed anyway.

use serde_json::{json, Value};

/// Does this request want a Table?
pub fn wants_table(headers: &axum::http::HeaderMap) -> bool {
    headers
        .get(axum::http::header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .map(|a| a.contains("as=Table"))
        .unwrap_or(false)
}

/// A column definition, in the shape `meta.k8s.io/v1` expects.
fn col(name: &str, kind: &str, priority: i64, desc: &str) -> Value {
    json!({
        "name": name,
        "type": kind,
        "format": "",
        "description": desc,
        "priority": priority,
    })
}

/// `metadata.creationTimestamp` rendered the way kubectl prints AGE.
///
/// Coarse on purpose, and matching upstream: minutes below an hour, hours
/// below a day, then days. A column nobody can read at a glance is a column
/// that costs width for nothing.
fn age(obj: &Value) -> String {
    let ts = obj["metadata"]["creationTimestamp"].as_str().unwrap_or("");
    let Ok(then) = chrono::DateTime::parse_from_rfc3339(ts) else {
        return "<unknown>".to_string();
    };
    let secs = (chrono::Utc::now() - then.with_timezone(&chrono::Utc)).num_seconds().max(0);
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86400)
    }
}

fn name_of(obj: &Value) -> String {
    obj["metadata"]["name"].as_str().unwrap_or("").to_string()
}

/// The columns and the row-builder for a resource.
///
/// Returns `None` for anything without a conventional printer, and the caller
/// falls back to NAME + AGE.
type RowFn = fn(&Value) -> Vec<Value>;

fn printer(resource: &str) -> Option<(Vec<Value>, RowFn)> {
    match resource {
        "persistentvolumeclaims" => Some((
            vec![
                col("Name", "string", 0, "Name of the claim"),
                col("Status", "string", 0, "Bound, Pending or Lost"),
                col("Volume", "string", 0, "The PersistentVolume it is bound to"),
                col("Capacity", "string", 0, "Capacity of the bound volume"),
                col("Access Modes", "string", 0, "How the volume may be mounted"),
                col("Storageclass", "string", 0, "The StorageClass that provisioned it"),
                col("Age", "string", 0, "Time since creation"),
            ],
            (|o: &Value| {
                vec![
                    json!(name_of(o)),
                    // A claim with no status is Pending, which is true and is
                    // what upstream prints — not an empty cell.
                    json!(o["status"]["phase"].as_str().unwrap_or("Pending")),
                    json!(o["spec"]["volumeName"].as_str().unwrap_or("")),
                    json!(o["status"]["capacity"]["storage"].as_str().unwrap_or("")),
                    json!(modes(&o["spec"]["accessModes"])),
                    json!(o["spec"]["storageClassName"].as_str().unwrap_or("")),
                    json!(age(o)),
                ]
            }) as RowFn,
        )),
        "persistentvolumes" => Some((
            vec![
                col("Name", "string", 0, "Name of the volume"),
                col("Capacity", "string", 0, "Its capacity"),
                col("Access Modes", "string", 0, "How it may be mounted"),
                col("Reclaim Policy", "string", 0, "What happens when its claim goes"),
                col("Status", "string", 0, "Available, Bound, Released or Failed"),
                col("Claim", "string", 0, "The claim it is bound to"),
                col("Storageclass", "string", 0, "The StorageClass it came from"),
                col("Age", "string", 0, "Time since creation"),
            ],
            (|o: &Value| {
                let claim = match (
                    o["spec"]["claimRef"]["namespace"].as_str(),
                    o["spec"]["claimRef"]["name"].as_str(),
                ) {
                    (Some(ns), Some(n)) => format!("{ns}/{n}"),
                    _ => String::new(),
                };
                vec![
                    json!(name_of(o)),
                    json!(o["spec"]["capacity"]["storage"].as_str().unwrap_or("")),
                    json!(modes(&o["spec"]["accessModes"])),
                    json!(o["spec"]["persistentVolumeReclaimPolicy"].as_str().unwrap_or("Retain")),
                    json!(o["status"]["phase"].as_str().unwrap_or("Available")),
                    json!(claim),
                    json!(o["spec"]["storageClassName"].as_str().unwrap_or("")),
                    json!(age(o)),
                ]
            }) as RowFn,
        )),
        "pods" => Some((
            vec![
                col("Name", "string", 0, "Name of the pod"),
                col("Ready", "string", 0, "Containers ready over total"),
                col("Status", "string", 0, "Pod phase"),
                col("Restarts", "string", 0, "Total container restarts"),
                col("Age", "string", 0, "Time since creation"),
                col("IP", "string", 1, "Pod IP"),
                col("Node", "string", 1, "Node it is on"),
            ],
            (|o: &Value| {
                let cs = o["status"]["containerStatuses"].as_array().cloned().unwrap_or_default();
                let total = o["spec"]["containers"].as_array().map(|c| c.len()).unwrap_or(cs.len());
                let ready = cs.iter().filter(|c| c["ready"].as_bool().unwrap_or(false)).count();
                let restarts: i64 = cs.iter().map(|c| c["restartCount"].as_i64().unwrap_or(0)).sum();
                vec![
                    json!(name_of(o)),
                    json!(format!("{ready}/{total}")),
                    json!(o["status"]["phase"].as_str().unwrap_or("")),
                    json!(restarts.to_string()),
                    json!(age(o)),
                    json!(o["status"]["podIP"].as_str().unwrap_or("<none>")),
                    json!(o["spec"]["nodeName"].as_str().unwrap_or("<none>")),
                ]
            }) as RowFn,
        )),
        "nodes" => Some((
            vec![
                col("Name", "string", 0, "Name of the node"),
                col("Status", "string", 0, "Ready, NotReady or unknown"),
                col("Roles", "string", 0, "Roles from node-role labels"),
                col("Age", "string", 0, "Time since creation"),
                col("Version", "string", 0, "Kubelet version"),
            ],
            (|o: &Value| {
                let status = o["status"]["conditions"]
                    .as_array()
                    .and_then(|cs| cs.iter().find(|c| c["type"] == "Ready"))
                    .map(|c| if c["status"] == "True" { "Ready" } else { "NotReady" })
                    .unwrap_or("Unknown");
                let roles: Vec<String> = o["metadata"]["labels"]
                    .as_object()
                    .map(|m| {
                        m.keys()
                            .filter_map(|k| k.strip_prefix("node-role.kubernetes.io/"))
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
                vec![
                    json!(name_of(o)),
                    json!(status),
                    json!(if roles.is_empty() { "<none>".to_string() } else { roles.join(",") }),
                    json!(age(o)),
                    json!(o["status"]["nodeInfo"]["kubeletVersion"].as_str().unwrap_or("")),
                ]
            }) as RowFn,
        )),
        "namespaces" => Some((
            vec![
                col("Name", "string", 0, "Name of the namespace"),
                col("Status", "string", 0, "Active or Terminating"),
                col("Age", "string", 0, "Time since creation"),
            ],
            (|o: &Value| {
                vec![
                    json!(name_of(o)),
                    json!(o["status"]["phase"].as_str().unwrap_or("Active")),
                    json!(age(o)),
                ]
            }) as RowFn,
        )),
        _ => None,
    }
}

/// Access modes as kubectl abbreviates them: RWO, ROX, RWX, RWOP.
fn modes(v: &Value) -> String {
    let Some(arr) = v.as_array() else { return String::new() };
    let mut out: Vec<&str> = Vec::new();
    for m in arr {
        out.push(match m.as_str().unwrap_or("") {
            "ReadWriteOnce" => "RWO",
            "ReadOnlyMany" => "ROX",
            "ReadWriteMany" => "RWX",
            "ReadWriteOncePod" => "RWOP",
            other => other,
        });
    }
    out.join(",")
}

/// Convert a List (or a single object) into a `meta.k8s.io/v1` Table.
///
/// The whole object rides in `row.object`, because `kubectl -o wide` and
/// `kubectl get -o yaml` on a Table response both expect to find it there —
/// dropping it makes the response smaller and several client behaviours stop
/// working.
pub fn to_table(resource: &str, body: Value) -> Value {
    let (columns, row_of) = printer(resource).unwrap_or_else(|| {
        (
            vec![
                col("Name", "string", 0, "Name of the object"),
                col("Age", "string", 0, "Time since creation"),
            ],
            (|o: &Value| vec![json!(name_of(o)), json!(age(o))]) as RowFn,
        )
    });

    let items: Vec<Value> = match body["items"].as_array() {
        Some(a) => a.clone(),
        // A single object is a Table of one row, which is what `kubectl get
        // pod x` asks for.
        None => vec![body.clone()],
    };

    let rows: Vec<Value> = items
        .iter()
        .map(|o| json!({ "cells": row_of(o), "object": o }))
        .collect();

    json!({
        "kind": "Table",
        "apiVersion": "meta.k8s.io/v1",
        "metadata": {
            "resourceVersion": body["metadata"]["resourceVersion"].as_str().unwrap_or(""),
        },
        "columnDefinitions": columns,
        "rows": rows,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_claim_with_no_status_prints_pending_not_a_blank() {
        // What the node actually produced before the binder existed: a claim
        // with no status at all. Upstream prints Pending, and a blank cell
        // would read as "something went wrong here" rather than "not bound
        // yet".
        let list = json!({"items":[{"metadata":{"name":"data","creationTimestamp":"2026-08-28T17:15:20Z"},
                                    "spec":{"accessModes":["ReadWriteOnce"]}}]});
        let t = to_table("persistentvolumeclaims", list);
        let cells = &t["rows"][0]["cells"];
        assert_eq!(cells[0], "data");
        assert_eq!(cells[1], "Pending");
        assert_eq!(cells[4], "RWO", "access modes are abbreviated as kubectl does");
    }

    #[test]
    fn a_bound_claim_shows_its_volume_and_capacity() {
        let list = json!({"items":[{
            "metadata":{"name":"data","creationTimestamp":"2026-08-28T17:15:20Z"},
            "spec":{"accessModes":["ReadWriteOnce"],"volumeName":"pv-1","storageClassName":"storm"},
            "status":{"phase":"Bound","capacity":{"storage":"16Mi"}}}]});
        let c = &to_table("persistentvolumeclaims", list)["rows"][0]["cells"];
        assert_eq!(c[1], "Bound");
        assert_eq!(c[2], "pv-1");
        assert_eq!(c[3], "16Mi");
        assert_eq!(c[5], "storm");
    }

    #[test]
    fn the_whole_object_rides_along() {
        // kubectl -o wide and -o yaml against a Table both read row.object;
        // dropping it makes the response smaller and breaks them.
        let list = json!({"items":[{"metadata":{"name":"x"},"spec":{"nodeName":"n1"}}]});
        let t = to_table("pods", list);
        assert_eq!(t["rows"][0]["object"]["spec"]["nodeName"], "n1");
    }

    #[test]
    fn a_resource_with_no_printer_still_prints_name_and_age() {
        let list = json!({"items":[{"metadata":{"name":"thing"}}]});
        let t = to_table("widgets", list);
        assert_eq!(t["columnDefinitions"].as_array().unwrap().len(), 2);
        assert_eq!(t["rows"][0]["cells"][0], "thing");
    }

    #[test]
    fn a_single_object_is_a_table_of_one_row() {
        let one = json!({"metadata":{"name":"solo"}});
        let t = to_table("namespaces", one);
        assert_eq!(t["rows"].as_array().unwrap().len(), 1);
        assert_eq!(t["rows"][0]["cells"][0], "solo");
    }
}
