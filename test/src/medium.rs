//! `medium`: not written yet (#96); reports one skip.

use crate::env::Env;
use crate::kube::Kube;
use crate::report::{skip, Report};

pub async fn run(_env: &Env, _kube: &Kube, rep: &mut Report) -> anyhow::Result<()> {
    rep.check("medium", async { skip("the medium suite is not written yet (#96)") }).await
}
