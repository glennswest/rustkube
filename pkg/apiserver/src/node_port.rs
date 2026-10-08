//! NodePort allocation, and what a Service's allocations do when its type
//! changes (#132).
//!
//! Ports are claimed the way `service_ip.rs` claims addresses: each one is a
//! key under [`PREFIX`], taken by an atomic create-if-absent, so two
//! apiservers can never hand out one port. A dynamic port is a random free
//! one from the watch cache's view of the claims, within
//! `--service-node-port-range` (default `30000-32767`).
//!
//! Which ports a Service holds:
//! - `NodePort`, and `LoadBalancer` unless `allocateLoadBalancerNodePorts:
//!   false`: every port, a named one claimed exactly (taken: 422 "provided
//!   port is already allocated"; outside the range: 422), a zero or absent
//!   one allocated. One port number may serve two protocols of one Service.
//! - `LoadBalancer` with `allocateLoadBalancerNodePorts: false`: only the
//!   ports it names.
//! - `ClusterIP` and `ExternalName`: none; naming one is 422.
//!
//! On an update ([`Plan`]), as upstream's Service strategy and REST update:
//! - a port or ClusterIP the body leaves empty keeps the stored value — node
//!   ports by port name, as upstream (a PUT of an edited `kubectl get -o
//!   yaml` does not move them);
//! - a ClusterIP, once set, does not change (422);
//! - a type change to one without node ports drops the ports it carried
//!   over, to `ExternalName` drops the ClusterIP; from `ExternalName` a
//!   ClusterIP is allocated, to `NodePort`/`LoadBalancer` node ports are;
//! - new claims are made before the write and given back if it fails;
//!   claims the old object held and the new one does not are released only
//!   once the write has landed.

use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::OnceLock;

use crate::error::ApiError;
use crate::storage::ResourceStorage;

const PREFIX: &str = "/registry/servicenodeportallocations";
static RANGE: OnceLock<(u16, u16)> = OnceLock::new();
pub const DEFAULT_RANGE: &str = "30000-32767";

fn key(port: u16) -> String {
    format!("{PREFIX}/{port}")
}

/// `30000-32767` into its bounds, inclusive.
pub fn parse_range(s: &str) -> Option<(u16, u16)> {
    let (a, b) = s.split_once('-')?;
    let (a, b): (u16, u16) = (a.trim().parse().ok()?, b.trim().parse().ok()?);
    (a > 0 && a <= b).then_some((a, b))
}

/// Set `--service-node-port-range`, once, at startup.
pub fn set_range(s: &str) -> anyhow::Result<()> {
    let r = parse_range(s).ok_or_else(|| anyhow::anyhow!("--service-node-port-range {s:?}: want FROM-TO, e.g. 30000-32767"))?;
    let _ = RANGE.set(r);
    Ok(())
}

fn range() -> (u16, u16) {
    *RANGE.get().unwrap_or(&(30000, 32767))
}

fn svc_type(svc: &Value) -> &str {
    svc["spec"]["type"].as_str().unwrap_or("ClusterIP")
}

/// Does this Service get a node port on every port?
fn allocates(svc: &Value) -> bool {
    match svc_type(svc) {
        "NodePort" => true,
        "LoadBalancer" => svc["spec"]["allocateLoadBalancerNodePorts"].as_bool() != Some(false),
        _ => false,
    }
}

fn may_have(svc: &Value) -> bool {
    matches!(svc_type(svc), "NodePort" | "LoadBalancer")
}

fn has_cluster_ip(svc: &Value) -> bool {
    svc_type(svc) != "ExternalName"
}

fn node_port(p: &Value) -> u16 {
    p["nodePort"].as_u64().filter(|n| *n <= u16::MAX as u64).unwrap_or(0) as u16
}

fn proto(p: &Value) -> &str {
    p["protocol"].as_str().unwrap_or("TCP")
}

/// The node ports an object holds.
pub fn held(svc: &Value) -> HashSet<u16> {
    svc["spec"]["ports"].as_array().into_iter().flatten().map(node_port).filter(|n| *n != 0).collect()
}

fn invalid(svc: &Value, errs: &[String]) -> ApiError {
    let name = svc["metadata"]["name"].as_str().unwrap_or("");
    ApiError::invalid(&format!("Service \"{name}\" is invalid: {}", errs.join(", ")))
}

async fn claim(storage: &ResourceStorage, port: u16, svc: &Value) -> Result<bool, ApiError> {
    let rec = json!({"port": port, "namespace": svc["metadata"]["namespace"], "name": svc["metadata"]["name"]});
    match storage.create(&key(port), rec).await {
        Ok(_) => Ok(true),
        Err(e) if e.is_already_exists() => Ok(false),
        Err(e) => Err(e),
    }
}

async fn claimed_view(storage: &ResourceStorage) -> HashSet<u16> {
    let prefix = format!("{PREFIX}/");
    match storage.watch_cache().snapshot(&prefix).await {
        Ok((_, items)) => items.iter().filter_map(|(k, _)| k.strip_prefix(&prefix)?.parse().ok()).collect(),
        Err(_) => Default::default(),
    }
}

/// What a Service write claimed, and what it lets go of once it lands.
#[derive(Debug, Default)]
pub struct Plan {
    claimed: Vec<String>,
    release: Vec<String>,
}

impl Plan {
    /// The write landed: give back what the old object held and the new does not.
    pub async fn commit(self, storage: &ResourceStorage) {
        for k in self.release {
            let _ = storage.delete(&k, None).await;
        }
    }
    /// The write failed: give back what this attempt claimed.
    pub async fn abort(self, storage: &ResourceStorage) {
        for k in self.claimed {
            let _ = storage.delete(&k, None).await;
        }
    }
}

/// Fill what the body left empty from `old` and drop what the type change
/// leaves behind; then refuse what may not be.
fn patch_and_check(old: Option<&Value>, new: &mut Value) -> Result<(), ApiError> {
    let mut errs = Vec::new();
    if let Some(old) = old {
        // ClusterIP: kept, carried over, or dropped with the type.
        let (oip, nip) = (old["spec"]["clusterIP"].as_str().unwrap_or(""), new["spec"]["clusterIP"].as_str().unwrap_or(""));
        if has_cluster_ip(old) && !oip.is_empty() {
            if has_cluster_ip(new) {
                if nip.is_empty() {
                    new["spec"]["clusterIP"] = json!(oip);
                    if old["spec"]["clusterIPs"].is_array() {
                        new["spec"]["clusterIPs"] = old["spec"]["clusterIPs"].clone();
                    }
                } else if nip != oip {
                    errs.push(format!("spec.clusterIP: Invalid value: \"{nip}\": field is immutable"));
                }
            } else if nip.is_empty() || nip == oip {
                if let Some(spec) = new["spec"].as_object_mut() {
                    spec.remove("clusterIP");
                    spec.remove("clusterIPs");
                }
            }
        }
        // Node ports: a zero one keeps the stored port of the same port
        // *name*, as upstream's patchAllocatedValues maps them (so a port
        // whose number changed keeps its node port); a type without them
        // drops the ones carried over.
        let old_ports = old["spec"]["ports"].as_array().cloned().unwrap_or_default();
        let (keep, new_may) = (may_have(old), may_have(new));
        if let Some(ports) = new["spec"]["ports"].as_array_mut() {
            for p in ports.iter_mut() {
                let pname = p["name"].as_str().unwrap_or("");
                let prev = old_ports.iter().find(|o| o["name"].as_str().unwrap_or("") == pname).map(node_port).unwrap_or(0);
                if prev == 0 {
                    continue;
                }
                if new_may && keep && node_port(p) == 0 {
                    p["nodePort"] = json!(prev);
                } else if !new_may && node_port(p) == prev {
                    p.as_object_mut().unwrap().remove("nodePort");
                }
            }
        }
    }
    if !may_have(new) {
        for (i, p) in new["spec"]["ports"].as_array().into_iter().flatten().enumerate() {
            if node_port(p) != 0 {
                errs.push(format!("spec.ports[{i}].nodePort: Forbidden: may not be used when `type` is '{}'", svc_type(new)));
            }
        }
    }
    let (lo, hi) = range();
    for (i, p) in new["spec"]["ports"].as_array().into_iter().flatten().enumerate() {
        let n = node_port(p);
        if n != 0 && may_have(new) && !(lo..=hi).contains(&n) {
            errs.push(format!(
                "spec.ports[{i}].nodePort: Invalid value: {n}: provided port is not in the valid range. The range of valid ports is {lo}-{hi}"
            ));
        }
    }
    if errs.is_empty() { Ok(()) } else { Err(invalid(new, &errs)) }
}

/// Allocate what `new` needs beyond what `old` holds: its ClusterIP, then
/// its node ports. On any refusal, whatever this call claimed is given back.
pub async fn plan(storage: &ResourceStorage, cidr: &str, old: Option<&Value>, new: &mut Value) -> Result<Plan, ApiError> {
    patch_and_check(old, new)?;
    let mut plan = Plan::default();
    let result = allocate(storage, cidr, old, new, &mut plan).await;
    if let Err(e) = result {
        plan.abort(storage).await;
        return Err(e);
    }
    Ok(plan)
}

async fn allocate(storage: &ResourceStorage, cidr: &str, old: Option<&Value>, new: &mut Value, plan: &mut Plan) -> Result<(), ApiError> {
    // ClusterIP: a create, or a change from ExternalName, allocates one.
    let old_ip = old.filter(|o| has_cluster_ip(o)).and_then(|o| o["spec"]["clusterIP"].as_str()).filter(|ip| !ip.is_empty() && *ip != "None");
    if has_cluster_ip(new) && old_ip.is_none() {
        crate::service_ip::allocate(storage, cidr, new).await?;
        if let Some(ip) = new["spec"]["clusterIP"].as_str().filter(|ip| !ip.is_empty() && *ip != "None") {
            plan.claimed.push(crate::service_ip::claim_key(ip));
        }
    }
    if let Some(ip) = old_ip {
        if new["spec"]["clusterIP"].as_str() != Some(ip) {
            plan.release.push(crate::service_ip::claim_key(ip));
        }
    }

    // Node ports.
    let before = old.map(held).unwrap_or_default();
    let mut taken_here: HashSet<u16> = HashSet::new();
    let mut by_number: Vec<(u16, String)> = Vec::new(); // (port, protocol) seen in this object
    let mut errs = Vec::new();
    let n_ports = new["spec"]["ports"].as_array().map_or(0, Vec::len);
    for i in 0..n_ports {
        let p = &new["spec"]["ports"][i];
        let n = node_port(p);
        if n == 0 {
            continue;
        }
        let pr = proto(p).to_string();
        if by_number.iter().any(|(x, q)| *x == n && *q == pr) {
            errs.push(format!("spec.ports[{i}].nodePort: Duplicate value: {n}"));
            continue;
        }
        by_number.push((n, pr));
        if before.contains(&n) || taken_here.contains(&n) {
            taken_here.insert(n);
            continue;
        }
        if claim(storage, n, new).await? {
            plan.claimed.push(key(n));
            taken_here.insert(n);
        } else {
            errs.push(format!("spec.ports[{i}].nodePort: Invalid value: {n}: provided port is already allocated"));
        }
    }
    if !errs.is_empty() {
        return Err(invalid(new, &errs));
    }
    if allocates(new) {
        let (lo, hi) = range();
        let mut view = claimed_view(storage).await;
        let span = (hi - lo) as u64 + 1;
        for i in 0..n_ports {
            if node_port(&new["spec"]["ports"][i]) != 0 {
                continue;
            }
            let start = uuid::Uuid::new_v4().as_u128() as u64 % span;
            let mut got = None;
            for off in 0..span {
                let port = lo + ((start + off) % span) as u16;
                if view.contains(&port) || taken_here.contains(&port) || before.contains(&port) {
                    continue;
                }
                if claim(storage, port, new).await? {
                    got = Some(port);
                    break;
                }
                view.insert(port); // claimed by someone the view had not seen
            }
            let Some(port) = got else {
                return Err(invalid(new, &[format!("spec.ports[{i}].nodePort: Internal error: range is full ({lo}-{hi})")]));
            };
            plan.claimed.push(key(port));
            taken_here.insert(port);
            new["spec"]["ports"][i]["nodePort"] = json!(port);
        }
    }
    for n in before.difference(&taken_here) {
        plan.release.push(key(*n));
    }
    Ok(())
}

/// Give back everything a Service holds: its ClusterIP and node ports —
/// on delete, and when a create that claimed them failed.
pub async fn release_all(storage: &ResourceStorage, svc: &Value) {
    crate::service_ip::release(storage, svc).await;
    release(storage, svc).await;
}

/// Give back every node port a Service holds.
pub async fn release(storage: &ResourceStorage, svc: &Value) {
    for n in held(svc) {
        let _ = storage.delete(&key(n), None).await;
    }
}

/// Claim the node ports Services already hold, best-effort: for the objects
/// written straight to storage (manifests) and at startup.
pub async fn claim_for(storage: &ResourceStorage, svc: &Value) {
    for n in held(svc) {
        let _ = claim(storage, n, svc).await;
    }
}

pub async fn reconcile(storage: &ResourceStorage) -> usize {
    let prefix = ResourceStorage::cluster_prefix("services");
    let Ok((items, _, _)) = storage.list(&prefix, 10_000, None).await else { return 0 };
    let mut n = 0;
    for svc in &items {
        for port in held(svc) {
            if matches!(claim(storage, port, svc).await, Ok(true)) {
                n += 1;
            }
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    fn svc(t: &str, ports: Value) -> Value {
        json!({"metadata": {"name": "s", "namespace": "d"}, "spec": {"type": t, "ports": ports}})
    }

    #[test]
    fn a_range_parses() {
        assert_eq!(parse_range("30000-32767"), Some((30000, 32767)));
        assert_eq!(parse_range("32767-30000"), None);
        assert_eq!(parse_range("x"), None);
    }

    #[test]
    fn an_update_keeps_what_it_left_empty_and_drops_what_the_type_leaves() {
        let old = json!({"metadata": {"name": "s"}, "spec": {"type": "NodePort", "clusterIP": "10.96.0.9", "clusterIPs": ["10.96.0.9"],
            "ports": [{"name": "http", "port": 80, "nodePort": 30080}, {"name": "dns", "port": 53, "protocol": "UDP", "nodePort": 30053}]}});
        // A PUT without the allocated values keeps them, by port name — a
        // port whose number changed too.
        let mut new = svc("NodePort", json!([{"name": "http", "port": 8080}, {"name": "dns", "port": 53, "protocol": "UDP"}]));
        patch_and_check(Some(&old), &mut new).unwrap();
        assert_eq!(new["spec"]["clusterIP"], "10.96.0.9");
        assert_eq!(new["spec"]["ports"][0]["nodePort"], 30080);
        assert_eq!(new["spec"]["ports"][1]["nodePort"], 30053);
        // To ClusterIP: the carried-over node ports go.
        let mut new = old.clone();
        new["spec"]["type"] = json!("ClusterIP");
        patch_and_check(Some(&old), &mut new).unwrap();
        assert!(held(&new).is_empty());
        assert_eq!(new["spec"]["clusterIP"], "10.96.0.9");
        // To ExternalName: the ClusterIP goes too.
        let mut new = old.clone();
        new["spec"]["type"] = json!("ExternalName");
        new["spec"]["externalName"] = json!("x.example.com");
        patch_and_check(Some(&old), &mut new).unwrap();
        assert!(new["spec"].get("clusterIP").is_none() && held(&new).is_empty());
        // A changed ClusterIP is refused.
        let mut new = old.clone();
        new["spec"]["clusterIP"] = json!("10.96.0.10");
        assert!(patch_and_check(Some(&old), &mut new).unwrap_err().message.contains("spec.clusterIP: Invalid value: \"10.96.0.10\": field is immutable"));
    }

    #[test]
    fn what_a_create_may_name() {
        let mut s = svc("ClusterIP", json!([{"port": 80, "nodePort": 30080}]));
        assert!(patch_and_check(None, &mut s).unwrap_err().message.contains("spec.ports[0].nodePort: Forbidden: may not be used when `type` is 'ClusterIP'"));
        let mut s = svc("NodePort", json!([{"port": 80, "nodePort": 80}]));
        assert!(patch_and_check(None, &mut s).unwrap_err().message
            .contains("spec.ports[0].nodePort: Invalid value: 80: provided port is not in the valid range. The range of valid ports is 30000-32767"));
        let mut s = svc("NodePort", json!([{"port": 80, "nodePort": 30080}]));
        assert!(patch_and_check(None, &mut s).is_ok());
        assert!(allocates(&svc("LoadBalancer", json!([]))));
        let mut lb = svc("LoadBalancer", json!([]));
        lb["spec"]["allocateLoadBalancerNodePorts"] = json!(false);
        assert!(!allocates(&lb) && may_have(&lb));
    }
}
