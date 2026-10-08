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
//!
//! **Only its own signers (#199).** As upstream's controller-manager, it signs
//! only the `kubernetes.io/*` signerNames it implements ([`SIGNERS`]): any
//! other signerName (`stormcert.io/…`) is another signer's. And a signerName
//! named in `--csr-external-signer-names` — stormcert as the cluster's
//! external signer for the kubelet signers — is neither approved nor signed
//! here, so stormcert's policy decides and only its CA writes
//! `status.certificate`.

use crate::owned::{self, Controller, Deps};
use crate::runner::ApiClient;
use base64::Engine;
use serde_json::{json, Value};
use std::sync::Arc;
use tracing::{info, warn};

const CSR_PATH: &str = "/apis/certificates.k8s.io/v1/certificatesigningrequests";
const KUBELET_CLIENT_SIGNER: &str = "kubernetes.io/kube-apiserver-client-kubelet";

/// The signerNames upstream's controller-manager signs with the cluster CA.
pub const SIGNERS: [&str; 4] = [
    "kubernetes.io/kube-apiserver-client",
    KUBELET_CLIENT_SIGNER,
    "kubernetes.io/kubelet-serving",
    "kubernetes.io/legacy-unknown",
];

pub struct CsrController {
    api: Arc<ApiClient>,
    /// CA cert + key PEM for signing (None → approval only).
    ca: Option<(String, String)>,
    /// signerNames an external signer handles (#199).
    external: Vec<String>,
}

impl CsrController {
    pub fn new(api: Arc<ApiClient>, ca: Option<(String, String)>) -> Self {
        Self { api, ca, external: Vec::new() }
    }

    pub fn with_external_signers(mut self, external: Vec<String>) -> Self {
        self.external = external;
        self
    }

    /// Is this CSR this controller's to approve or sign at all?
    fn ours(&self, spec: &Value) -> bool {
        is_ours(spec, &self.external)
    }

    pub async fn run(&self) {
        owned::run(&self.api, self).await;
    }

    async fn reconcile_csr(&self, csr: &Value) -> anyhow::Result<()> {
        let name = csr["metadata"]["name"].as_str().unwrap_or("").to_string();
        if name.is_empty() {
            return Ok(());
        }
        let spec = &csr["spec"];
        let status = &csr["status"];
        let approved = has_condition(status, "Approved");
        let denied = has_condition(status, "Denied");
        // Another signer's, or the external signer's (#199): left alone.
        if !self.ours(spec) {
            return Ok(());
        }

        // 1) Approve eligible, undecided CSRs.
        if !approved && !denied && self.should_auto_approve(spec) {
            self.approve(&name, csr).await;
            return Ok(()); // approval acknowledgement queues signing
        }

        // 2) Sign approved CSRs that have no issued certificate yet.
        if approved && status.get("certificate").and_then(|c| c.as_str()).is_none() {
            if let Some((ca_cert, ca_key)) = &self.ca {
                self.sign(&name, csr, ca_cert, ca_key).await;
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

/// A CSR this controller handles: one of [`SIGNERS`], and not one the
/// external signer takes (#199).
pub fn is_ours(spec: &Value, external: &[String]) -> bool {
    let signer = spec["signerName"].as_str().unwrap_or("");
    SIGNERS.contains(&signer) && !external.iter().any(|e| e == signer)
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

#[async_trait::async_trait]
impl Controller for CsrController {
    fn name(&self) -> &'static str {
        "csr"
    }
    fn primary(&self) -> &'static str {
        CSR_PATH
    }
    async fn reconcile(
        &self,
        csr: &Value,
        _children: &[Value],
        _deps: &Deps,
    ) -> anyhow::Result<()> {
        self.reconcile_csr(csr).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_its_own_signers_and_never_an_external_one() {
        let spec = |s: &str| json!({"signerName": s});
        let none: Vec<String> = Vec::new();
        assert!(is_ours(&spec("kubernetes.io/kube-apiserver-client-kubelet"), &none));
        assert!(is_ours(&spec("kubernetes.io/kubelet-serving"), &none));
        assert!(is_ours(&spec("kubernetes.io/kube-apiserver-client"), &none));
        assert!(!is_ours(&spec("stormcert.io/workload"), &none), "another signer's");
        assert!(!is_ours(&json!({}), &none), "no signerName");
        let stormcert = vec!["kubernetes.io/kube-apiserver-client-kubelet".to_string(), "kubernetes.io/kubelet-serving".to_string()];
        assert!(!is_ours(&spec("kubernetes.io/kube-apiserver-client-kubelet"), &stormcert), "the external signer's");
        assert!(!is_ours(&spec("kubernetes.io/kubelet-serving"), &stormcert));
        assert!(is_ours(&spec("kubernetes.io/kube-apiserver-client"), &stormcert), "not listed: still ours");
    }
}
