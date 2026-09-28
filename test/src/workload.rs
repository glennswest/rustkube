//! The pods the suites make run this same image, as `/test idle`: the test
//! image is already in the node's registry (stormcentral put it there to
//! start the Job), so nothing is pulled from outside the cluster, which the
//! standard forbids.

use crate::kube::Kube;
use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::time::Duration;

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
            if let Some(i) = image_of(&p) {
                return Ok(i);
            }
        }
    }
    let list = kube
        .get(&format!("{pods}?labelSelector=storm.io%2Fcomponent%3Drustkube"))
        .await?
        .context("listing the run's pods")?;
    list["items"]
        .as_array()
        .into_iter()
        .flatten()
        .find_map(image_of)
        .ok_or_else(|| anyhow!("cannot find this pod's own image (set RUSTKUBE_TEST_IMAGE)"))
}

fn image_of(pod: &Value) -> Option<String> {
    pod["spec"]["containers"][0]["image"].as_str().filter(|s| !s.is_empty()).map(str::to_string)
}

/// A pod template running `/test idle`: unprivileged, small, quick to stop,
/// with no API credentials (it needs none).
pub fn template(run: &str, image: &str, app: &str) -> Value {
    json!({
        "metadata": {"labels": {"storm.io/test-run": run, "app": app}},
        "spec": {
            "terminationGracePeriodSeconds": 2,
            "automountServiceAccountToken": false,
            "securityContext": {"runAsNonRoot": true, "runAsUser": 65532},
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

pub fn running(pod: &Value) -> bool {
    pod["status"]["phase"] == "Running"
        && pod["status"]["conditions"]
            .as_array()
            .is_some_and(|cs| cs.iter().any(|c| c["type"] == "Ready" && c["status"] == "True"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_template_runs_this_image_idle_and_unprivileged() {
        let t = template("r1", "test-rustkube-short:abc", "web");
        let c = &t["spec"]["containers"][0];
        assert_eq!(c["image"], "test-rustkube-short:abc");
        assert_eq!(c["command"][0], "/test");
        assert_eq!(c["args"][0], "idle");
        assert_eq!(t["spec"]["securityContext"]["runAsNonRoot"], true);
        assert_eq!(t["metadata"]["labels"]["storm.io/test-run"], "r1");
    }

    #[test]
    fn ready_means_running_and_ready() {
        let p = json!({"status": {"phase": "Running", "conditions": [{"type": "Ready", "status": "True"}]}});
        assert!(running(&p));
        assert!(!running(&json!({"status": {"phase": "Running"}})));
        assert!(!running(&json!({"status": {"phase": "Pending"}})));
    }
}
