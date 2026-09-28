//! rustkube-test: rustkube's control plane on a running node, tested from a
//! pod, per stormcentral `docs/test-standard.md` (#96).
//!
//! `/test short|medium|long` prints one JSON object per test and a summary,
//! and exits 0 if everything passed, 1 if a test failed, and 2 if the suite
//! could not run. `/test idle [seconds]` is what the pods the suites create
//! run. See test/README.md for what each suite covers and what it needs.

mod env;
mod kube;
mod long;
mod medium;
mod report;
mod short;
mod workload;

use report::Report;

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let suite = args.next().or_else(|| std::env::var("STORM_SUITE").ok()).unwrap_or_else(|| "short".into());
    if suite == "idle" {
        std::process::exit(workload::idle(args.next().and_then(|s| s.parse().ok())).await);
    }
    std::process::exit(run(&suite).await);
}

async fn run(suite: &str) -> i32 {
    let mut rep = Report::default();
    if !matches!(suite, "short" | "medium" | "long") {
        return rep.abort(&anyhow::anyhow!("unknown suite {suite:?}: use short, medium or long"));
    }
    let env = match env::Env::discover(suite) {
        Ok(e) => e,
        Err(e) => return rep.abort(&e),
    };
    eprintln!(
        "rustkube-test {suite}: {} namespace {}, run {}, commit {}, {:?} to run",
        env.api,
        env.namespace,
        env.run_id,
        if env.commit.is_empty() { "?" } else { &env.commit },
        env.left()
    );
    let kube = match kube::Kube::new(&env) {
        Ok(k) => k,
        Err(e) => return rep.abort(&e),
    };
    if let Err(e) = kube.preflight().await {
        return rep.abort(&e.context("the apiserver"));
    }
    let r = match suite {
        "short" => short::run(&env, &kube, &mut rep).await,
        "medium" => medium::run(&env, &kube, &mut rep).await,
        _ => long::run(&env, &kube, &mut rep).await,
    };
    match r {
        Ok(()) => rep.finish(),
        Err(e) => rep.abort(&e),
    }
}
