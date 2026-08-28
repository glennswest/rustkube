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
        "routes" => Some((
            vec![
                col("Name", "string", 0, "Name of the route"),
                col("Host/Port", "string", 0, "The hostname it answers on"),
                col("Path", "string", 0, "Path prefix, if the route is path-scoped"),
                col("Services", "string", 0, "The service it reaches"),
                col("Port", "string", 0, "Target port on that service"),
                col("Termination", "string", 0, "TLS termination, if any"),
                col("Wildcard", "string", 0, "Wildcard policy"),
            ],
            (|o: &Value| {
                // `host` is what the router admitted; `spec.host` is what was
                // asked for. Neither is guaranteed, and an unadmitted route
                // showing a blank host is the signal that it was not admitted.
                let host = o["spec"]["host"].as_str().unwrap_or("");
                let port = o["spec"]["port"]["targetPort"].as_str().map(str::to_string).or_else(
                    || o["spec"]["port"]["targetPort"].as_i64().map(|n| n.to_string()),
                );
                vec![
                    json!(name_of(o)),
                    json!(host),
                    json!(o["spec"]["path"].as_str().unwrap_or("")),
                    json!(o["spec"]["to"]["name"].as_str().unwrap_or("")),
                    json!(port.unwrap_or_default()),
                    json!(o["spec"]["tls"]["termination"].as_str().unwrap_or("")),
                    json!(o["spec"]["wildcardPolicy"].as_str().unwrap_or("None")),
                ]
            }) as RowFn,
        )),
        "services" => Some((
            vec![
                col("Name", "string", 0, "Name of the service"),
                col("Type", "string", 0, "ClusterIP, NodePort, LoadBalancer or ExternalName"),
                col("Cluster-IP", "string", 0, "The service's cluster IP"),
                col("External-IP", "string", 0, "External address, if any"),
                col("Port(s)", "string", 0, "Exposed ports"),
                col("Age", "string", 0, "Time since creation"),
            ],
            (|o: &Value| {
                let ports = o["spec"]["ports"]
                    .as_array()
                    .map(|ps| {
                        ps.iter()
                            .map(|p| {
                                let port = p["port"].as_i64().unwrap_or(0);
                                let proto = p["protocol"].as_str().unwrap_or("TCP");
                                match p["nodePort"].as_i64() {
                                    Some(np) => format!("{port}:{np}/{proto}"),
                                    None => format!("{port}/{proto}"),
                                }
                            })
                            .collect::<Vec<_>>()
                            .join(",")
                    })
                    .unwrap_or_default();
                vec![
                    json!(name_of(o)),
                    json!(o["spec"]["type"].as_str().unwrap_or("ClusterIP")),
                    // "None" for a headless service is what upstream prints,
                    // and it is meaningful — not a missing value.
                    json!(o["spec"]["clusterIP"].as_str().unwrap_or("<none>")),
                    json!(o["spec"]["externalIPs"][0].as_str().unwrap_or("<none>")),
                    json!(ports),
                    json!(age(o)),
                ]
            }) as RowFn,
        )),
        "ingresses" => Some((
            vec![
                col("Name", "string", 0, "Name of the ingress"),
                col("Class", "string", 0, "IngressClass that serves it"),
                col("Hosts", "string", 0, "Hostnames it answers on"),
                col("Address", "string", 0, "Address the controller assigned"),
                col("Ports", "string", 0, "80, and 443 when TLS is configured"),
                col("Age", "string", 0, "Time since creation"),
            ],
            (|o: &Value| {
                let hosts = o["spec"]["rules"]
                    .as_array()
                    .map(|rs| {
                        rs.iter()
                            .filter_map(|r| r["host"].as_str())
                            .collect::<Vec<_>>()
                            .join(",")
                    })
                    .filter(|h: &String| !h.is_empty())
                    .unwrap_or_else(|| "*".to_string());
                let addr = o["status"]["loadBalancer"]["ingress"][0]["ip"]
                    .as_str()
                    .or_else(|| o["status"]["loadBalancer"]["ingress"][0]["hostname"].as_str())
                    .unwrap_or("");
                let ports =
                    if o["spec"]["tls"].as_array().is_some_and(|t| !t.is_empty()) {
                        "80, 443"
                    } else {
                        "80"
                    };
                vec![
                    json!(name_of(o)),
                    json!(o["spec"]["ingressClassName"].as_str().unwrap_or("<none>")),
                    json!(hosts),
                    json!(addr),
                    json!(ports),
                    json!(age(o)),
                ]
            }) as RowFn,
        )),
        "deployments" => Some((
            vec![
                col("Name", "string", 0, "Name of the deployment"),
                col("Ready", "string", 0, "Ready replicas over desired"),
                col("Up-to-date", "integer", 0, "Replicas at the current spec"),
                col("Available", "integer", 0, "Replicas available to serve"),
                col("Age", "string", 0, "Time since creation"),
            ],
            (|o: &Value| {
                let want = o["spec"]["replicas"].as_i64().unwrap_or(1);
                let ready = o["status"]["readyReplicas"].as_i64().unwrap_or(0);
                vec![
                    json!(name_of(o)),
                    json!(format!("{ready}/{want}")),
                    json!(o["status"]["updatedReplicas"].as_i64().unwrap_or(0)),
                    json!(o["status"]["availableReplicas"].as_i64().unwrap_or(0)),
                    json!(age(o)),
                ]
            }) as RowFn,
        )),
        "daemonsets" => Some((
            vec![
                col("Name", "string", 0, "Name of the daemonset"),
                col("Desired", "integer", 0, "Nodes that should run it"),
                col("Current", "integer", 0, "Nodes running it"),
                col("Ready", "integer", 0, "Pods ready"),
                col("Up-to-date", "integer", 0, "Pods at the current spec"),
                col("Available", "integer", 0, "Pods available"),
                col("Age", "string", 0, "Time since creation"),
            ],
            (|o: &Value| {
                let st = &o["status"];
                vec![
                    json!(name_of(o)),
                    json!(st["desiredNumberScheduled"].as_i64().unwrap_or(0)),
                    json!(st["currentNumberScheduled"].as_i64().unwrap_or(0)),
                    json!(st["numberReady"].as_i64().unwrap_or(0)),
                    json!(st["updatedNumberScheduled"].as_i64().unwrap_or(0)),
                    json!(st["numberAvailable"].as_i64().unwrap_or(0)),
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

/// Resolve one `additionalPrinterColumns` JSONPath against an object.
///
/// The subset upstream actually allows in a printer column: a leading `.`,
/// dotted field names, and numeric indices — `.spec.replicas`,
/// `.status.conditions[0].type`. Filters and wildcards are not permitted
/// there, so they are not implemented here; an unresolvable path yields an
/// empty cell, which is what upstream prints.
fn json_path<'a>(obj: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = obj;
    for seg in path.trim_start_matches('.').split('.') {
        if seg.is_empty() {
            continue;
        }
        // `conditions[0]` — a field, then one or more indices.
        let (field, rest) = match seg.find('[') {
            Some(i) => (&seg[..i], &seg[i..]),
            None => (seg, ""),
        };
        if !field.is_empty() {
            cur = cur.get(field)?;
        }
        for idx in rest.split('[').filter(|p| !p.is_empty()) {
            let n: usize = idx.trim_end_matches(']').parse().ok()?;
            cur = cur.get(n)?;
        }
    }
    Some(cur)
}

/// A cell's printed form. Strings print bare; everything else prints as JSON,
/// so a number is `3` rather than `"3"` and a client formatting by `type`
/// gets what it expects.
fn cell(v: &Value) -> Value {
    match v {
        Value::String(s) => json!(s),
        other => other.clone(),
    }
}

/// Columns and rows from a CRD's own `additionalPrinterColumns`.
///
/// NAME first always. **AGE only when the CRD declares no columns of its
/// own** — that is what upstream's apiextensions table converter does, and it
/// matters because most real CRDs declare an Age column themselves. Appending
/// one unconditionally prints Age twice, which is exactly what Cilium's
/// CiliumNetworkPolicy did before this was fixed.
///
/// A column of type `date` carries the raw timestamp; kubectl renders it as an
/// age itself, so converting here would double-translate it.
fn crd_printer(columns: &[Value]) -> (Vec<Value>, Vec<(String, i64)>) {
    let mut cols = vec![col("Name", "string", 0, "Name of the object")];
    let mut paths = Vec::new();
    for c in columns {
        let Some(name) = c["name"].as_str() else { continue };
        let Some(path) = c["jsonPath"].as_str().or_else(|| c["JSONPath"].as_str()) else {
            continue;
        };
        let ty = c["type"].as_str().unwrap_or("string");
        let desc = c["description"].as_str().unwrap_or("");
        // `priority` > 0 is a wide-only column. kubectl asks for every column
        // and hides the wide ones itself, so they are all emitted.
        cols.push(col(name, ty, c["priority"].as_i64().unwrap_or(0), desc));
        paths.push((path.to_string(), 0i64));
    }
    if columns.is_empty() {
        cols.push(col("Age", "string", 0, "Time since creation"));
    }
    (cols, paths)
}

/// Convert a List into a Table using a CRD's declared printer columns.
///
/// Separate from [`to_table`] because the columns are data rather than code:
/// they come from the CustomResourceDefinition, so one implementation serves
/// every custom resource on the cluster instead of a printer per kind.
pub fn to_table_crd(body: Value, columns: &[Value]) -> Value {
    let (cols, paths) = crd_printer(columns);
    let items: Vec<Value> = match body.get("items").and_then(|i| i.as_array()) {
        Some(arr) => arr.clone(),
        None => vec![body.clone()],
    };
    let rows: Vec<Value> = items
        .iter()
        .map(|o| {
            let mut cells = vec![json!(name_of(o))];
            for (path, _) in &paths {
                cells.push(json_path(o, path).map(cell).unwrap_or(json!("")));
            }
            if paths.is_empty() {
                cells.push(json!(age(o)));
            }
            json!({ "cells": cells, "object": o })
        })
        .collect();
    json!({
        "kind": "Table",
        "apiVersion": "meta.k8s.io/v1",
        "metadata": body.get("metadata").cloned().unwrap_or(json!({})),
        "columnDefinitions": cols,
        "rows": rows,
    })
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

    /// The JSONPath subset a printer column may use: dotted fields and
    /// numeric indices. Anything unresolvable is an empty cell rather than an
    /// error — a column whose field is not set yet is normal.
    #[test]
    fn printer_column_paths_resolve_fields_and_indices() {
        let o = json!({
            "spec": { "replicas": 3, "name": "web" },
            "status": { "conditions": [{ "type": "Ready", "status": "True" }] },
        });
        assert_eq!(json_path(&o, ".spec.replicas"), Some(&json!(3)));
        assert_eq!(json_path(&o, ".spec.name"), Some(&json!("web")));
        assert_eq!(
            json_path(&o, ".status.conditions[0].type"),
            Some(&json!("Ready"))
        );
        assert_eq!(json_path(&o, ".status.conditions[1].type"), None);
        assert_eq!(json_path(&o, ".spec.missing"), None);
    }

    /// A CRD's own columns become the table's, with NAME first and AGE last.
    #[test]
    fn a_crd_prints_the_columns_it_declares() {
        let columns = vec![
            json!({"name": "Endpoint", "type": "string", "jsonPath": ".status.id"}),
            json!({"name": "Ready", "type": "string", "jsonPath": ".status.conditions[0].status"}),
        ];
        let list = json!({"items": [{
            "metadata": {"name": "cep-1"},
            "status": {"id": 42, "conditions": [{"status": "True"}]},
        }]});
        let t = to_table_crd(list, &columns);
        let names: Vec<&str> = t["columnDefinitions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["name"].as_str().unwrap())
            .collect();
        // No trailing Age: the CRD declared its own columns.
        assert_eq!(names, ["Name", "Endpoint", "Ready"]);
        let cells = &t["rows"][0]["cells"];
        assert_eq!(cells[0], json!("cep-1"));
        // A number stays a number: the column says type integer and a client
        // formatting by type would be handed a string otherwise.
        assert_eq!(cells[1], json!(42));
        assert_eq!(cells[2], json!("True"));
        assert_eq!(cells.as_array().unwrap().len(), 3, "no extra Age cell");
        // The whole object rides along, as every Table row must.
        assert_eq!(t["rows"][0]["object"]["metadata"]["name"], json!("cep-1"));
    }

    /// A CRD with no declared columns still gets a usable table rather than an
    /// empty one — this is the shape every CRD had before.
    #[test]
    fn a_crd_without_columns_still_prints_name_and_age() {
        let list = json!({"items": [{"metadata": {"name": "x"}}]});
        let t = to_table_crd(list, &[]);
        let names: Vec<&str> = t["columnDefinitions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["Name", "Age"]);
    }

    /// A single object, not a list — `oc get cnp foo` takes this path.
    /// A CRD that declares its own Age column must not get a second one —
    /// what CiliumNetworkPolicy does, and what printed "Age Age" before.
    #[test]
    fn a_crd_declaring_age_does_not_get_two() {
        let cols = vec![json!({
            "name": "Age", "type": "date", "jsonPath": ".metadata.creationTimestamp"})];
        let list = json!({"items": [{"metadata": {"name": "allow-dns"}}]});
        let t = to_table_crd(list, &cols);
        let names: Vec<&str> = t["columnDefinitions"]
            .as_array().unwrap().iter().map(|c| c["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["Name", "Age"]);
    }

    #[test]
    fn a_single_custom_resource_is_one_row() {
        let one = json!({"metadata": {"name": "only"}, "spec": {"n": 1}});
        let cols = vec![json!({"name": "N", "type": "integer", "jsonPath": ".spec.n"})];
        let t = to_table_crd(one, &cols);
        assert_eq!(t["rows"].as_array().unwrap().len(), 1);
        assert_eq!(t["rows"][0]["cells"][1], json!(1));
    }

    /// A Route prints what `oc get route` prints. The DaemonSet columns matter
    /// for Cilium in particular — it is the shape a pod network is deployed in.
    #[test]
    fn routes_services_and_daemonsets_print_their_own_columns() {
        let route = json!({"items": [{
            "metadata": {"name": "console"},
            "spec": {
                "host": "console.storm1.g8.lo",
                "to": {"name": "stormconsole"},
                "port": {"targetPort": 9094},
                "tls": {"termination": "edge"},
            },
        }]});
        let c = &to_table("routes", route)["rows"][0]["cells"];
        assert_eq!(c[1], json!("console.storm1.g8.lo"));
        assert_eq!(c[3], json!("stormconsole"));
        // A numeric targetPort prints as a number's text, not an empty cell.
        assert_eq!(c[4], json!("9094"));
        assert_eq!(c[5], json!("edge"));
        assert_eq!(c[6], json!("None"));

        let svc = json!({"items": [{
            "metadata": {"name": "api"},
            "spec": {"type": "NodePort", "clusterIP": "10.0.0.1",
                     "ports": [{"port": 443, "nodePort": 30443, "protocol": "TCP"}]},
        }]});
        let c = &to_table("services", svc)["rows"][0]["cells"];
        assert_eq!(c[1], json!("NodePort"));
        assert_eq!(c[4], json!("443:30443/TCP"));

        let ds = json!({"items": [{
            "metadata": {"name": "cilium"},
            "status": {"desiredNumberScheduled": 3, "numberReady": 2},
        }]});
        let c = &to_table("daemonsets", ds)["rows"][0]["cells"];
        assert_eq!(c[1], json!(3));
        assert_eq!(c[3], json!(2));
    }

    /// An ingress with no host is `*`, and with TLS it serves 443 too — both
    /// are what upstream prints and both read as wrong if guessed.
    #[test]
    fn an_ingress_without_a_host_prints_a_star() {
        let ing = json!({"items": [{
            "metadata": {"name": "i"},
            "spec": {"rules": [{}], "tls": [{"hosts": ["a"]}]},
        }]});
        let c = &to_table("ingresses", ing)["rows"][0]["cells"];
        assert_eq!(c[2], json!("*"));
        assert_eq!(c[4], json!("80, 443"));
    }
}
