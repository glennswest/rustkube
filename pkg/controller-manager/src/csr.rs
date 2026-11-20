//! CertificateSigningRequest controller — the node-join half of the OpenShift
//! model. Two responsibilities, matching upstream kube-controller-manager:
//!
//! 1. **Approve** — auto-approve a node renewing its own kubelet client
//!    certificate (signerName `kubernetes.io/kube-apiserver-client-kubelet`,
//!    requested by `system:node:<n>` for exactly `CN=system:node:<n>,
//!    O=system:nodes`: [`self_node_client`]). Anything else is left pending
//!    for a human `kubectl certificate approve` or another approver.
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
        if !approved && !denied && spec["signerName"].as_str() == Some(KUBELET_CLIENT_SIGNER) {
            match self_node_client(spec) {
                Ok(()) => {
                    self.approve(&name, csr).await;
                    return Ok(()); // approval acknowledgement queues signing
                }
                // Left pending for a person (or an approver that knows more).
                Err(why) => {
                    tracing::debug!("CSR {name}: not auto-approved: {why}");
                    return Ok(());
                }
            }
        }

        // 2) Sign approved CSRs that have no issued certificate yet.
        if approved && status.get("certificate").and_then(|c| c.as_str()).is_none() {
            if let Some((ca_cert, ca_key)) = &self.ca {
                self.sign(&name, csr, ca_cert, ca_key).await;
            }
        }
        Ok(())
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

/// The usages a kubelet client certificate may ask for (upstream's
/// `kube-apiserver-client-kubelet` signer); `client auth` is required.
const KUBELET_CLIENT_USAGES: [&str; 3] = ["digital signature", "key encipherment", "client auth"];

/// Whether a `kube-apiserver-client-kubelet` CSR is a node renewing its own
/// client certificate — the one case auto-approved, as upstream's approver
/// does for `selfnodeclient` (review finding 77f265e40744). Everything was
/// approved before, whatever it asked: any requester with `create` on CSRs
/// got a certificate for any CN and organisation, `system:masters` included.
///
/// - the PKCS#10 subject is `O=system:nodes` (only) and
///   `CN=system:node:<name>`, with no subject alternative names;
/// - `spec.usages` are within [`KUBELET_CLIENT_USAGES`] and name `client auth`;
/// - the requester (`spec.username`/`spec.groups`, which the apiserver sets
///   from who filed it) is that same node, in `system:nodes`.
///
/// A first certificate for a new node is not this: it waits for a person
/// (`kubectl certificate approve`) or an approver such as stormcert.
pub fn self_node_client(spec: &Value) -> Result<(), String> {
    use x509_parser::prelude::*;
    let request = spec["request"].as_str().ok_or("no spec.request")?;
    let pem_bytes = base64::engine::general_purpose::STANDARD
        .decode(request)
        .map_err(|e| format!("spec.request is not base64: {e}"))?;
    let (_, pem) = x509_parser::pem::parse_x509_pem(&pem_bytes).map_err(|e| format!("spec.request is not PEM: {e}"))?;
    if pem.label != "CERTIFICATE REQUEST" && pem.label != "NEW CERTIFICATE REQUEST" {
        return Err(format!("spec.request is a {:?}, not a certificate request", pem.label));
    }
    let (_, req) = X509CertificationRequest::from_der(&pem.contents)
        .map_err(|e| format!("spec.request is not a PKCS#10 request: {e}"))?;
    let subject = &req.certification_request_info.subject;
    let cns: Vec<&str> = subject.iter_common_name().filter_map(|a| a.as_str().ok()).collect();
    let orgs: Vec<&str> = subject.iter_organization().filter_map(|a| a.as_str().ok()).collect();
    let [cn] = cns.as_slice() else { return Err(format!("subject has {} common names", cns.len())) };
    let node = cn.strip_prefix("system:node:").unwrap_or("");
    if node.is_empty() {
        return Err(format!("CN {cn:?} is not system:node:<name>"));
    }
    if orgs != ["system:nodes"] {
        return Err(format!("organizations {orgs:?} are not exactly [\"system:nodes\"]"));
    }
    if let Some(exts) = req.requested_extensions() {
        for ext in exts {
            if let ParsedExtension::SubjectAlternativeName(_) = ext {
                return Err("a kubelet client certificate carries no subject alternative names".into());
            }
        }
    }
    let usages: Vec<&str> = spec["usages"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    if !usages.contains(&"client auth") {
        return Err(format!("usages {usages:?} do not include client auth"));
    }
    if let Some(u) = usages.iter().copied().find(|u| !KUBELET_CLIENT_USAGES.contains(u)) {
        return Err(format!("usage {u:?} is not a kubelet client usage"));
    }
    if spec["username"].as_str() != Some(*cn) {
        return Err(format!("requested by {:?}, not by {cn:?} itself", spec["username"].as_str().unwrap_or("")));
    }
    let in_nodes = spec["groups"].as_array().into_iter().flatten().any(|g| g.as_str() == Some("system:nodes"));
    if !in_nodes {
        return Err("the requester is not in system:nodes".into());
    }
    Ok(())
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

    /// A base64 PEM PKCS#10 request for `cn` in `orgs`, with `sans`.
    fn request(cn: &str, orgs: &[&str], sans: &[&str]) -> String {
        use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair};
        let mut params = CertificateParams::new(sans.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap();
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, cn);
        for o in orgs {
            dn.push(DnType::OrganizationName, *o);
        }
        params.distinguished_name = dn;
        let key = KeyPair::generate().unwrap();
        let pem = params.serialize_request(&key).unwrap().pem().unwrap();
        base64::engine::general_purpose::STANDARD.encode(pem)
    }

    fn kubelet_csr(req: String, username: &str, groups: &[&str], usages: &[&str]) -> Value {
        json!({"signerName": KUBELET_CLIENT_SIGNER, "request": req, "username": username,
               "groups": groups, "usages": usages})
    }

    #[test]
    fn only_a_node_renewing_its_own_client_certificate_is_auto_approved() {
        let usages = ["digital signature", "client auth"];
        let nodes = ["system:nodes", "system:authenticated"];
        let own = || request("system:node:n1", &["system:nodes"], &[]);
        assert_eq!(self_node_client(&kubelet_csr(own(), "system:node:n1", &nodes, &usages)), Ok(()));
        let all = ["digital signature", "key encipherment", "client auth"];
        assert_eq!(self_node_client(&kubelet_csr(own(), "system:node:n1", &nodes, &all)), Ok(()));

        // Review finding 77f265e40744: each of these was approved.
        let refused = [
            ("another node's identity", kubelet_csr(own(), "system:node:n2", &nodes, &usages)),
            ("someone else asking", kubelet_csr(own(), "mallory", &["system:authenticated"], &usages)),
            ("a bootstrapper", kubelet_csr(own(), "system:bootstrap:abcdef", &["system:bootstrappers"], &usages)),
            ("requester not in system:nodes", kubelet_csr(own(), "system:node:n1", &["system:authenticated"], &usages)),
            ("system:masters asked for",
             kubelet_csr(request("system:node:n1", &["system:nodes", "system:masters"], &[]), "system:node:n1", &nodes, &usages)),
            ("masters only",
             kubelet_csr(request("system:node:n1", &["system:masters"], &[]), "system:node:n1", &nodes, &usages)),
            ("an admin CN", kubelet_csr(request("admin", &["system:nodes"], &[]), "admin", &nodes, &usages)),
            ("an empty node name",
             kubelet_csr(request("system:node:", &["system:nodes"], &[]), "system:node:", &nodes, &usages)),
            ("a SAN", kubelet_csr(request("system:node:n1", &["system:nodes"], &["n1.example"]), "system:node:n1", &nodes, &usages)),
            ("server auth", kubelet_csr(own(), "system:node:n1", &nodes, &["digital signature", "client auth", "server auth"])),
            ("no client auth", kubelet_csr(own(), "system:node:n1", &nodes, &["digital signature"])),
            ("not a request", kubelet_csr("bm90IGEgcmVxdWVzdA==".into(), "system:node:n1", &nodes, &usages)),
        ];
        for (what, spec) in refused {
            assert!(self_node_client(&spec).is_err(), "{what} must not be auto-approved");
        }
        assert!(self_node_client(&json!({"signerName": KUBELET_CLIENT_SIGNER})).is_err(), "no request");
    }
}
