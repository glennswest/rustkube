//! The pods the suites make run this same image, as `/test idle`: the test
//! image is already in the node's registry (stormcentral put it there to
//! start the Job), so nothing is pulled from outside the cluster, which the
//! standard forbids.

use crate::kube::Kube;
use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::sync::OnceLock;
use std::time::Duration;

/// Whether the run's namespace is governed by SecurityContextConstraints
/// (rustkube's `restricted-v2`, #321): this pod was admitted with an
/// `openshift.io/scc` annotation. Recorded by [`own_image`], which every
/// suite calls before it builds a template.
static SCC: OnceLock<bool> = OnceLock::new();

/// This pod's own image, as its spec names it. `RUSTKUBE_TEST_IMAGE`
/// overrides, for a run by hand.
///
/// The pod's name is the container's hostname. If that pod cannot be found
/// the Job's pod is looked up by the labels stormcentral puts on it.
pub async fn own_image(kube: &Kube) -> Result<String> {
    if let Ok(i) = std::env::var("RUSTKUBE_TEST_IMAGE") {
        if !i.is_empty() {
            return Ok(i);
        }
    }
    let pods = kube.path("", "v1", "pods");
    if let Ok(h) = std::fs::read_to_string("/proc/sys/kernel/hostname") {
        if let Some(p) = kube.get(&format!("{pods}/{}", h.trim())).await? {
            let _ = SCC.set(scc_admitted(&p));
            if let Some(i) = image_of(&p) {
                return Ok(i);
            }
        }
    }
    let list = kube
        .get(&format!("{pods}?labelSelector=storm.io%2Fcomponent%3Drustkube"))
        .await?
        .context("listing the run's pods")?;
    let items = list["items"].as_array().cloned().unwrap_or_default();
    if let Some(p) = items.iter().find(|p| image_of(p).is_some()) {
        let _ = SCC.set(scc_admitted(p));
    }
    items
        .iter()
        .find_map(image_of)
        .ok_or_else(|| anyhow!("cannot find this pod's own image (set RUSTKUBE_TEST_IMAGE)"))
}

fn scc_admitted(pod: &Value) -> bool {
    pod["metadata"]["annotations"]["openshift.io/scc"].as_str().is_some_and(|s| !s.is_empty())
}

fn image_of(pod: &Value) -> Option<String> {
    pod["spec"]["containers"][0]["image"].as_str().filter(|s| !s.is_empty()).map(str::to_string)
}

/// A pod template running `/test idle`: unprivileged, small, quick to stop,
/// with no API credentials (it needs none).
///
/// Under SCC the pod leaves `runAsUser` unset and `restricted-v2` gives it
/// the namespace's UID (a pinned UID outside the range is refused, as on
/// OpenShift; the first live `short` run, 034b3dd344, hit exactly that).
/// Elsewhere it pins 65532, since the image's own user is root.
pub fn template(run: &str, image: &str, app: &str) -> Value {
    template_for(run, image, app, SCC.get().copied().unwrap_or(false))
}

fn template_for(run: &str, image: &str, app: &str, scc: bool) -> Value {
    let pod_sc = if scc { json!({"runAsNonRoot": true}) } else { json!({"runAsNonRoot": true, "runAsUser": 65532}) };
    json!({
        "metadata": {"labels": {"storm.io/test-run": run, "app": app}},
        "spec": {
            "terminationGracePeriodSeconds": 2,
            "automountServiceAccountToken": false,
            "securityContext": pod_sc,
            "containers": [{
                "name": "idle",
                "image": image,
                "imagePullPolicy": "IfNotPresent",
                "command": ["/test"],
                "args": ["idle"],
                "resources": {"requests": {"cpu": "5m", "memory": "8Mi"}, "limits": {"memory": "32Mi"}},
                "securityContext": {"allowPrivilegeEscalation": false, "capabilities": {"drop": ["ALL"]}},
            }],
        },
    })
}

/// `/test idle [seconds]`: do nothing until stopped, or for `seconds`
/// (default an hour, so a pod left behind by a killed run ends by itself).
/// `0` exits at once with 0 — what a Job's pod runs to complete.
pub async fn idle(secs: Option<u64>) -> i32 {
    let secs = secs.unwrap_or(3600);
    if secs == 0 {
        return 0;
    }
    let mut term = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(s) => s,
        Err(_) => {
            tokio::time::sleep(Duration::from_secs(secs)).await;
            return 0;
        }
    };
    tokio::select! {
        _ = term.recv() => 0,
        _ = tokio::signal::ctrl_c() => 0,
        _ = tokio::time::sleep(Duration::from_secs(secs)) => 0,
    }
}

/// Pods in the run's namespace carrying `app=<app>`.
pub async fn pods_of(kube: &Kube, app: &str) -> Result<Vec<Value>> {
    let l = kube
        .get(&format!("{}?labelSelector=app%3D{app}", kube.path("", "v1", "pods")))
        .await?
        .unwrap_or(Value::Null);
    Ok(l["items"].as_array().cloned().unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_template_runs_this_image_idle_and_unprivileged() {
        let t = template_for("r1", "test-rustkube-short:abc", "web", false);
        let c = &t["spec"]["containers"][0];
        assert_eq!(c["image"], "test-rustkube-short:abc");
        assert_eq!(c["command"][0], "/test");
        assert_eq!(c["args"][0], "idle");
        assert_eq!(t["spec"]["securityContext"]["runAsNonRoot"], true);
        assert_eq!(t["spec"]["securityContext"]["runAsUser"], 65532);
        assert_eq!(t["metadata"]["labels"]["storm.io/test-run"], "r1");
    }

    #[test]
    fn under_scc_the_uid_is_left_to_the_namespace() {
        let t = template_for("r1", "i", "web", true);
        assert_eq!(t["spec"]["securityContext"]["runAsNonRoot"], true);
        assert!(t["spec"]["securityContext"].get("runAsUser").is_none());
        assert!(t["spec"]["containers"][0]["securityContext"].get("runAsUser").is_none());
    }

    #[test]
    fn scc_admission_is_read_from_the_pod() {
        assert!(scc_admitted(&json!({"metadata": {"annotations": {"openshift.io/scc": "restricted-v2"}}})));
        assert!(!scc_admitted(&json!({"metadata": {"annotations": {}}})));
        assert!(!scc_admitted(&json!({"metadata": {}})));
    }
}
