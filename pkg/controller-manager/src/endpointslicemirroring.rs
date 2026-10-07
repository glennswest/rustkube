//! EndpointSliceMirroring (#133): the EndpointSlices of a Service with no
//! selector, mirrored from the Endpoints someone wrote for it by hand, as
//! upstream's endpointslicemirroring controller makes them.
//!
//! Run by the Service controller (`service.rs`), which wakes on the
//! Service, its same-named Endpoints and the slices labelled with its name.
//! A Service with no selector whose Endpoints carries no
//! `endpointslice.kubernetes.io/skip-mirror: "true"` gets one slice per
//! address type and port set (at most [`MAX_PER_SLICE`] endpoints each):
//!
//! - labels: the Endpoints' own, then `kubernetes.io/service-name` and
//!   `endpointslice.kubernetes.io/managed-by:
//!   endpointslicemirroring-controller.k8s.io`;
//! - annotations: the Endpoints' own, but for
//!   `endpoints.kubernetes.io/last-change-trigger-time` and
//!   `kubectl.kubernetes.io/last-applied-configuration`;
//! - owner: the Endpoints, as controller — so deleting it collects them;
//! - an endpoint per address, `ready` true for `addresses`, false for
//!   `notReadyAddresses`, with its hostname, nodeName and targetRef;
//!   an address that is no IP is skipped.
//!
//! Slice names are the Endpoints name and a stable hash of the address type
//! and ports, so a re-run finds the slice it wrote instead of making another.
//! The slices go when the Service gains a selector, the Endpoints goes or is
//! marked skip-mirror, or the Service is deleted.

use serde_json::{json, Map, Value};

pub const MANAGED_BY: &str = "endpointslicemirroring-controller.k8s.io";
pub const MAX_PER_SLICE: usize = 1000;
const DROPPED_ANNOTATIONS: [&str; 2] =
    ["endpoints.kubernetes.io/last-change-trigger-time", "kubectl.kubernetes.io/last-applied-configuration"];

/// Does this Service select Pods? Absent, null and `{}` do not (upstream's
/// nil selector; an empty map does not survive its serialization).
pub fn has_selector(svc: &Value) -> bool {
    svc["spec"]["selector"].as_object().is_some_and(|m| !m.is_empty())
}

/// Should this Endpoints be mirrored at all?
pub fn mirrors(endpoints: &Value) -> bool {
    endpoints["metadata"]["labels"]["endpointslice.kubernetes.io/skip-mirror"].as_str() != Some("true")
        && endpoints["metadata"]["deletionTimestamp"].is_null()
}

/// Is this slice one of the mirroring controller's for `service` in `namespace`?
pub fn is_mirrored(slice: &Value, namespace: &str, service: &str) -> bool {
    let labels = &slice["metadata"]["labels"];
    slice["metadata"]["namespace"].as_str() == Some(namespace)
        && labels["kubernetes.io/service-name"].as_str() == Some(service)
        && labels["endpointslice.kubernetes.io/managed-by"].as_str() == Some(MANAGED_BY)
}

fn address_type(ip: &str) -> Option<&'static str> {
    if ip.parse::<std::net::Ipv4Addr>().is_ok() {
        Some("IPv4")
    } else if ip.parse::<std::net::Ipv6Addr>().is_ok() {
        Some("IPv6")
    } else {
        None
    }
}

fn fnv(s: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{:08x}", h & 0xFFFF_FFFF)
}

fn slice_port(p: &Value) -> Value {
    let mut out = Map::new();
    out.insert("name".into(), json!(p["name"].as_str().unwrap_or("")));
    out.insert("port".into(), p["port"].clone());
    out.insert("protocol".into(), json!(p["protocol"].as_str().unwrap_or("TCP")));
    if let Some(a) = p["appProtocol"].as_str() {
        out.insert("appProtocol".into(), json!(a));
    }
    Value::Object(out)
}

fn endpoint(addr: &Value, ready: bool) -> Value {
    let mut ep = json!({"addresses": [addr["ip"]], "conditions": {"ready": ready}});
    for field in ["hostname", "nodeName"] {
        if let Some(v) = addr[field].as_str().filter(|v| !v.is_empty()) {
            ep[field] = json!(v);
        }
    }
    if addr["targetRef"].is_object() {
        ep["targetRef"] = addr["targetRef"].clone();
    }
    ep
}

/// The slices the mirroring controller wants for `endpoints`, in a stable
/// order.
pub fn desired(endpoints: &Value) -> Vec<Value> {
    let meta = &endpoints["metadata"];
    let name = meta["name"].as_str().unwrap_or("");
    let namespace = meta["namespace"].as_str().unwrap_or("");
    // (address type, ports) → endpoints, in first-seen order.
    let mut groups: Vec<((&'static str, Value), Vec<Value>)> = Vec::new();
    for subset in endpoints["subsets"].as_array().into_iter().flatten() {
        let mut ports: Vec<Value> = subset["ports"].as_array().into_iter().flatten().map(slice_port).collect();
        ports.sort_by_key(|p| p.to_string());
        let ports = Value::Array(ports);
        for (list, ready) in [("addresses", true), ("notReadyAddresses", false)] {
            for addr in subset[list].as_array().into_iter().flatten() {
                let Some(kind) = addr["ip"].as_str().and_then(address_type) else { continue };
                let key = (kind, ports.clone());
                match groups.iter_mut().find(|(k, _)| *k == key) {
                    Some((_, eps)) => eps.push(endpoint(addr, ready)),
                    None => groups.push((key, vec![endpoint(addr, ready)])),
                }
            }
        }
    }
    let mut labels: Map<String, Value> = meta["labels"].as_object().cloned().unwrap_or_default();
    labels.insert("kubernetes.io/service-name".into(), json!(name));
    labels.insert("endpointslice.kubernetes.io/managed-by".into(), json!(MANAGED_BY));
    let mut annotations: Map<String, Value> = meta["annotations"].as_object().cloned().unwrap_or_default();
    for a in DROPPED_ANNOTATIONS {
        annotations.remove(a);
    }
    let base: String = name.chars().take(230).collect();
    let mut out = Vec::new();
    for ((kind, ports), eps) in groups {
        for (i, chunk) in eps.chunks(MAX_PER_SLICE).enumerate() {
            let mut metadata = json!({
                "name": format!("{base}-{}", fnv(&format!("{kind}/{ports}/{i}"))),
                "namespace": namespace,
                "labels": labels,
                "ownerReferences": [{"apiVersion": "v1", "kind": "Endpoints", "name": name, "uid": meta["uid"],
                                     "controller": true, "blockOwnerDeletion": true}],
            });
            if !annotations.is_empty() {
                metadata["annotations"] = json!(annotations);
            }
            out.push(json!({
                "apiVersion": "discovery.k8s.io/v1",
                "kind": "EndpointSlice",
                "metadata": metadata,
                "addressType": kind,
                "endpoints": chunk,
                "ports": ports,
            }));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoints(subsets: Value) -> Value {
        json!({"apiVersion": "v1", "kind": "Endpoints",
               "metadata": {"name": "example-custom-endpoints", "namespace": "ns", "uid": "u1",
                            "labels": {"app": "x"},
                            "annotations": {"note": "kept", "endpoints.kubernetes.io/last-change-trigger-time": "t"}},
               "subsets": subsets})
    }

    #[test]
    fn the_conformance_endpoints_mirror_into_one_slice() {
        let ep = endpoints(json!([{"addresses": [{"ip": "10.1.2.3"}], "ports": [{"name": "example", "port": 80, "protocol": "TCP"}]}]));
        let s = desired(&ep);
        assert_eq!(s.len(), 1);
        let s = &s[0];
        assert_eq!(s["addressType"], "IPv4");
        assert_eq!(s["endpoints"], json!([{"addresses": ["10.1.2.3"], "conditions": {"ready": true}}]));
        assert_eq!(s["ports"], json!([{"name": "example", "port": 80, "protocol": "TCP"}]));
        assert_eq!(s["metadata"]["labels"]["kubernetes.io/service-name"], "example-custom-endpoints");
        assert_eq!(s["metadata"]["labels"]["endpointslice.kubernetes.io/managed-by"], MANAGED_BY);
        assert_eq!(s["metadata"]["labels"]["app"], "x");
        assert_eq!(s["metadata"]["annotations"], json!({"note": "kept"}));
        assert_eq!(s["metadata"]["ownerReferences"][0]["kind"], "Endpoints");
        assert_eq!(s["metadata"]["ownerReferences"][0]["uid"], "u1");
        assert!(is_mirrored(s, "ns", "example-custom-endpoints"));
        // An update of the address keeps the slice's name.
        let moved = desired(&endpoints(json!([{"addresses": [{"ip": "10.2.3.4"}], "ports": [{"name": "example", "port": 80, "protocol": "TCP"}]}])));
        assert_eq!(moved[0]["metadata"]["name"], s["metadata"]["name"]);
    }

    #[test]
    fn split_by_address_type_and_ports_merged_by_ports() {
        let ep = endpoints(json!([
            {"addresses": [{"ip": "10.0.0.1", "nodeName": "n1", "hostname": "h"}, {"ip": "fd00::1"}, {"ip": "not-an-ip"}],
             "notReadyAddresses": [{"ip": "10.0.0.2", "targetRef": {"kind": "Pod", "name": "p"}}],
             "ports": [{"port": 80}]},
            {"addresses": [{"ip": "10.0.0.3"}], "ports": [{"port": 80, "protocol": "TCP", "name": ""}]},
            {"addresses": [{"ip": "10.0.0.4"}], "ports": [{"port": 443}]},
        ]));
        let s = desired(&ep);
        let summary: Vec<(String, usize)> =
            s.iter().map(|s| (s["addressType"].as_str().unwrap().to_string(), s["endpoints"].as_array().unwrap().len())).collect();
        assert_eq!(summary, vec![("IPv4".into(), 3), ("IPv6".into(), 1), ("IPv4".into(), 1)]);
        assert_eq!(s[0]["endpoints"][0], json!({"addresses": ["10.0.0.1"], "conditions": {"ready": true}, "hostname": "h", "nodeName": "n1"}));
        assert_eq!(s[0]["endpoints"][1]["conditions"]["ready"], false);
        assert_eq!(s[0]["endpoints"][1]["targetRef"]["name"], "p");
        let names: std::collections::HashSet<_> = s.iter().map(|s| s["metadata"]["name"].clone()).collect();
        assert_eq!(names.len(), 3, "distinct names");
    }

    #[test]
    fn what_is_and_is_not_mirrored() {
        assert!(!has_selector(&json!({"spec": {}})));
        assert!(!has_selector(&json!({"spec": {"selector": {}}})));
        assert!(has_selector(&json!({"spec": {"selector": {"app": "a"}}})));
        let mut ep = endpoints(json!([]));
        assert!(mirrors(&ep));
        assert!(desired(&ep).is_empty());
        ep["metadata"]["labels"]["endpointslice.kubernetes.io/skip-mirror"] = json!("true");
        assert!(!mirrors(&ep));
    }
}
