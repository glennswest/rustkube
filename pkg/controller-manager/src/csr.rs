//! CertificateSigningRequest controller — the node-join half of the OpenShift
//! model. Two responsibilities, matching upstream kube-controller-manager:
//!
//! 1. **Approve** — auto-approve kubelet client-cert CSRs from bootstrappers
//!    (signerName `kubernetes.io/kube-apiserver-client-kubelet`). Anything else
//!    is left pending for a human `kubectl certificate approve`.
//! 2. **Sign** — for approved CSRs with no issued cert, sign the embedded PKCS#10
//!    request with the cluster CA and publish the cert in `status.certificate`.
//!
//! Requires the cluster CA cert+key (`--cluster-signing-cert-file` /
//! `--cluster-signing-key-file`); without them only approval runs.

use crate::runner::ApiClient;
use base64::Engine;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::time::{self, Duration};
use tracing::{error, info, warn};

const CSR_PATH: &str = "/apis/certificates.k8s.io/v1/certificatesigningrequests";
const KUBELET_CLIENT_SIGNER: &str = "kubernetes.io/kube-apiserver-client-kubelet";

pub struct CsrController {
    api: Arc<ApiClient>,
    /// CA cert + key PEM for signing (None → approval only).
    ca: Option<(String, String)>,
}

impl CsrController {
    pub fn new(api: Arc<ApiClient>, ca: Option<(String, String)>) -> Self {
        Self { api, ca }
    }

    pub async fn run(&self) {
        info!(
            "CSR controller started (signing {})",
            if self.ca.is_some() { "enabled" } else { "disabled" }
        );
        let mut interval = time::interval(Duration::from_secs(5));
        loop {
            interval.tick().await;
            if let Err(e) = self.reconcile().await {
                error!("CSR reconcile error: {e}");
            }
        }
    }

    async fn reconcile(&self) -> anyhow::Result<()> {
        let list = self.api.list(CSR_PATH).await?;
        let items = list["items"].as_array().cloned().unwrap_or_default();
        for csr in &items {
            let name = csr["metadata"]["name"].as_str().unwrap_or("").to_string();
            if name.is_empty() {
                continue;
            }
            let spec = &csr["spec"];
            let status = &csr["status"];
            let approved = has_condition(status, "Approved");
            let denied = has_condition(status, "Denied");

            // 1) Approve eligible, undecided CSRs.
            if !approved && !denied && self.should_auto_approve(spec) {
                self.approve(&name, csr).await;
                continue; // sign on the next pass, once the approval is persisted
            }

            // 2) Sign approved CSRs that have no issued certificate yet.
            if approved && status.get("certificate").and_then(|c| c.as_str()).is_none() {
                if let Some((ca_cert, ca_key)) = &self.ca {
                    self.sign(&name, csr, ca_cert, ca_key).await;
                }
            }
        }
        Ok(())
    }

    /// Auto-approve kubelet client CSRs (bootstrap node join). Everything else
    /// waits for manual approval.
    fn should_auto_approve(&self, spec: &Value) -> bool {
        spec["signerName"].as_str() == Some(KUBELET_CLIENT_SIGNER)
    }

    async fn approve(&self, name: &str, csr: &Value) {
        let mut updated = csr.clone();
        let conds = updated["status"]["conditions"].as_array().cloned();
        let mut conds = conds.unwrap_or_default();
        conds.push(json!({
            "type": "Approved",
            "status": "True",
            "reason": "AutoApproved",
            "message": "Auto-approved kubelet client CSR by controller-manager"
        }));
        updated["status"]["conditions"] = json!(conds);
        let path = format!("{CSR_PATH}/{name}/approval");
        match self.api.update(&path, &updated).await {
            Ok(_) => info!("CSR {name}: approved"),
            Err(e) => warn!("CSR {name}: approval failed: {e}"),
        }
    }

    async fn sign(&self, name: &str, csr: &Value, ca_cert: &str, ca_key: &str) {
        let req_b64 = match csr["spec"]["request"].as_str() {
            Some(r) => r,
            None => return,
        };
        let csr_pem = match base64::engine::general_purpose::STANDARD.decode(req_b64) {
            Ok(bytes) => String::from_utf8_lossy(&bytes).to_string(),
            Err(e) => {
                warn!("CSR {name}: bad base64 request: {e}");
                return;
            }
        };
        let cert_pem = match sign_csr(&csr_pem, ca_cert, ca_key) {
            Ok(pem) => pem,
            Err(e) => {
                warn!("CSR {name}: signing failed: {e}");
                return;
            }
        };
        let cert_b64 = base64::engine::general_purpose::STANDARD.encode(cert_pem.as_bytes());
        let mut updated = csr.clone();
        updated["status"]["certificate"] = json!(cert_b64);
        let path = format!("{CSR_PATH}/{name}/status");
        match self.api.update(&path, &updated).await {
            Ok(_) => info!("CSR {name}: signed and issued certificate"),
            Err(e) => warn!("CSR {name}: publishing cert failed: {e}"),
        }
    }
}

fn has_condition(status: &Value, cond_type: &str) -> bool {
    status["conditions"]
        .as_array()
        .map(|cs| cs.iter().any(|c| c["type"].as_str() == Some(cond_type)))
        .unwrap_or(false)
}

/// Sign a PKCS#10 CSR with the cluster CA, returning the issued cert PEM.
fn sign_csr(csr_pem: &str, ca_cert_pem: &str, ca_key_pem: &str) -> anyhow::Result<String> {
    use rcgen::{CertificateParams, CertificateSigningRequestParams, KeyPair};
    let ca_key = KeyPair::from_pem(ca_key_pem)?;
    let ca_cert = CertificateParams::from_ca_cert_pem(ca_cert_pem)?.self_signed(&ca_key)?;
    let csr = CertificateSigningRequestParams::from_pem(csr_pem)?;
    let cert = csr.params.signed_by(&csr.public_key, &ca_cert, &ca_key)?;
    Ok(cert.pem())
}
