//! What the run is given: stormcentral's `STORM_*` variables and the
//! ServiceAccount the Job runs as. Nothing about the machine is assumed.

use anyhow::{anyhow, Context, Result};
use std::time::{Duration, Instant};

/// Where a pod's ServiceAccount is mounted. `STORM_SA_DIR` overrides it, for
/// a run outside a pod (test/e2e/test-container.sh).
const SA_DIR: &str = "/var/run/secrets/kubernetes.io/serviceaccount";

pub struct Env {
    /// `https://<node>:6443`, verified with the ServiceAccount's `ca.crt`.
    pub api: String,
    pub run_id: String,
    pub namespace: String,
    pub commit: String,
    pub token: String,
    pub ca: Vec<u8>,
    /// When the suite must be done by: `STORM_TIMEOUT` from the start, less a
    /// margin for cleanup and the summary.
    pub deadline: Instant,
}

fn var(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.is_empty())
}

/// The standard's budgets, for a run by hand without `STORM_TIMEOUT`.
pub fn default_budget(suite: &str) -> u64 {
    match suite {
        "short" => 120,
        "medium" => 1800,
        _ => 8 * 3600,
    }
}

impl Env {
    pub fn discover(suite: &str) -> Result<Env> {
        let sa = var("STORM_SA_DIR").unwrap_or_else(|| SA_DIR.into());
        let namespace = match var("STORM_NAMESPACE") {
            Some(n) => n,
            None => std::fs::read_to_string(format!("{sa}/namespace"))
                .context("no STORM_NAMESPACE and no service-account namespace")?
                .trim()
                .to_string(),
        };
        let api = var("STORM_API")
            .or_else(|| {
                let h = var("KUBERNETES_SERVICE_HOST")?;
                let p = var("KUBERNETES_SERVICE_PORT").unwrap_or_else(|| "443".into());
                Some(if h.contains(':') { format!("https://[{h}]:{p}") } else { format!("https://{h}:{p}") })
            })
            .ok_or_else(|| anyhow!("no STORM_API and no KUBERNETES_SERVICE_HOST"))?;
        let token = std::fs::read_to_string(format!("{sa}/token"))
            .context("reading the service-account token")?
            .trim()
            .to_string();
        let ca = std::fs::read(format!("{sa}/ca.crt")).context("reading the service-account ca.crt")?;
        let budget = var("STORM_TIMEOUT").and_then(|t| t.parse().ok()).unwrap_or_else(|| default_budget(suite));
        Ok(Env {
            api: api.trim_end_matches('/').to_string(),
            run_id: var("STORM_RUN_ID").unwrap_or_else(|| "manual".into()),
            namespace,
            commit: var("STORM_COMMIT").unwrap_or_default(),
            token,
            ca,
            deadline: Instant::now() + Duration::from_secs(budget).saturating_sub(margin(budget)),
        })
    }

    /// Time left before the deadline.
    pub fn left(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }
}

/// What is kept back from the budget: a tenth, at least 15 s and at most
/// 5 min — enough to delete what a suite made and print the summary.
fn margin(budget: u64) -> Duration {
    Duration::from_secs((budget / 10).clamp(15, 300))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_margin_scales_with_the_budget() {
        assert_eq!(margin(120), Duration::from_secs(15));
        assert_eq!(margin(1800), Duration::from_secs(180));
        assert_eq!(margin(8 * 3600), Duration::from_secs(300));
    }
}
