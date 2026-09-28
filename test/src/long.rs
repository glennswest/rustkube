//! `long` (the night window): waves of the control plane's own workload,
//! per the standard's "Overnight soaks: waves". Each wave:
//!
//! 1. **ramps** ConfigMaps, sixteen writers at once, with four watches
//!    following them, and a Deployment of small pods;
//! 2. **holds and exercises**: every object updated once, everything listed
//!    in pages;
//! 3. **drains**: a `deletecollection`, the Deployment deleted, and checks
//!    nothing is left.
//!
//! and it repeats until the window ends, varying the size. Wave 1 is sized
//! to a fixed 200 objects; later waves are sized from wave 1's measured
//! write rate (about a minute of writes, at most 5000) and varied ×½, ×1,
//! ×1½ — so a faster control plane gets bigger waves, with nothing assumed.
//!
//! Pod waves at the machine's capacity are system stress and live in
//! stormcos_qa (owner, 2026-09-28); the pods here are a fixed ten, enough to
//! keep the controllers and the scheduler in the loop.
//!
//! One line per wave, with its measurements, and a final `trend` line. A
//! wave fails on residue, a failed update or a watch that missed events; the trend fails on a
//! slowdown (create p50 of the last three waves over twice wave 1's) or the
//! apiserver's resident memory growing by half after the first wave.

use crate::env::Env;
use crate::kube::{brief, Kube, JSON, STRATEGIC};
use crate::report::{Outcome, Report};
use crate::workload;
use anyhow::Result;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};

const WRITERS: usize = 16;
const WATCHES: usize = 4;
const PODS: u32 = 10;

#[derive(Debug, Clone, Default)]
struct Wave {
    n: usize,
    size: usize,
    create_p50_ms: f64,
    create_p99_ms: f64,
    writes_per_s: f64,
    update_errors: usize,
    list_ms: u128,
    watch_missed: usize,
    pods_ready_ms: Option<u128>,
    residue: usize,
    rss_mb: Option<f64>,
    secs: u64,
}

pub async fn run(env: &Env, kube: &Kube, rep: &mut Report) -> Result<()> {
    let image = workload::own_image(kube).await?;
    let kube = Arc::new(kube.clone());
    let mut waves: Vec<Wave> = Vec::new();
    let mut rate = 0.0;
    let mut n = 0;
    loop {
        n += 1;
        let size = if n == 1 { 200 } else { size_for(rate, n) };
        // Room for the wave: the last one's time and half again, at least
        // three minutes.
        let need = Duration::from_secs(waves.last().map(|w| w.secs * 3 / 2).unwrap_or(0).max(180));
        if env.left() < need {
            break;
        }
        let t = Instant::now();
        let w = wave(env, &kube, &image, n, size).await?;
        if n == 1 {
            rate = w.writes_per_s;
        }
        let bad = w.residue > 0 || w.watch_missed > 0 || w.update_errors > 0;
        let detail = format!(
            "{} objects: create p50 {:.0} ms p99 {:.0} ms, {:.0}/s; updates failed {}; list {} ms; watch events missed {}; pods ready {}; residue {}; apiserver rss {}",
            w.size,
            w.create_p50_ms,
            w.create_p99_ms,
            w.writes_per_s,
            w.update_errors,
            w.list_ms,
            w.watch_missed,
            w.pods_ready_ms.map(|m| format!("{m} ms")).unwrap_or_else(|| "never".into()),
            w.residue,
            w.rss_mb.map(|m| format!("{m:.0} MB")).unwrap_or_else(|| "unread".into()),
        );
        let extra = json!({"wave": n, "size": w.size, "create_p50_ms": w.create_p50_ms, "create_p99_ms": w.create_p99_ms,
            "writes_per_s": w.writes_per_s, "update_errors": w.update_errors, "list_ms": w.list_ms as u64, "watch_missed": w.watch_missed,
            "pods_ready_ms": w.pods_ready_ms.map(|m| m as u64), "residue": w.residue, "rss_mb": w.rss_mb});
        rep.line(&format!("wave-{n}"), if bad { "fail" } else { "pass" }, t.elapsed().as_millis(), &detail, Some(extra));
        waves.push(Wave { secs: t.elapsed().as_secs(), ..w });
    }
    if waves.is_empty() {
        return Err(anyhow::anyhow!("no time for a single wave ({:?} left)", env.left()));
    }
    rep.check("trend", async { Ok(trend(&waves)) }).await
}

/// About a minute of writes at wave 1's rate, at most 5000, varied by the
/// wave number.
fn size_for(rate: f64, n: usize) -> usize {
    let base = (rate * 60.0).clamp(100.0, 5000.0);
    let f = [1.0, 0.5, 1.5][n % 3];
    ((base * f) as usize).clamp(100, 5000)
}

fn trend(waves: &[Wave]) -> Outcome {
    let first = &waves[0];
    let tail: Vec<&Wave> = waves.iter().rev().take(3).collect();
    let tail_p50 = tail.iter().map(|w| w.create_p50_ms).sum::<f64>() / tail.len() as f64;
    let mut problems = Vec::new();
    if waves.len() > 1 && tail_p50 > 2.0 * first.create_p50_ms.max(1.0) {
        let slow = waves.iter().find(|w| w.create_p50_ms > 2.0 * first.create_p50_ms.max(1.0)).map(|w| w.n).unwrap_or(0);
        problems.push(format!("create p50 {tail_p50:.0} ms over the last waves vs {:.0} ms in wave 1 (first slow wave: {slow})", first.create_p50_ms));
    }
    // Memory: against wave 2, once caches have warmed.
    if let (Some(base), Some(last)) = (waves.get(1).and_then(|w| w.rss_mb), waves.last().and_then(|w| w.rss_mb)) {
        if waves.len() > 3 && last > base * 1.5 {
            problems.push(format!("apiserver rss {last:.0} MB vs {base:.0} MB after wave 2"));
        }
    }
    let residue: usize = waves.iter().map(|w| w.residue).sum();
    if residue > 0 {
        problems.push(format!("{residue} objects left behind across waves"));
    }
    let summary = format!("{} waves, create p50 {:.0} → {:.0} ms", waves.len(), first.create_p50_ms, waves.last().map(|w| w.create_p50_ms).unwrap_or(0.0));
    if problems.is_empty() {
        Outcome::Pass(summary)
    } else {
        Outcome::Fail(format!("{summary}; {}", problems.join("; ")))
    }
}

fn pct(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

async fn wave(env: &Env, kube: &Arc<Kube>, image: &str, n: usize, size: usize) -> Result<Wave> {
    let cms = kube.path("", "v1", "configmaps");
    let sel = format!("labelSelector=wave%3D{n}");
    let from = kube.get(&cms).await?.unwrap_or(Value::Null)["metadata"]["resourceVersion"].as_str().unwrap_or("0").to_string();

    // Watches first, from the list's revision, so they must see every create.
    let mut watchers = Vec::new();
    for _ in 0..WATCHES {
        let mut w = kube.watch(&format!("{cms}?watch=1&resourceVersion={from}&{sel}&timeoutSeconds=1800")).await?;
        let want = size;
        watchers.push(tokio::spawn(async move {
            let mut seen = std::collections::HashSet::new();
            let end = Instant::now() + Duration::from_secs(1800);
            while seen.len() < want && Instant::now() < end {
                match w.next(Duration::from_secs(60)).await {
                    Ok(Some(ev)) if ev["type"] == "ADDED" => {
                        seen.insert(ev["object"]["metadata"]["name"].as_str().unwrap_or("").to_string());
                    }
                    Ok(Some(_)) => {}
                    _ => break,
                }
            }
            seen.len()
        }));
    }

    // Pods alongside the objects.
    let deps = kube.path("apps", "v1", "deployments");
    let dep = format!("wave-{n}");
    let mut tpl = workload::template(&kube.run, image, &dep);
    tpl["metadata"]["labels"]["wave"] = json!(n.to_string());
    kube.create(&deps, json!({"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": dep, "namespace": kube.ns, "labels": {"storm.io/test-run": kube.run, "wave": n.to_string()}},
        "spec": {"replicas": PODS, "selector": {"matchLabels": {"app": dep}}, "template": tpl}}))
        .await?;
    let pods_t = Instant::now();

    // Ramp.
    let t = Instant::now();
    let lat = writers(kube, size, move |k, i| {
        json!({"apiVersion": "v1", "kind": "ConfigMap",
               "metadata": {"name": format!("w{n}-{i}"), "namespace": k.ns, "labels": {"storm.io/test-run": k.run, "wave": n.to_string()}},
               "data": {"i": i.to_string(), "pad": "x".repeat(256)}})
    })
    .await?;
    let ramp = t.elapsed().as_secs_f64();
    let mut sorted: Vec<f64> = lat.iter().map(|d| d.as_secs_f64() * 1000.0).collect();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    let mut missed = 0;
    for w in watchers {
        let seen = tokio::time::timeout(Duration::from_secs(60).min(env.left()), w).await.ok().and_then(|r| r.ok()).unwrap_or(0);
        missed += size.saturating_sub(seen);
    }

    // Hold: update each once (merge patch, as a controller would), then list.
    let mut update_errors = 0;
    for chunk in (0..size).collect::<Vec<_>>().chunks(WRITERS) {
        let mut set = tokio::task::JoinSet::new();
        for &i in chunk {
            let k = kube.clone();
            let path = format!("{cms}/w{n}-{i}");
            set.spawn(async move { k.patch(&path, STRATEGIC, &json!({"data": {"touched": "yes"}})).await.map(|r| r.ok()).unwrap_or(false) });
        }
        while let Some(r) = set.join_next().await {
            if !r.unwrap_or(false) {
                update_errors += 1;
            }
        }
    }
    let t = Instant::now();
    let mut listed = 0;
    let mut cont = String::new();
    loop {
        let mut q = format!("{cms}?{sel}&limit=500");
        if !cont.is_empty() {
            q.push_str(&format!("&continue={}", enc(&cont)));
        }
        let page = kube.get(&q).await?.unwrap_or(Value::Null);
        listed += page["items"].as_array().map(|a| a.len()).unwrap_or(0);
        cont = page["metadata"]["continue"].as_str().unwrap_or("").to_string();
        if cont.is_empty() {
            break;
        }
    }
    let list_ms = t.elapsed().as_millis();
    if listed != size {
        missed += size.abs_diff(listed);
    }

    let (_, ready) = kube
        .wait_for(&format!("{deps}/{dep}"), Duration::from_secs(300).min(env.left()), |v| v["status"]["readyReplicas"] == PODS)
        .await?;
    let pods_ready_ms = ready.then(|| pods_t.elapsed().as_millis());

    // Drain.
    let r = kube.req("DELETE", &format!("{cms}?{sel}"), JSON, None, None).await?;
    if !r.ok() {
        anyhow::bail!("deletecollection: {} {}", r.status, brief(&r.body));
    }
    let opts = json!({"apiVersion": "v1", "kind": "DeleteOptions", "propagationPolicy": "Foreground"});
    kube.req("DELETE", &format!("{deps}/{dep}"), JSON, Some(&opts), None).await?;
    let end = Instant::now() + Duration::from_secs(120).min(env.left());
    let residue = loop {
        let left_cms = kube.get(&format!("{cms}?{sel}")).await?.unwrap_or(Value::Null)["items"].as_array().map(|a| a.len()).unwrap_or(0);
        let left_pods = workload::pods_of(kube, &dep).await?.len();
        let left_dep = kube.get(&format!("{deps}/{dep}")).await?.is_some() as usize;
        let left = left_cms + left_pods + left_dep;
        if left == 0 || Instant::now() >= end {
            break left;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    };

    Ok(Wave {
        n,
        size,
        create_p50_ms: pct(&sorted, 0.5),
        create_p99_ms: pct(&sorted, 0.99),
        writes_per_s: size as f64 / ramp.max(0.001),
        update_errors,
        list_ms,
        watch_missed: missed,
        pods_ready_ms,
        residue,
        rss_mb: rss_mb(kube).await,
        secs: 0,
    })
}

/// Create `count` objects with `WRITERS` at once; the latency of each.
async fn writers(kube: &Arc<Kube>, count: usize, body: impl Fn(&Kube, usize) -> Value) -> Result<Vec<Duration>> {
    let cms = kube.path("", "v1", "configmaps");
    let mut lat = Vec::with_capacity(count);
    for chunk in (0..count).collect::<Vec<_>>().chunks(WRITERS) {
        let mut set = tokio::task::JoinSet::new();
        for &i in chunk {
            let k = kube.clone();
            let b = body(&k, i);
            let path = cms.clone();
            set.spawn(async move {
                let t = Instant::now();
                k.create(&path, b).await.map(|_| t.elapsed())
            });
        }
        while let Some(r) = set.join_next().await {
            lat.push(r??);
        }
    }
    Ok(lat)
}

/// The apiserver's resident memory, from its `/metrics`, if this run may
/// read it.
async fn rss_mb(kube: &Kube) -> Option<f64> {
    let r = kube.req("GET", "/metrics", "", None, Some("text/plain")).await.ok()?;
    let text = r.body.as_str()?.to_string();
    text.lines()
        .find(|l| l.starts_with("process_resident_memory_bytes"))
        .and_then(|l| l.split_whitespace().last())
        .and_then(|v| v.parse::<f64>().ok())
        .map(|b| b / 1_048_576.0)
}

fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(n: usize, p50: f64, rss: Option<f64>) -> Wave {
        Wave { n, create_p50_ms: p50, rss_mb: rss, ..Default::default() }
    }

    #[test]
    fn waves_are_sized_from_the_first_waves_rate() {
        assert_eq!(size_for(10.0, 3), 600);
        assert_eq!(size_for(10.0, 4), 300);
        assert_eq!(size_for(10.0, 5), 900);
        assert_eq!(size_for(1000.0, 3), 5000);
        assert_eq!(size_for(0.1, 3), 100);
    }

    #[test]
    fn a_steady_run_passes_the_trend() {
        let ws: Vec<Wave> = (1..=6).map(|n| w(n, 10.0, Some(100.0))).collect();
        assert!(matches!(trend(&ws), Outcome::Pass(_)));
    }

    #[test]
    fn a_slowdown_or_growing_memory_fails_it() {
        let mut ws: Vec<Wave> = (1..=6).map(|n| w(n, 10.0, Some(100.0))).collect();
        for x in ws.iter_mut().skip(3) {
            x.create_p50_ms = 30.0;
        }
        assert!(matches!(trend(&ws), Outcome::Fail(ref d) if d.contains("first slow wave: 4")));
        let ws: Vec<Wave> = (1..=6).map(|n| w(n, 10.0, Some(100.0 * n as f64))).collect();
        assert!(matches!(trend(&ws), Outcome::Fail(ref d) if d.contains("rss")));
    }

    #[test]
    fn percentiles() {
        let v: Vec<f64> = (1..=100).map(|x| x as f64).collect();
        assert_eq!(pct(&v, 0.5), 51.0);
        assert_eq!(pct(&v, 0.99), 99.0);
        assert_eq!(pct(&[], 0.5), 0.0);
    }
}
