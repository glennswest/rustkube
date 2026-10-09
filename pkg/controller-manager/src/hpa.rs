//! Horizontal Pod Autoscaler (HPA) controller (#89): upstream's algorithm on
//! CPU and memory from `metrics.k8s.io`, which the apiserver serves from each
//! node's cadvisor (owner's decision on #89).
//!
//! Every 15 s (upstream's `--horizontal-pod-autoscaler-sync-period`), and
//! whenever its target changes, an HPA is evaluated as upstream's
//! `ReplicaCalculator` does:
//!
//! - each `Resource` metric (`cpu`/`memory`; `Utilization` of the Pods'
//!   requests or `AverageValue`; none listed means cpu at 80 %) gives a
//!   replica count from the usage ratio, within a 0.1 tolerance of no change;
//!   a Pod without metrics counts as 100 % (or the target, if higher) when
//!   scaling down and 0 when scaling up, and an unready Pod as 0 when scaling
//!   up — so a missing number never pushes the count the way it was going;
//! - the highest count wins; then `spec.behavior` (or upstream's default:
//!   up by the larger of 100 % or 4 Pods per 15 s, at once; down by up to
//!   100 % per 15 s, but only to the highest count recommended in the last
//!   300 s) and `minReplicas`/`maxReplicas` bound it;
//! - the target's `spec.replicas` is written directly: Deployments,
//!   ReplicaSets and StatefulSets only, not through `/scale` (#258).
//!
//! Status carries `currentMetrics` and upstream's conditions: `AbleToScale`,
//! `ScalingActive` (`False`/`FailedGetResourceMetric` when there are no
//! metrics — then nothing is changed, the inert behaviour this controller had
//! before) and `ScalingLimited`. Other metric types (`Pods`, `Object`,
//! `External`, `ContainerResource`) are not evaluated and say so.
//!
//! Pod metrics need cadvisor to know which Pod a cgroup is: stormpump's are
//! unlabelled until cadvisor#3, so on stormcos an HPA reads `ScalingActive=False`
//! until then.

use crate::owned::{self, Controller, Dependency, Deps};
use crate::runner::ApiClient;
use apimachinery::informer::Index;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Upstream's sync period.
const SYNC: Duration = Duration::from_secs(15);
/// Upstream's `--horizontal-pod-autoscaler-tolerance`.
const TOLERANCE: f64 = 0.1;

pub struct HpaController {
    api: Arc<ApiClient>,
    /// Per HPA (`ns/name`): recent recommendations and scale events, kept in
    /// memory as upstream keeps them.
    history: Mutex<HashMap<String, History>>,
}

#[derive(Default, Clone)]
pub(crate) struct History {
    recommendations: Vec<(Instant, u32)>,
    /// Replicas added (+) or removed (−), when.
    events: Vec<(Instant, i64)>,
}

/// CPU in cores from a quantity: `250m`, `0.5`, `500000000n`, `2`.
pub(crate) fn cpu_cores(s: &str) -> f64 {
    let s = s.trim();
    let (num, mult) = match s.chars().last() {
        Some('n') => (&s[..s.len() - 1], 1e-9),
        Some('u') => (&s[..s.len() - 1], 1e-6),
        Some('m') => (&s[..s.len() - 1], 1e-3),
        _ => (s, 1.0),
    };
    num.parse::<f64>().map(|v| v * mult).unwrap_or(0.0)
}

fn quantity(resource: &str, s: &str) -> f64 {
    if resource == "cpu" {
        cpu_cores(s)
    } else {
        apimachinery::quantity::parse_bytes(s) as f64
    }
}

/// What a Resource metric asks for.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Target {
    /// Percent of the Pods' requests.
    Utilization(f64),
    /// Per Pod, in cores (cpu) or bytes.
    AverageValue(f64),
}

/// One Pod as the calculator needs it.
#[derive(Clone, Debug)]
pub(crate) struct PodState {
    pub name: String,
    /// Phase Pending: counted, never measured.
    pub pending: bool,
    pub ready: bool,
    /// The sum of its containers' requests for the resource; None if any
    /// container has none.
    pub request: Option<f64>,
}

/// What a metric resolved to.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Computed {
    pub replicas: u32,
    /// Utilization in percent (Utilization targets), and the average per
    /// measured Pod.
    pub utilization: Option<i64>,
    pub average: f64,
}

/// Upstream's `GetResourceReplicas` / raw-value replicas for one metric.
pub(crate) fn calculate(
    current: u32,
    resource: &str,
    target: &Target,
    pods: &[PodState],
    usage: &HashMap<String, f64>,
) -> Result<Computed, String> {
    if pods.is_empty() {
        return Err("no pods match the target's selector".into());
    }
    // CPU of an unready Pod is not yet representative (upstream's
    // initialization period); memory is.
    let mut metrics: HashMap<&str, f64> = HashMap::new();
    let (mut ready, mut unready, mut missing) = (0usize, Vec::new(), Vec::new());
    for p in pods {
        if p.pending || (resource == "cpu" && !p.ready) {
            unready.push(p);
            continue;
        }
        match usage.get(&p.name) {
            Some(v) => {
                ready += 1;
                metrics.insert(&p.name, *v);
            }
            None => missing.push(p),
        }
    }
    if metrics.is_empty() {
        return Err(format!("did not receive metrics for targeted pods (pods might be unready): no {resource} usage for any"));
    }
    let request = |name: &str| pods.iter().find(|p| p.name == name).and_then(|p| p.request);
    if matches!(target, Target::Utilization(_)) {
        if let Some(p) = pods.iter().find(|p| p.request.is_none()) {
            return Err(format!("missing request for {resource} in pod {}", p.name));
        }
    }
    // The usage ratio over the measured Pods, and what that is in percent
    // or per Pod.
    let ratio = |m: &HashMap<&str, f64>| -> (f64, Option<i64>, f64) {
        let sum: f64 = m.values().sum();
        let avg = sum / m.len() as f64;
        match target {
            Target::Utilization(t) => {
                let req: f64 = m.keys().filter_map(|n| request(n)).sum();
                let util = if req > 0.0 { sum * 100.0 / req } else { 0.0 };
                (util / t, Some(util.round() as i64), avg)
            }
            Target::AverageValue(t) => (avg / t, None, avg),
        }
    };
    let (usage_ratio, utilization, average) = ratio(&metrics);
    let scale_up_with_unready = !unready.is_empty() && usage_ratio > 1.0;
    if !scale_up_with_unready && missing.is_empty() {
        let replicas = if (1.0 - usage_ratio).abs() <= TOLERANCE {
            current
        } else {
            (usage_ratio * ready as f64).ceil() as u32
        };
        return Ok(Computed { replicas, utilization, average });
    }
    // Fill the gaps the way that holds the count back.
    if !missing.is_empty() {
        for p in &missing {
            let v = if usage_ratio < 1.0 {
                match target {
                    Target::Utilization(t) => p.request.unwrap_or(0.0) * t.max(100.0) / 100.0,
                    Target::AverageValue(t) => *t,
                }
            } else {
                0.0
            };
            metrics.insert(&p.name, v);
        }
    }
    if scale_up_with_unready {
        for p in &unready {
            metrics.insert(&p.name, 0.0);
        }
    }
    let (new_ratio, _, _) = ratio(&metrics);
    if (1.0 - new_ratio).abs() <= TOLERANCE
        || (usage_ratio < 1.0 && new_ratio > 1.0)
        || (usage_ratio > 1.0 && new_ratio < 1.0)
    {
        return Ok(Computed { replicas: current, utilization, average });
    }
    let replicas = (new_ratio * metrics.len() as f64).ceil() as u32;
    let replicas = if (new_ratio < 1.0 && replicas > current) || (new_ratio > 1.0 && replicas < current) {
        current
    } else {
        replicas
    };
    Ok(Computed { replicas, utilization, average })
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Select {
    Max,
    Min,
    Disabled,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Rules {
    pub window: u64,
    pub select: Select,
    /// (Pods or Percent, value, period seconds)
    pub policies: Vec<(bool, i64, u64)>,
}

/// `spec.behavior.scaleUp`/`scaleDown`, with upstream's defaults for what
/// is not set.
pub(crate) fn rules(hpa: &Value, up: bool) -> Rules {
    let b = &hpa["spec"]["behavior"][if up { "scaleUp" } else { "scaleDown" }];
    let window = b["stabilizationWindowSeconds"].as_u64().unwrap_or(if up { 0 } else { 300 });
    let select = match b["selectPolicy"].as_str() {
        Some("Min") => Select::Min,
        Some("Disabled") => Select::Disabled,
        _ => Select::Max,
    };
    let policies: Vec<(bool, i64, u64)> = b["policies"]
        .as_array()
        .map(|ps| {
            ps.iter()
                .filter_map(|p| Some((p["type"].as_str()? == "Pods", p["value"].as_i64()?, p["periodSeconds"].as_u64()?)))
                .collect()
        })
        .filter(|v: &Vec<_>| !v.is_empty())
        .unwrap_or_else(|| if up { vec![(false, 100, 15), (true, 4, 15)] } else { vec![(false, 100, 15)] });
    Rules { window, select, policies }
}

/// Upstream's stabilization: the count is moved up only as far as the
/// lowest recommendation in the scale-up window, and down only as far as
/// the highest in the scale-down window. Records `desired`.
pub(crate) fn stabilize(h: &mut History, now: Instant, desired: u32, current: u32, up: &Rules, down: &Rules) -> u32 {
    let longest = up.window.max(down.window);
    h.recommendations.retain(|(t, _)| now.duration_since(*t).as_secs() <= longest);
    let (mut up_rec, mut down_rec) = (desired, desired);
    for (t, r) in &h.recommendations {
        let age = now.duration_since(*t).as_secs();
        if age <= up.window {
            up_rec = up_rec.min(*r);
        }
        if age <= down.window {
            down_rec = down_rec.max(*r);
        }
    }
    h.recommendations.push((now, desired));
    current.max(up_rec).min(down_rec)
}

fn changed_in(h: &History, now: Instant, period: u64) -> (i64, i64) {
    h.events
        .iter()
        .filter(|(t, _)| now.duration_since(*t).as_secs() <= period)
        .fold((0, 0), |(up, down), (_, d)| if *d > 0 { (up + d, down) } else { (up, down - d) })
}

/// Upstream's `convertDesiredReplicasWithBehaviorRate`: the rate policies,
/// then min/max. Returns the count and why it is limited, if it is.
pub(crate) fn limit(
    h: &History,
    now: Instant,
    desired: u32,
    current: u32,
    min: u32,
    max: u32,
    up: &Rules,
    down: &Rules,
) -> (u32, Option<(&'static str, String)>) {
    let start = |period: u64| {
        let (added, removed) = changed_in(h, now, period);
        current as i64 - added + removed
    };
    if desired > current {
        let limit = match up.select {
            Select::Disabled => current as i64,
            _ => {
                let proposals = up.policies.iter().map(|(pods, v, p)| {
                    let s = start(*p);
                    if *pods { s + v } else { (s as f64 * (1.0 + *v as f64 / 100.0)).ceil() as i64 }
                });
                if up.select == Select::Min { proposals.min() } else { proposals.max() }.unwrap_or(current as i64)
            }
        }
        .max(current as i64) as u32;
        if desired > max.min(limit) {
            return if limit < max {
                (limit, Some(("ScaleUpLimit", "the desired replica count is increasing faster than the maximum scale rate".into())))
            } else {
                (max, Some(("TooManyReplicas", "the desired replica count is more than the maximum replica count".into())))
            };
        }
        (desired, None)
    } else if desired < current {
        let limit = match down.select {
            Select::Disabled => current as i64,
            _ => {
                let proposals = down.policies.iter().map(|(pods, v, p)| {
                    let s = start(*p);
                    if *pods { s - v } else { (s as f64 * (1.0 - *v as f64 / 100.0)) as i64 }
                });
                // Max picks the policy allowing the largest change: the lowest.
                if down.select == Select::Min { proposals.max() } else { proposals.min() }.unwrap_or(current as i64)
            }
        }
        .min(current as i64)
        .max(0) as u32;
        if desired < min.max(limit) {
            return if limit > min {
                (limit, Some(("ScaleDownLimit", "the desired replica count is decreasing faster than the maximum scale rate".into())))
            } else {
                (min, Some(("TooFewReplicas", "the desired replica count is less than the minimum replica count".into())))
            };
        }
        (desired, None)
    } else if desired > max {
        (max, Some(("TooManyReplicas", "the desired replica count is more than the maximum replica count".into())))
    } else if desired < min {
        (min, Some(("TooFewReplicas", "the desired replica count is less than the minimum replica count".into())))
    } else {
        (desired, None)
    }
}

fn condition(kind: &str, status: bool, reason: &str, message: &str, now: &str) -> Value {
    json!({"type": kind, "status": if status { "True" } else { "False" }, "reason": reason,
           "message": message, "lastTransitionTime": now})
}

/// The metrics an HPA asks for: `spec.metrics`, or upstream's default.
fn metric_specs(hpa: &Value) -> Vec<Value> {
    match hpa["spec"]["metrics"].as_array() {
        Some(m) if !m.is_empty() => m.clone(),
        _ => vec![json!({"type": "Resource", "resource": {"name": "cpu", "target": {"type": "Utilization", "averageUtilization": 80}}})],
    }
}

fn pod_states(pods: &[Value], resource: &str) -> Vec<PodState> {
    pods.iter()
        .map(|p| {
            let containers = p["spec"]["containers"].as_array().cloned().unwrap_or_default();
            let request = containers
                .iter()
                .map(|c| c["resources"]["requests"][resource].as_str().map(|q| quantity(resource, q)))
                .sum::<Option<f64>>()
                .filter(|_| !containers.is_empty());
            PodState {
                name: p["metadata"]["name"].as_str().unwrap_or("").to_string(),
                pending: p["status"]["phase"].as_str().unwrap_or("Pending") == "Pending",
                ready: p["status"]["conditions"]
                    .as_array()
                    .is_some_and(|c| c.iter().any(|c| c["type"] == "Ready" && c["status"] == "True")),
                request,
            }
        })
        .collect()
}

/// Pod name → summed container usage of `resource`, from a PodMetricsList.
fn usage_by_pod(list: &Value, resource: &str) -> HashMap<String, f64> {
    list["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| {
            let name = m["metadata"]["name"].as_str()?.to_string();
            let sum = m["containers"]
                .as_array()?
                .iter()
                .map(|c| c["usage"][resource].as_str().map(|q| quantity(resource, q)).unwrap_or(0.0))
                .sum();
            Some((name, sum))
        })
        .collect()
}

impl HpaController {
    pub fn new(api: Arc<ApiClient>) -> Self {
        Self { api, history: Mutex::new(HashMap::new()) }
    }

    pub async fn run(&self) {
        owned::run(&self.api, self).await;
    }

    async fn pod_metrics(&self, namespace: &str) -> Result<Value, String> {
        let url = format!("{}/apis/metrics.k8s.io/v1beta1/namespaces/{namespace}/pods", self.api.base_url);
        let resp = self.api.client.get(url).timeout(Duration::from_secs(10)).send().await.map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("metrics.k8s.io answered {}", resp.status()));
        }
        resp.json().await.map_err(|e| e.to_string())
    }

    async fn reconcile_hpa(&self, namespace: &str, hpa: &Value, deps: &Deps) -> anyhow::Result<()> {
        apimachinery::reactor::requeue_after(SYNC);
        let hpa_name = hpa["metadata"]["name"].as_str().ok_or_else(|| anyhow::anyhow!("HPA missing name"))?;
        let now_s = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let target_ref = &hpa["spec"]["scaleTargetRef"];
        let (feed, api_path, resource) = match target_ref["kind"].as_str().unwrap_or("") {
            "Deployment" => (0, "apis/apps/v1", "deployments"),
            "ReplicaSet" => (1, "apis/apps/v1", "replicasets"),
            "StatefulSet" => (2, "apis/apps/v1", "statefulsets"),
            _ => (usize::MAX, "", ""),
        };
        let target_name = target_ref["name"].as_str().unwrap_or("");
        let target = if feed == usize::MAX {
            None
        } else {
            deps.feed(feed).select(&Index::Name(namespace.into(), target_name.into()))?.into_iter().next()
        };
        let min = hpa["spec"]["minReplicas"].as_u64().unwrap_or(1) as u32;
        let max = hpa["spec"]["maxReplicas"].as_u64().unwrap_or(10) as u32;

        let mut status = json!({"currentMetrics": [], "conditions": []});
        if !hpa["status"]["lastScaleTime"].is_null() {
            status["lastScaleTime"] = hpa["status"]["lastScaleTime"].clone();
        }
        let Some(target) = target else {
            let current = hpa["status"]["currentReplicas"].as_u64().unwrap_or(0);
            status["currentReplicas"] = json!(current);
            status["desiredReplicas"] = json!(current);
            status["conditions"] = json!([condition("AbleToScale", false, "FailedGetScale",
                &format!("the HPA controller was unable to get the target's current scale: {} {target_name:?} not found or not supported",
                         target_ref["kind"].as_str().unwrap_or("")), &now_s)]);
            return self.write_status(namespace, hpa_name, hpa, status).await;
        };
        let current = target["spec"]["replicas"].as_u64().unwrap_or(1) as u32;
        status["currentReplicas"] = json!(current);
        status["desiredReplicas"] = json!(current);
        let able = condition("AbleToScale", true, "SucceededGetScale", "the HPA controller was able to get the target's current scale", &now_s);
        if current == 0 {
            status["conditions"] = json!([able, condition("ScalingActive", false, "ScalingDisabled",
                "scaling is disabled since the replica count of the target is zero", &now_s)]);
            return self.write_status(namespace, hpa_name, hpa, status).await;
        }

        let selector = &target["spec"]["selector"];
        let pods: Vec<Value> = deps
            .feed(3)
            .select(&Index::Namespace(namespace.into()))?
            .into_iter()
            .filter(|p| {
                apimachinery::selector::matches(selector, &p["metadata"]["labels"])
                    && p["metadata"]["deletionTimestamp"].is_null()
                    && p["status"]["phase"] != "Failed"
                    && p["status"]["phase"] != "Succeeded"
            })
            .collect();
        let metrics = self.pod_metrics(namespace).await;

        let mut desired: Option<u32> = None;
        let mut current_metrics = Vec::new();
        let mut errors = Vec::new();
        for spec in metric_specs(hpa) {
            if spec["type"] != "Resource" {
                errors.push(format!("metric type {} is not supported (rustkube#89)", spec["type"]));
                continue;
            }
            let res = spec["resource"]["name"].as_str().unwrap_or("cpu");
            let t = &spec["resource"]["target"];
            let target_kind = match t["type"].as_str() {
                Some("AverageValue") => Target::AverageValue(quantity(res, t["averageValue"].as_str().unwrap_or("0"))),
                _ => Target::Utilization(t["averageUtilization"].as_f64().unwrap_or(80.0)),
            };
            let usage = match &metrics {
                Ok(list) => usage_by_pod(list, res),
                Err(e) => {
                    errors.push(format!("failed to get {res} utilization: unable to get metrics for resource {res}: {e}"));
                    continue;
                }
            };
            match calculate(current, res, &target_kind, &pod_states(&pods, res), &usage) {
                Ok(c) => {
                    desired = Some(desired.map_or(c.replicas, |d| d.max(c.replicas)));
                    let mut cur = json!({"averageValue": if res == "cpu" { format!("{}m", (c.average * 1000.0).round() as i64) }
                                                         else { format!("{}", c.average.round() as i64) }});
                    if let Some(u) = c.utilization {
                        cur["averageUtilization"] = json!(u);
                    }
                    current_metrics.push(json!({"type": "Resource", "resource": {"name": res, "current": cur}}));
                }
                Err(e) => errors.push(format!("failed to get {res} utilization: {e}")),
            }
        }
        status["currentMetrics"] = json!(current_metrics);
        let Some(raw) = desired else {
            // Nothing measured: change nothing, and say why.
            status["conditions"] = json!([able, condition("ScalingActive", false, "FailedGetResourceMetric",
                &format!("the HPA was unable to compute the replica count: {}", errors.join("; ")), &now_s)]);
            return self.write_status(namespace, hpa_name, hpa, status).await;
        };

        let key = format!("{namespace}/{hpa_name}");
        let (up, down) = (rules(hpa, true), rules(hpa, false));
        let now = Instant::now();
        let (desired, limited) = {
            let mut all = self.history.lock().unwrap();
            let h = all.entry(key).or_default();
            let stabilized = stabilize(h, now, raw, current, &up, &down);
            limit(h, now, stabilized, current, min, max, &up, &down)
        };
        status["desiredReplicas"] = json!(desired);
        let rescaled;
        if desired != current {
            let mut updated = target.clone();
            updated["spec"]["replicas"] = json!(desired);
            self.api
                .update(&format!("/{api_path}/namespaces/{namespace}/{resource}/{}", target_name), &updated)
                .await?;
            tracing::info!("HPA {namespace}/{hpa_name}: {} {target_name} {current} → {desired}", target_ref["kind"].as_str().unwrap_or(""));
            self.history
                .lock()
                .unwrap()
                .entry(format!("{namespace}/{hpa_name}"))
                .or_default()
                .events
                .push((now, desired as i64 - current as i64));
            status["lastScaleTime"] = json!(now_s);
            rescaled = condition("AbleToScale", true, "SucceededRescale", &format!("the HPA controller was able to update the target scale to {desired}"), &now_s);
        } else {
            rescaled = condition("AbleToScale", true, "ReadyForNewScale", "recommended size matches current size", &now_s);
        }
        let resources: Vec<String> = current_metrics.iter().filter_map(|m| m["resource"]["name"].as_str().map(str::to_string)).collect();
        let active = condition("ScalingActive", true, "ValidMetricFound",
            &format!("the HPA was able to successfully calculate a replica count from {} resource utilization", resources.join(", ")), &now_s);
        let limited = match limited {
            Some((reason, message)) => condition("ScalingLimited", true, reason, &message, &now_s),
            None => condition("ScalingLimited", false, "DesiredWithinRange", "the desired count is within the acceptable range", &now_s),
        };
        status["conditions"] = json!([rescaled, active, limited]);
        self.write_status(namespace, hpa_name, hpa, status).await
    }

    async fn write_status(&self, namespace: &str, name: &str, hpa: &Value, mut status: Value) -> anyhow::Result<()> {
        owned::preserve_transition_times(&hpa["status"], &mut status);
        if hpa["status"] == status {
            return Ok(());
        }
        let mut updated = hpa.clone();
        updated["status"] = status;
        self.api
            .update_status(&format!("/apis/autoscaling/v2/namespaces/{namespace}/horizontalpodautoscalers/{name}"), &updated)
            .await?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl Controller for HpaController {
    fn name(&self) -> &'static str {
        "hpa"
    }
    fn primary(&self) -> &'static str {
        "/apis/autoscaling/v2/horizontalpodautoscalers"
    }
    fn dependencies(&self) -> Vec<Dependency> {
        let route: owned::Route = Arc::new(|delta, primary| {
            delta
                .old
                .iter()
                .chain(delta.new.iter())
                .flat_map(|o| {
                    owned::keys_at(
                        primary,
                        Index::Target(
                            o["metadata"]["namespace"].as_str().unwrap_or("").into(),
                            o["kind"].as_str().unwrap_or("").into(),
                            o["metadata"]["name"].as_str().unwrap_or("").into(),
                        ),
                    )
                })
                .collect()
        });
        vec![
            Dependency { path: "/apis/apps/v1/deployments".into(), route: route.clone() },
            Dependency { path: "/apis/apps/v1/replicasets".into(), route: route.clone() },
            Dependency { path: "/apis/apps/v1/statefulsets".into(), route },
            // The target's Pods, read by selector at each evaluation; their
            // changes need not wake an HPA (it resyncs every 15 s).
            Dependency { path: "/api/v1/pods".into(), route: Arc::new(|_, _| Vec::new()) },
        ]
    }
    async fn reconcile(&self, hpa: &Value, _: &[Value], deps: &Deps) -> anyhow::Result<()> {
        self.reconcile_hpa(hpa["metadata"]["namespace"].as_str().unwrap_or("default"), hpa, deps).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pod(name: &str, request: f64) -> PodState {
        PodState { name: name.into(), pending: false, ready: true, request: Some(request) }
    }
    fn usage(v: &[(&str, f64)]) -> HashMap<String, f64> {
        v.iter().map(|(n, u)| (n.to_string(), *u)).collect()
    }

    #[test]
    fn utilization_scales_by_the_ratio_and_holds_within_tolerance() {
        // 3 pods at 1 core request, 0.9 cores used each → 90 % vs 50 %:
        // ceil(1.8 × 3) = 6.
        let pods = vec![pod("a", 1.0), pod("b", 1.0), pod("c", 1.0)];
        let c = calculate(3, "cpu", &Target::Utilization(50.0), &pods, &usage(&[("a", 0.9), ("b", 0.9), ("c", 0.9)])).unwrap();
        assert_eq!((c.replicas, c.utilization), (6, Some(90)));
        // 52 % vs 50 %: within 0.1, no change.
        let c = calculate(3, "cpu", &Target::Utilization(50.0), &pods, &usage(&[("a", 0.52), ("b", 0.52), ("c", 0.52)])).unwrap();
        assert_eq!(c.replicas, 3);
        // 10 % vs 50 %: scale down to ceil(0.2 × 3) = 1.
        let c = calculate(3, "cpu", &Target::Utilization(50.0), &pods, &usage(&[("a", 0.1), ("b", 0.1), ("c", 0.1)])).unwrap();
        assert_eq!(c.replicas, 1);
    }

    #[test]
    fn missing_metrics_never_push_the_count_the_way_it_was_going() {
        let pods = vec![pod("a", 1.0), pod("b", 1.0), pod("c", 1.0), pod("d", 1.0)];
        // Scaling down on two measured pods, two missing counted at 100 %:
        // (0.1+0.1+1+1)/4 = 55 % vs 50 % — within tolerance, stays at 4.
        let c = calculate(4, "cpu", &Target::Utilization(50.0), &pods, &usage(&[("a", 0.1), ("b", 0.1)])).unwrap();
        assert_eq!(c.replicas, 4);
        // Scaling up with missing at 0: (1+1+0+0)/4 = 50 % — no change.
        let c = calculate(4, "cpu", &Target::Utilization(50.0), &pods, &usage(&[("a", 1.0), ("b", 1.0)])).unwrap();
        assert_eq!(c.replicas, 4);
    }

    #[test]
    fn unready_pods_count_as_zero_only_for_a_cpu_scale_up() {
        let mut pods = vec![pod("a", 1.0), pod("b", 1.0)];
        pods[1].ready = false;
        // a at 100 % vs 50 %, b unready at 0: (1+0)/2 = 50 % → no change.
        let c = calculate(2, "cpu", &Target::Utilization(50.0), &pods, &usage(&[("a", 1.0), ("b", 1.0)])).unwrap();
        assert_eq!(c.replicas, 2);
        // Memory counts the unready pod's usage.
        let c = calculate(2, "memory", &Target::Utilization(50.0), &pods, &usage(&[("a", 1.0), ("b", 1.0)])).unwrap();
        assert_eq!(c.replicas, 4);
    }

    #[test]
    fn average_value_and_the_errors_that_change_nothing() {
        let pods = vec![pod("a", 1.0), pod("b", 1.0)];
        let mib = 1048576.0;
        let c = calculate(2, "memory", &Target::AverageValue(100.0 * mib), &pods, &usage(&[("a", 300.0 * mib), ("b", 300.0 * mib)])).unwrap();
        assert_eq!(c.replicas, 6);
        assert!(calculate(2, "cpu", &Target::Utilization(50.0), &pods, &usage(&[])).is_err(), "no metrics");
        let no_req = vec![PodState { name: "a".into(), pending: false, ready: true, request: None }];
        assert!(calculate(1, "cpu", &Target::Utilization(50.0), &no_req, &usage(&[("a", 1.0)])).unwrap_err().contains("missing request"));
    }

    #[test]
    fn scale_down_waits_for_the_window_and_scale_up_is_rate_limited() {
        let hpa = json!({"spec": {}});
        let (up, down) = (rules(&hpa, true), rules(&hpa, false));
        assert_eq!((up.window, down.window), (0, 300));
        let mut h = History::default();
        let t0 = Instant::now();
        // A spike recommends 10, then the load drops: the highest
        // recommendation in 300 s holds the count.
        assert_eq!(stabilize(&mut h, t0, 10, 4, &up, &down), 10);
        assert_eq!(stabilize(&mut h, t0 + Duration::from_secs(60), 2, 10, &up, &down), 10);
        assert_eq!(stabilize(&mut h, t0 + Duration::from_secs(400), 2, 10, &up, &down), 2, "past the window");
        // From 2, up is bounded by max(100 %, 4 pods) per 15 s: 6.
        let (n, why) = limit(&History::default(), t0, 50, 2, 1, 100, &up, &down);
        assert_eq!((n, why.map(|w| w.0)), (6, Some("ScaleUpLimit")));
        // From 20, 100 %: max(40, 24) = 40; maxReplicas 30 caps it.
        let (n, why) = limit(&History::default(), t0, 50, 20, 1, 30, &up, &down);
        assert_eq!((n, why.map(|w| w.0)), (30, Some("TooManyReplicas")));
        // Down to below minReplicas.
        let (n, why) = limit(&History::default(), t0, 1, 4, 2, 10, &up, &down);
        assert_eq!((n, why.map(|w| w.0)), (2, Some("TooFewReplicas")));
        // A scale-up already made this period counts against it.
        let mut h = History::default();
        h.events.push((t0, 4));
        let (n, _) = limit(&h, t0 + Duration::from_secs(5), 50, 6, 1, 100, &up, &down);
        assert_eq!(n, 6, "period started at 2: max(4, 2+4) = 6");
    }

    #[test]
    fn behavior_overrides_and_disabled() {
        let hpa = json!({"spec": {"behavior": {"scaleDown": {"selectPolicy": "Disabled"},
            "scaleUp": {"stabilizationWindowSeconds": 60, "policies": [{"type": "Pods", "value": 1, "periodSeconds": 60}]}}}});
        let (up, down) = (rules(&hpa, true), rules(&hpa, false));
        assert_eq!(up.window, 60);
        assert_eq!(limit(&History::default(), Instant::now(), 10, 3, 1, 20, &up, &down).0, 4);
        assert_eq!(limit(&History::default(), Instant::now(), 1, 3, 1, 20, &up, &down).0, 3, "scale-down disabled");
    }

    #[test]
    fn quantities_and_pod_metrics_parse() {
        assert_eq!(cpu_cores("500000000n"), 0.5);
        assert_eq!(cpu_cores("250m"), 0.25);
        assert_eq!(cpu_cores("2"), 2.0);
        let list = json!({"items": [{"metadata": {"name": "a"}, "containers": [
            {"usage": {"cpu": "100000000n", "memory": "1024Ki"}}, {"usage": {"cpu": "50m", "memory": "1Mi"}}]}]});
        let u = usage_by_pod(&list, "cpu");
        assert!((u["a"] - 0.15).abs() < 1e-9);
        assert_eq!(usage_by_pod(&list, "memory")["a"], 2.0 * 1048576.0);
        let pods = pod_states(&[json!({"metadata": {"name": "p"}, "status": {"phase": "Running",
            "conditions": [{"type": "Ready", "status": "True"}]},
            "spec": {"containers": [{"resources": {"requests": {"cpu": "200m"}}}, {"resources": {"requests": {"cpu": "300m"}}}]}})], "cpu");
        assert_eq!(pods[0].request, Some(0.5));
        assert!(pods[0].ready && !pods[0].pending);
    }
}
