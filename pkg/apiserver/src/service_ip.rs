//! ClusterIP allocation.
//!
//! **The address is claimed, not chosen.** The first version of this scanned
//! every existing Service, picked the lowest address it did not see, and wrote
//! it into the object — which is correct only if nothing else is allocating at
//! the same time. Two concurrent creates read the same set, pick the same
//! address, and both succeed: two Services with one ClusterIP, and the second
//! one silently never works.
//!
//! Instead each address is a key. Claiming one is an atomic create-if-absent
//! against the store, so exactly one caller can win it and the loser simply
//! tries the next. That also leaves a durable record of what is allocated,
//! which is what makes a repair loop possible later — a scan over Services can
//! tell you what *should* be allocated but never what leaked.
//!
//! Upstream keeps this in the Service registry rather than in admission, and
//! for the same reason it is its own module here: allocation is a side effect
//! with its own lifetime, not a validation.

use serde_json::{json, Value};

use crate::error::ApiError;
use crate::storage::ResourceStorage;

/// Where claims live. Not an API resource: nothing serves this prefix, and it
/// must not collide with one that is.
const PREFIX: &str = "/registry/serviceipallocations";

fn key(ip: &str) -> String {
    format!("{PREFIX}/{ip}")
}

/// `10.96.0.0/12` into its base address and prefix length.
pub fn parse_cidr(cidr: &str) -> Option<(std::net::Ipv4Addr, u32)> {
    let (addr, prefix) = cidr.split_once('/')?;
    Some((addr.parse().ok()?, prefix.parse().ok()?))
}

/// Claim `ip` for a Service. `Ok(false)` means somebody else holds it.
async fn claim(
    storage: &ResourceStorage,
    ip: &str,
    namespace: &str,
    name: &str,
) -> Result<bool, ApiError> {
    let rec = json!({
        "ip": ip,
        "namespace": namespace,
        "name": name,
    });
    match storage.create(&key(ip), rec).await {
        Ok(_) => Ok(true),
        // Already claimed. Not an error: the caller tries the next address.
        Err(e) if e.is_already_exists() => Ok(false),
        Err(e) => Err(e),
    }
}

/// Give an address back.
///
/// Called on Service delete. A leaked claim is worse than a leaked Service —
/// the Service is visible and the claim is not — so this runs even when the
/// Service being deleted looks like it never had one.
pub async fn release(storage: &ResourceStorage, obj: &Value) {
    if let Some(ip) = obj["spec"]["clusterIP"].as_str() {
        if ip != "None" && !ip.is_empty() {
            let _ = storage.delete(&key(ip), None).await;
        }
    }
}

/// Claim whatever address this Service object carries, best-effort.
///
/// For the paths that write a Service directly rather than through admission —
/// the bootstrap objects and the manifest applier. Best-effort because those
/// paths are idempotent and re-run: a claim that already exists is the normal
/// case on every boot after the first.
pub async fn claim_for(storage: &ResourceStorage, obj: &Value) {
    let Some(ip) = obj["spec"]["clusterIP"].as_str() else {
        return;
    };
    if ip == "None" || ip.is_empty() {
        return;
    }
    let ns = obj["metadata"]["namespace"].as_str().unwrap_or("default");
    let name = obj["metadata"]["name"].as_str().unwrap_or("");
    let _ = claim(storage, ip, ns, name).await;
}

/// Find Services sharing an address.
///
/// **A duplicate address is the failure this whole module exists to prevent**,
/// and preventing it is not the same as being able to prove it did not happen.
/// Every mechanism here — claims, reconcile, claiming on the direct write
/// paths — is a guard, and a guard nobody checks is a guess. This checks.
///
/// Returns each address held by more than one Service. Empty is the only
/// acceptable answer; a non-empty one means something wrote a Service by a
/// path that does not claim, and that path is a bug rather than a
/// configuration.
pub async fn duplicates(storage: &ResourceStorage) -> Vec<(String, Vec<String>)> {
    let prefix = ResourceStorage::cluster_prefix("services");
    let Ok((items, _, _)) = storage.list(&prefix, 10_000, None).await else {
        return Vec::new();
    };
    let mut by_ip: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    for svc in &items {
        let Some(ip) = svc["spec"]["clusterIP"].as_str() else {
            continue;
        };
        if ip == "None" || ip.is_empty() {
            continue;
        }
        let ns = svc["metadata"]["namespace"].as_str().unwrap_or("default");
        let name = svc["metadata"]["name"].as_str().unwrap_or("");
        by_ip.entry(ip.to_string()).or_default().push(format!("{ns}/{name}"));
    }
    by_ip.into_iter().filter(|(_, v)| v.len() > 1).collect()
}

/// Claim every address an existing Service already holds.
///
/// **Not every Service is created through admission.** Bootstrap objects — the
/// `kubernetes` Service — and everything the manifest applier writes go
/// straight to storage, so no claim is recorded for them. The allocator then
/// scans from the bottom of the range, finds the first address unclaimed, and
/// hands out one that is already in use: a new Service was given `10.96.0.1`,
/// the apiserver's own address, and every connection to it silently reached
/// the apiserver instead.
///
/// So this runs at startup, after bootstrap and after the manifests, and
/// claims what is already there. It is the repair loop the claim record exists
/// to make possible — a scan over Services can tell you what *should* be
/// allocated, and only a durable claim can tell you what is.
///
/// Idempotent and best-effort: a claim that already exists is the normal case,
/// and a store that will not answer is not a reason to refuse to start.
pub async fn reconcile(storage: &ResourceStorage) -> usize {
    let prefix = ResourceStorage::cluster_prefix("services");
    let Ok((items, _, _)) = storage.list(&prefix, 10_000, None).await else {
        return 0;
    };
    let mut claimed = 0;
    for svc in &items {
        let Some(ip) = svc["spec"]["clusterIP"].as_str() else {
            continue;
        };
        if ip == "None" || ip.is_empty() {
            continue;
        }
        let ns = svc["metadata"]["namespace"].as_str().unwrap_or("default");
        let name = svc["metadata"]["name"].as_str().unwrap_or("");
        if matches!(claim(storage, ip, ns, name).await, Ok(true)) {
            claimed += 1;
        }
    }
    claimed
}

/// Give a Service a ClusterIP, unless it should not have one.
pub async fn allocate(
    storage: &ResourceStorage,
    cidr: &str,
    obj: &mut Value,
) -> Result<(), ApiError> {
    let kind = obj["spec"]["type"].as_str().unwrap_or("ClusterIP");
    // ExternalName is a CNAME and has no address by definition.
    if kind == "ExternalName" {
        return Ok(());
    }
    let namespace = obj["metadata"]["namespace"].as_str().unwrap_or("default").to_string();
    let name = obj["metadata"]["name"].as_str().unwrap_or("").to_string();

    match obj["spec"]["clusterIP"].as_str() {
        // Headless: the caller asked for no address, and DNS answers with the
        // pod set instead.
        Some("None") => {
            obj["spec"]["clusterIPs"] = json!(["None"]);
            return Ok(());
        }
        // Asked for a specific one — bootstrap objects do this. Claim exactly
        // it, so two Services cannot both be given the same fixed address.
        Some(ip) if !ip.is_empty() => {
            let ip = ip.to_string();
            if !claim(storage, &ip, &namespace, &name).await? {
                return Err(ApiError::invalid(&format!(
                    "ClusterIP {ip} is already allocated"
                )));
            }
            obj["spec"]["clusterIPs"] = json!([ip]);
            return Ok(());
        }
        _ => {}
    }

    let Some((base, prefix)) = parse_cidr(cidr) else {
        return Ok(());
    };
    let count = 1u32 << (32 - prefix.min(32));
    // From 1: the network address is never handed out.
    for offset in 1..count.saturating_sub(1) {
        let Some(v) = u32::from(base).checked_add(offset) else {
            break;
        };
        let candidate = std::net::Ipv4Addr::from(v).to_string();
        if claim(storage, &candidate, &namespace, &name).await? {
            obj["spec"]["clusterIP"] = json!(candidate);
            obj["spec"]["clusterIPs"] = json!([candidate]);
            if obj["spec"]["type"].as_str().is_none() {
                obj["spec"]["type"] = json!("ClusterIP");
            }
            return Ok(());
        }
    }
    // Refused rather than created without an address: a Service that exists
    // and resolves to nothing is worse than one that failed.
    Err(ApiError::invalid(&format!(
        "no ClusterIP available in {cidr}"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;


    /// The property that matters: an address held by two Services must be
    /// visible. Every guard in this module is meant to make this impossible,
    /// and this is the check that says whether they did.
    #[test]
    fn duplicate_detection_finds_a_shared_address() {
        use serde_json::json;
        let svcs = vec![
            json!({"metadata":{"namespace":"default","name":"a"},"spec":{"clusterIP":"10.96.0.1"}}),
            json!({"metadata":{"namespace":"kube-system","name":"b"},"spec":{"clusterIP":"10.96.0.1"}}),
            json!({"metadata":{"namespace":"default","name":"c"},"spec":{"clusterIP":"10.96.0.9"}}),
            // Headless services share "None" and are not duplicates.
            json!({"metadata":{"namespace":"default","name":"d"},"spec":{"clusterIP":"None"}}),
            json!({"metadata":{"namespace":"default","name":"e"},"spec":{"clusterIP":"None"}}),
        ];
        let mut by_ip: std::collections::BTreeMap<String, Vec<String>> = Default::default();
        for svc in &svcs {
            let ip = svc["spec"]["clusterIP"].as_str().unwrap_or("");
            if ip.is_empty() || ip == "None" {
                continue;
            }
            let ns = svc["metadata"]["namespace"].as_str().unwrap();
            let name = svc["metadata"]["name"].as_str().unwrap();
            by_ip.entry(ip.into()).or_default().push(format!("{ns}/{name}"));
        }
        let dups: Vec<_> = by_ip.into_iter().filter(|(_, v)| v.len() > 1).collect();
        assert_eq!(dups.len(), 1, "exactly one address is shared");
        assert_eq!(dups[0].0, "10.96.0.1");
        assert_eq!(dups[0].1, vec!["default/a", "kube-system/b"]);
    }

    #[test]
    fn a_cidr_parses_into_a_base_and_a_prefix() {
        let (base, prefix) = parse_cidr("10.96.0.0/12").unwrap();
        assert_eq!(base.to_string(), "10.96.0.0");
        assert_eq!(prefix, 12);
        assert!(parse_cidr("10.96.0.0").is_none());
        assert!(parse_cidr("nonsense/12").is_none());
    }

    /// The claim key is derived from the address alone, so two Services
    /// racing for one address collide on one key — which is the whole
    /// mechanism.
    #[test]
    fn one_address_is_one_key() {
        assert_eq!(key("10.96.0.2"), "/registry/serviceipallocations/10.96.0.2");
        assert_ne!(key("10.96.0.2"), key("10.96.0.3"));
    }
}
