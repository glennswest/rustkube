//! Scheduling a VirtualMachineInstance.
//!
//! Nothing placed a VMI. The scheduler looked only at pods, and rustkube-node's
//! kubelet refuses to touch a VMI that names no node — correctly, because a
//! kubelet that claimed unassigned work would start the machine on every node
//! at once. Its comment named the component that was missing:
//!
//! > `status.nodeName` first, **because that is what the scheduler writes**,
//! > then `spec.nodeName` for one placed by hand.
//!
//! So a VM only ever ran if somebody set `spec.nodeName` themselves, and one
//! created from the console never started and never said why (rustkube#72).
//!
//! ## A VMI is scheduled as the pod it would have been
//!
//! Upstream KubeVirt does not schedule VMIs either: it creates a
//! **virt-launcher pod** per VMI and lets the ordinary scheduler place that.
//! There is no launcher pod here — the kubelet starts the hypervisor as a
//! stormpump workload directly — so the faithful analogue is to build the pod
//! that would have existed, use it to choose a node, and throw it away.
//!
//! That is [`scheduling_shim`], and it is why this module is small: every
//! filter and every score plugin runs on it unchanged. Taints, node selectors,
//! node affinity, inter-pod affinity, topology spread and resource fit all
//! work on a VM the day they are written for a pod, rather than being
//! reimplemented here and drifting.
//!
//! The one thing the shim must get right is what a VM *costs*, which is not
//! what a pod costs — see [`requests`].

use serde_json::{json, Value};

/// Every VirtualMachineInstance in the cluster, in one call.
///
/// Cluster-wide rather than per namespace: the pod pass already lists per
/// namespace because it must, and there is no reason to add one round trip
/// per namespace per second for an object most namespaces do not have.
pub const LIST_PATH: &str = "/apis/kubevirt.io/v1/virtualmachineinstances";

/// Where a VMI's placement is written.
///
/// **`status.nodeName`, through the status subresource** — not `spec`. A pod
/// is bound by writing `spec.nodeName`; a VMI is placed by patching its
/// status, and `spec.nodeName` is the manual override a person sets. The
/// apiserver's own kubevirt handler depends on the same distinction, and
/// getting it backwards would produce a VM that the kubelet ignores and a
/// spec the user did not write.
pub fn status_path(namespace: &str, name: &str) -> String {
    format!("/apis/kubevirt.io/v1/namespaces/{namespace}/virtualmachineinstances/{name}/status")
}

/// The node this VMI is on, if any.
///
/// `status.nodeName` is the scheduler's answer; `spec.nodeName` is a person's.
/// Both count as placed, because the kubelet honours both — a VM pinned by
/// hand must not be scheduled a second time, and it must still be counted
/// against the node it is actually running on.
pub fn node_of(vmi: &Value) -> Option<&str> {
    let status = vmi["status"]["nodeName"].as_str().filter(|s| !s.is_empty());
    status.or_else(|| vmi["spec"]["nodeName"].as_str().filter(|s| !s.is_empty()))
}

/// A VM that has finished. Its resources are the node's again.
pub fn is_terminal(vmi: &Value) -> bool {
    matches!(vmi["status"]["phase"].as_str(), Some("Succeeded") | Some("Failed"))
}

/// What a VM costs a node, in milli-CPU and bytes.
///
/// **Memory is a hard request and CPU usually is not**, and treating them the
/// same is the mistake this function exists to avoid.
///
/// A guest's memory is allocated by the hypervisor and is not given back: if a
/// node promises 8 GiB to a VM, that 8 GiB is gone whether the guest touches
/// it or not. So `domain.memory.guest` — or the resource request that older
/// objects carry, which is what `stormvm_spec::kube::from_kube` reads in the
/// same order — counts in full.
///
/// vCPUs are timeshared. A four-core VM on an eight-core node is ordinary, and
/// charging four whole cores for it would make a node look full while it idles
/// — which is why upstream takes a VM's CPU request from
/// `domain.resources.requests.cpu` and not from the core count. The exception
/// is `dedicatedCpuPlacement`, where the cores really are pinned and exclusive
/// and the count *is* the request.
pub fn requests(vmi: &Value) -> (u64, u64) {
    let domain = &vmi["spec"]["domain"];
    let res = &domain["resources"]["requests"];

    let memory = domain["memory"]["guest"]
        .as_str()
        .or_else(|| res["memory"].as_str())
        .or_else(|| domain["resources"]["limits"]["memory"].as_str())
        .map(crate::filter::parse_memory_bytes)
        .unwrap_or(0);

    let cpu = if let Some(req) = res["cpu"].as_str() {
        // Stated outright. Whatever the core count is, this is the promise.
        crate::filter::parse_cpu_millis(req)
    } else if domain["cpu"]["dedicatedCpuPlacement"].as_bool().unwrap_or(false) {
        // Pinned cores are exclusive, so the vCPU count is the request.
        // The product of cores × sockets × threads, each defaulting to 1,
        // is the vCPU count — the same arithmetic the spec converter does,
        // because two answers to "how many CPUs is this" is one too many.
        vcpus(vmi) * 1000
    } else {
        // Timeshared. The node's CPU is not promised away.
        0
    };

    (cpu, memory)
}

/// vCPUs: `cores × sockets × threads`, each defaulting to 1.
///
/// KubeVirt's `cpu.cores` is per socket, which is why this is a product and
/// not a field. Mirrors `stormvm_spec::kube::from_kube`.
pub fn vcpus(vmi: &Value) -> u64 {
    let n = |v: &Value| v.as_u64().unwrap_or(1).max(1);
    let cpu = &vmi["spec"]["domain"]["cpu"];
    n(&cpu["cores"]) * n(&cpu["sockets"]) * n(&cpu["threads"])
}

/// The VMI, shaped as the pod the scheduler already knows how to place.
///
/// Only ever used to *choose* a node. Nothing is created from it and it is
/// never sent anywhere — the placement is written back to the VMI's own
/// status.
///
/// What carries across, and why each one:
///
/// - **labels**, because inter-pod affinity and topology spread match on them,
///   and a VM that should not land beside its replica says so the same way a
///   pod does.
/// - **`nodeSelector`, `affinity`, `tolerations`**, which the spec converter
///   deliberately does not translate — "`nodeSelector` and tolerations are the
///   scheduler's" — and which nothing has read until now.
/// - **the requests**, on a single container. Resource fit reads pod-level
///   requests too since #73 (before that it read only containers, and a
///   pod-level shim fit on any node however full); one container, named for
///   what it is, is still the closer analogue — a virt-launcher pod has one.
pub fn scheduling_shim(vmi: &Value) -> Value {
    let (cpu, mem) = requests(vmi);
    let spec = &vmi["spec"];
    let mut shim = json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {
            "name": vmi["metadata"]["name"].clone(),
            "namespace": vmi["metadata"]["namespace"].clone(),
            "labels": vmi["metadata"]["labels"].clone(),
        },
        "spec": {
            "containers": [{
                "name": "vm",
                "resources": {"requests": {
                    "cpu": format!("{cpu}m"),
                    "memory": format!("{mem}"),
                }},
            }],
        },
        "status": {"phase": "Pending"},
    });
    // Copied rather than defaulted: a field the VMI does not set must be
    // absent on the shim too, because `null` and "not present" are the same
    // to the filters and an empty object is not.
    for key in ["nodeSelector", "affinity", "tolerations", "priorityClassName", "schedulerName"] {
        if !spec[key].is_null() {
            shim["spec"][key] = spec[key].clone();
        }
    }
    shim
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vmi(spec: Value) -> Value {
        json!({
            "apiVersion": "kubevirt.io/v1",
            "kind": "VirtualMachineInstance",
            "metadata": {"name": "web", "namespace": "default"},
            "spec": spec,
        })
    }

    #[test]
    fn memory_counts_in_full_because_a_guest_does_not_give_it_back() {
        let (_, mem) = requests(&vmi(json!({"domain": {"memory": {"guest": "8Gi"}}})));
        assert_eq!(mem, 8 * 1024 * 1024 * 1024);
    }

    #[test]
    fn memory_falls_back_the_way_the_spec_converter_reads_it() {
        // `domain.memory.guest`, then the request, then the limit — the same
        // order `stormvm_spec::kube::from_kube` uses, because two answers to
        // "how much memory is this VM" is one too many.
        let req = vmi(json!({"domain": {"resources": {"requests": {"memory": "2Gi"}}}}));
        assert_eq!(requests(&req).1, 2 * 1024 * 1024 * 1024);
        let lim = vmi(json!({"domain": {"resources": {"limits": {"memory": "512Mi"}}}}));
        assert_eq!(requests(&lim).1, 512 * 1024 * 1024);
        // And guest wins over both.
        let both = vmi(json!({"domain": {
            "memory": {"guest": "4Gi"},
            "resources": {"requests": {"memory": "1Gi"}}
        }}));
        assert_eq!(requests(&both).1, 4 * 1024 * 1024 * 1024);
    }

    #[test]
    fn timeshared_vcpus_do_not_reserve_cpu() {
        // A four-core VM on an eight-core node is ordinary. Charging four
        // whole cores would make the node look full while it idles.
        let v = vmi(json!({"domain": {"cpu": {"cores": 2, "sockets": 2}, "memory": {"guest": "1Gi"}}}));
        assert_eq!(requests(&v).0, 0);
    }

    #[test]
    fn dedicated_placement_reserves_the_cores_it_pins() {
        // Pinned cores are exclusive, so the count is the promise.
        let v = vmi(json!({"domain": {
            "cpu": {"cores": 2, "sockets": 2, "dedicatedCpuPlacement": true},
            "memory": {"guest": "1Gi"}
        }}));
        assert_eq!(requests(&v).0, 4000, "cores × sockets × threads, each defaulting to 1");
    }

    #[test]
    fn a_stated_cpu_request_wins_over_both() {
        let v = vmi(json!({"domain": {
            "cpu": {"cores": 8, "dedicatedCpuPlacement": true},
            "resources": {"requests": {"cpu": "500m"}},
            "memory": {"guest": "1Gi"}
        }}));
        assert_eq!(requests(&v).0, 500);
    }

    #[test]
    fn vcpus_default_each_factor_to_one() {
        assert_eq!(vcpus(&vmi(json!({"domain": {}}))), 1);
        assert_eq!(vcpus(&vmi(json!({"domain": {"cpu": {"cores": 4}}}))), 4);
        assert_eq!(
            vcpus(&vmi(json!({"domain": {"cpu": {"cores": 2, "sockets": 2, "threads": 2}}}))),
            8
        );
    }

    #[test]
    fn placement_is_read_from_status_first_and_spec_second() {
        let mut v = vmi(json!({"domain": {}}));
        assert_eq!(node_of(&v), None);
        // A VM pinned by hand is placed, and must not be scheduled again.
        v["spec"]["nodeName"] = json!("n2");
        assert_eq!(node_of(&v), Some("n2"));
        // The scheduler's own answer wins.
        v["status"]["nodeName"] = json!("n1");
        assert_eq!(node_of(&v), Some("n1"));
    }

    #[test]
    fn an_empty_node_name_is_not_a_placement() {
        // An object that carries the key with nothing in it is unscheduled,
        // and treating it as placed would strand the VM forever.
        let v = vmi(json!({"domain": {}, "nodeName": ""}));
        assert_eq!(node_of(&v), None);
    }

    #[test]
    fn a_finished_vm_has_given_its_resources_back() {
        let mut v = vmi(json!({"domain": {}}));
        assert!(!is_terminal(&v));
        v["status"]["phase"] = json!("Running");
        assert!(!is_terminal(&v));
        for done in ["Succeeded", "Failed"] {
            v["status"]["phase"] = json!(done);
            assert!(is_terminal(&v), "{done}");
        }
    }

    #[test]
    fn the_shim_carries_what_the_converter_calls_the_schedulers() {
        // "`nodeSelector` and tolerations are the scheduler's" — and nothing
        // read them until now.
        let v = vmi(json!({
            "domain": {"memory": {"guest": "2Gi"}},
            "nodeSelector": {"disk": "nvme"},
            "tolerations": [{"key": "vm", "operator": "Exists"}],
            "affinity": {"nodeAffinity": {}}
        }));
        let shim = scheduling_shim(&v);
        assert_eq!(shim["spec"]["nodeSelector"]["disk"], "nvme");
        assert_eq!(shim["spec"]["tolerations"][0]["key"], "vm");
        assert!(!shim["spec"]["affinity"].is_null());
        // On a container, as a virt-launcher pod carries them.
        let req = &shim["spec"]["containers"][0]["resources"]["requests"];
        assert_eq!(req["memory"], "2147483648");
        assert_eq!(req["cpu"], "0m");
    }

    #[test]
    fn the_requests_are_where_the_resource_filter_actually_reads_them() {
        // An 8 GiB guest must not fit a 4 GiB node. This first failed with
        // the requests at pod level, which resource fit ignored until #73.
        let v = vmi(json!({"domain": {"memory": {"guest": "8Gi"}}}));
        let shim = scheduling_shim(&v);
        let node = json!({
            "metadata": {"name": "n1"},
            "status": {
                "conditions": [{"type": "Ready", "status": "True"}],
                "allocatable": {"cpu": "8", "memory": "4Gi"}
            }
        });
        let state = crate::scheduler::ClusterState::default();
        let result = crate::filter::run_filters(
            &shim,
            &node,
            crate::filter::NodeUsage::default(),
            &state,
            std::slice::from_ref(&node),
        );
        assert!(
            matches!(result, crate::filter::FilterResult::Fail(_)),
            "an 8 GiB guest must not fit a node with 4 GiB allocatable, got {result:?}"
        );
    }

    #[test]
    fn a_vm_already_on_a_node_is_charged_to_it() {
        // The other half of the same claim: a node running an 8 GiB guest has
        // 8 GiB less to offer the next thing, VM or pod.
        let v = vmi(json!({"domain": {"memory": {"guest": "8Gi"}}}));
        let shim = scheduling_shim(&v);
        let node = json!({
            "metadata": {"name": "n1"},
            "status": {
                "conditions": [{"type": "Ready", "status": "True"}],
                "allocatable": {"cpu": "8", "memory": "16Gi"}
            }
        });
        let state = crate::scheduler::ClusterState::default();
        let nodes = std::slice::from_ref(&node);
        // Empty node: it fits.
        assert!(matches!(
            crate::filter::run_filters(&shim, &node, crate::filter::NodeUsage::default(), &state, nodes),
            crate::filter::FilterResult::Pass
        ));
        // With 12 GiB already promised, it does not.
        let used = crate::filter::NodeUsage { cpu_milli: 0, mem_bytes: 12 * 1024 * 1024 * 1024 };
        assert!(matches!(
            crate::filter::run_filters(&shim, &node, used, &state, nodes),
            crate::filter::FilterResult::Fail(_)
        ));
    }

    #[test]
    fn a_taint_the_vm_does_not_tolerate_rules_the_node_out() {
        // The point of the shim: the filters written for pods apply to a VM
        // the day they are written, rather than being reimplemented here.
        let plain = scheduling_shim(&vmi(json!({"domain": {"memory": {"guest": "1Gi"}}})));
        let tolerant = scheduling_shim(&vmi(json!({
            "domain": {"memory": {"guest": "1Gi"}},
            "tolerations": [{"key": "vm-only", "operator": "Exists"}]
        })));
        let node = json!({
            "metadata": {"name": "n1"},
            "spec": {"taints": [{"key": "vm-only", "effect": "NoSchedule"}]},
            "status": {
                "conditions": [{"type": "Ready", "status": "True"}],
                "allocatable": {"cpu": "8", "memory": "16Gi"}
            }
        });
        let state = crate::scheduler::ClusterState::default();
        let nodes = std::slice::from_ref(&node);
        let u = crate::filter::NodeUsage::default();
        assert!(matches!(
            crate::filter::run_filters(&plain, &node, u, &state, nodes),
            crate::filter::FilterResult::Fail(_)
        ));
        assert!(matches!(
            crate::filter::run_filters(&tolerant, &node, u, &state, nodes),
            crate::filter::FilterResult::Pass
        ));
    }

    #[test]
    fn a_node_selector_the_vm_states_is_honoured() {
        let shim = scheduling_shim(&vmi(json!({
            "domain": {"memory": {"guest": "1Gi"}},
            "nodeSelector": {"disk": "nvme"}
        })));
        let mk = |labels: Value| json!({
            "metadata": {"name": "n1", "labels": labels},
            "status": {
                "conditions": [{"type": "Ready", "status": "True"}],
                "allocatable": {"cpu": "8", "memory": "16Gi"}
            }
        });
        let state = crate::scheduler::ClusterState::default();
        let u = crate::filter::NodeUsage::default();
        let spinning = mk(json!({"disk": "hdd"}));
        assert!(matches!(
            crate::filter::run_filters(&shim, &spinning, u, &state, std::slice::from_ref(&spinning)),
            crate::filter::FilterResult::Fail(_)
        ));
        let fast = mk(json!({"disk": "nvme"}));
        assert!(matches!(
            crate::filter::run_filters(&shim, &fast, u, &state, std::slice::from_ref(&fast)),
            crate::filter::FilterResult::Pass
        ));
    }

    #[test]
    fn the_shim_omits_what_the_vmi_did_not_set() {
        // `null` and "not present" are the same to the filters; an empty
        // object is not, and a shim that invented an empty nodeSelector
        // would be asking the filters a question the VM never asked.
        let shim = scheduling_shim(&vmi(json!({"domain": {"memory": {"guest": "1Gi"}}})));
        for absent in ["nodeSelector", "affinity", "tolerations"] {
            assert!(shim["spec"][absent].is_null(), "{absent} should be absent");
        }
    }

    #[test]
    fn the_shim_carries_labels_because_affinity_and_spread_match_on_them() {
        let mut v = vmi(json!({"domain": {"memory": {"guest": "1Gi"}}}));
        v["metadata"]["labels"] = json!({"app": "db"});
        let shim = scheduling_shim(&v);
        assert_eq!(shim["metadata"]["labels"]["app"], "db");
        assert_eq!(shim["metadata"]["namespace"], "default");
    }

    #[test]
    fn the_status_path_is_the_subresource_not_the_object() {
        // A pod is bound by writing spec.nodeName; a VMI is placed by
        // patching its status. Getting it backwards produces a VM the
        // kubelet ignores and a spec the user did not write.
        assert_eq!(
            status_path("default", "web"),
            "/apis/kubevirt.io/v1/namespaces/default/virtualmachineinstances/web/status"
        );
    }
}
