//! CertificateSigningRequest admission (#264, stormcert#78): who asked, what
//! a bootstrapper may ask for, and who may approve and sign.
//!
//! A machine enrolls with its forge by filing a CSR with `signerName:
//! storm.io/forge-node`, first with a bootstrap token, later with its own
//! certificate. stormcert's approver decides by who filed it, so that has to
//! be the apiserver's word, not the client's. As upstream:
//!
//! - **On create** `spec.username` and `spec.groups` are the authenticated
//!   requester's, over whatever the body says (`spec.uid` and `spec.extra`,
//!   which this apiserver has no source for, are dropped), and `status` is
//!   emptied: a request cannot arrive approved or signed.
//! - **On update** `spec` is kept as stored; a main-resource update also
//!   keeps `status`; `/approval` keeps `status.certificate`.
//! - **`/approval`** needs `approve` on `signers` named the CSR's signerName
//!   (or `<domain>/*`); **`/status` setting `status.certificate`** needs
//!   `sign` on it — upstream's CertificateApproval / CertificateSigning
//!   admission. A signer's role is what RBAC says, nothing else.
//!
//! And narrower than upstream, as stormcert#78 asks: a **bootstrapper** (in
//! `system:bootstrappers`, not `system:masters`) may create a CSR only for
//! [`FORGE_NODE_SIGNER`], and read only its own (`spec.username` its name):
//! GET by name, or a WATCH of exactly that name. So an enrollment token
//! cannot obtain a kubelet client certificate, nor see other machines'
//! requests. One exception (#388, stormcentral#1676): a bootstrapper in
//! [`STORMCENTRAL_GROUP`] may also ask [`CLIENT_SIGNER`] for stormcentral's
//! own client certificate — subject `CN=stormcentral` (with `O` only
//! `stormcentral:forge`), no SANs, usages within `client auth`/`digital
//! signature`/`key encipherment` ([`stormcentral_client`]); it too reads
//! only its own. RBAC cannot say either — they depend on the body and on the
//! stored object — so [`guard_read`] runs after RBAC and [`admit`] in
//! admission.

use crate::admission::{Operation, RequestAttrs};
use crate::auth::UserInfo;
use crate::error::ApiError;
use crate::rbac_engine::{AuthorizationRequest, RbacEngine};
use crate::storage::ResourceStorage;
use serde_json::{json, Value};

pub const GROUP: &str = "certificates.k8s.io";
pub const RESOURCE: &str = "certificatesigningrequests";
/// The signer a bootstrapper may ask (#264).
pub const FORGE_NODE_SIGNER: &str = "storm.io/forge-node";

/// The bootstrapper group whose members may also ask [`CLIENT_SIGNER`] for
/// stormcentral's own client certificate (#388, stormcentral#1676).
pub const STORMCENTRAL_GROUP: &str = "system:bootstrappers:stormcentral";
pub const CLIENT_SIGNER: &str = "kubernetes.io/kube-apiserver-client";
/// The one subject that request may carry: `CN=stormcentral`, and
/// `O=stormcentral:forge` or no organisation.
pub const STORMCENTRAL_CN: &str = "stormcentral";
pub const STORMCENTRAL_ORG: &str = "stormcentral:forge";
/// What its `spec.usages` may name; `client auth` is required.
const STORMCENTRAL_USAGES: [&str; 3] = ["client auth", "digital signature", "key encipherment"];

/// Is `user` held to the bootstrap narrowing?
pub fn is_bootstrapper(user: &UserInfo) -> bool {
    user.groups.iter().any(|g| g == crate::bootstrap_token::GROUP)
        && !user.groups.iter().any(|g| g == "system:masters")
}

/// Admission for a CSR write, after the mutating webhooks (`admission`'s
/// `after_mutating`).
pub async fn admit(
    request: &RequestAttrs,
    op: Operation,
    object: &mut Value,
    old: Option<&Value>,
) -> Result<(), ApiError> {
    match (op, request.subresource.as_deref(), old) {
        (Operation::Create, None, _) => on_create(&request.user, object),
        (Operation::Update, sub, Some(stored)) => {
            keep_stored(sub, object, stored);
            let Some(rbac) = &request.rbac else { return Ok(()) };
            let signer = stored["spec"]["signerName"].as_str().unwrap_or("");
            match sub {
                Some("approval") => require(rbac, &request.user, "approve", signer).await,
                Some("status") if object["status"]["certificate"] != stored["status"]["certificate"] => {
                    require(rbac, &request.user, "sign", signer).await
                }
                _ => Ok(()),
            }
        }
        _ => Ok(()),
    }
}

/// The requester stamp, and the bootstrapper's signer narrowing.
pub fn on_create(user: &UserInfo, object: &mut Value) -> Result<(), ApiError> {
    if !object["spec"].is_object() {
        object["spec"] = json!({});
    }
    let signer = object["spec"]["signerName"].as_str().unwrap_or("").to_string();
    if is_bootstrapper(user) && signer != FORGE_NODE_SIGNER {
        let stormcentral = user.groups.iter().any(|g| g == STORMCENTRAL_GROUP);
        if !(stormcentral && signer == CLIENT_SIGNER) {
            return Err(ApiError::forbidden(&format!(
                "certificatesigningrequests is forbidden: User \"{}\" may only request signerName \"{FORGE_NODE_SIGNER}\", not \"{signer}\"",
                user.username
            )));
        }
        stormcentral_client(&object["spec"]).map_err(|why| {
            ApiError::forbidden(&format!(
                "certificatesigningrequests is forbidden: User \"{}\" may request \"{CLIENT_SIGNER}\" only for CN={STORMCENTRAL_CN}: {why}",
                user.username
            ))
        })?;
    }
    let spec = object["spec"].as_object_mut().expect("made an object above");
    spec.insert("username".into(), json!(user.username));
    spec.insert("groups".into(), json!(user.groups));
    spec.remove("uid");
    spec.remove("extra");
    // Status is the approver's and the signer's, never the requester's: a
    // create carrying `Approved` (or a certificate) was stored as sent and
    // the signer issued it (review finding 27b84965dcae). Upstream's
    // PrepareForCreate resets it the same way.
    object["status"] = json!({});
    Ok(())
}

/// Whether a CSR `spec` asks for stormcentral's client certificate and
/// nothing more (#388): the PKCS#10 subject holds exactly one `CN`,
/// `stormcentral`, and at most one `O`, `stormcentral:forge`, and no other
/// attribute; the request asks for no subject alternative names; and
/// `spec.usages` are within [`STORMCENTRAL_USAGES`] and include `client
/// auth`. Approval is still the approver's (stormcert's policy).
pub fn stormcentral_client(spec: &Value) -> Result<(), String> {
    use base64::Engine;
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
    let (mut cns, mut orgs) = (Vec::new(), Vec::new());
    for attr in subject.iter_attributes() {
        let value = attr.as_str().map_err(|_| "a subject attribute is not a string".to_string())?;
        if *attr.attr_type() == x509_parser::oid_registry::OID_X509_COMMON_NAME {
            cns.push(value);
        } else if *attr.attr_type() == x509_parser::oid_registry::OID_X509_ORGANIZATION_NAME {
            orgs.push(value);
        } else {
            return Err(format!("the subject carries {}, not only CN and O", attr.attr_type()));
        }
    }
    if cns != [STORMCENTRAL_CN] {
        return Err(format!("subject common names {cns:?} are not exactly [\"{STORMCENTRAL_CN}\"]"));
    }
    if !(orgs.is_empty() || orgs == [STORMCENTRAL_ORG]) {
        return Err(format!("subject organizations {orgs:?} are not [\"{STORMCENTRAL_ORG}\"]"));
    }
    for ext in req.requested_extensions().into_iter().flatten() {
        if let ParsedExtension::SubjectAlternativeName(_) = ext {
            return Err("it asks for subject alternative names".into());
        }
    }
    let usages: Vec<&str> = spec["usages"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    if !usages.contains(&"client auth") {
        return Err(format!("usages {usages:?} do not include client auth"));
    }
    if let Some(u) = usages.iter().copied().find(|u| !STORMCENTRAL_USAGES.contains(u)) {
        return Err(format!("usage {u:?} is not one of {STORMCENTRAL_USAGES:?}"));
    }
    if spec["usages"].as_array().is_some_and(|a| a.len() != usages.len()) {
        return Err("a usage is not a string".into());
    }
    Ok(())
}

/// What an update may not change.
fn keep_stored(sub: Option<&str>, object: &mut Value, stored: &Value) {
    object["spec"] = stored["spec"].clone();
    match sub {
        None => object["status"] = stored["status"].clone(),
        Some("approval") => match stored["status"].get("certificate") {
            Some(c) => object["status"]["certificate"] = c.clone(),
            None => {
                if let Some(s) = object["status"].as_object_mut() {
                    s.remove("certificate");
                }
            }
        },
        _ => {}
    }
}

/// `verb` (`approve` or `sign`) on `signers`, by the signer's name or its
/// domain's `<domain>/*`, as upstream checks.
async fn require(rbac: &RbacEngine, user: &UserInfo, verb: &str, signer: &str) -> Result<(), ApiError> {
    let mut names = vec![signer.to_string()];
    if let Some((domain, _)) = signer.split_once('/') {
        names.push(format!("{domain}/*"));
    }
    for name in names {
        let req = AuthorizationRequest {
            verb: verb.into(),
            resource: "signers".into(),
            subresource: None,
            api_group: GROUP.into(),
            namespace: None,
            name: Some(name),
        };
        if rbac.authorize(user, &req).await {
            return Ok(());
        }
    }
    Err(ApiError::forbidden(&format!(
        "user not permitted to {verb} requests with signerName \"{signer}\" (User \"{}\")",
        user.username
    )))
}

/// A bootstrapper's read of CSRs, after RBAC allowed it: its own only. `name`
/// is the path's object; `query` the request's query string. Anyone else, or
/// any other resource: `Ok`.
pub async fn guard_read(
    storage: &ResourceStorage,
    user: &UserInfo,
    req: &AuthorizationRequest,
    query: Option<&str>,
) -> Result<(), ApiError> {
    if req.api_group != GROUP || req.resource != RESOURCE || !is_bootstrapper(user) {
        return Ok(());
    }
    if !matches!(req.verb.as_str(), "get" | "list" | "watch") {
        return Ok(());
    }
    let name = match &req.name {
        Some(n) => Some(n.clone()),
        // A collection: only a watch pinned to one name.
        None => watched_name(query),
    };
    let refuse = || {
        ApiError::forbidden(&format!(
            "certificatesigningrequests is forbidden: User \"{}\" may read only its own requests, by name",
            user.username
        ))
    };
    let Some(name) = name else { return Err(refuse()) };
    let key = ResourceStorage::cluster_key(RESOURCE, &name);
    match storage.get(&key).await {
        Ok(csr) if csr["spec"]["username"].as_str() == Some(user.username.as_str()) => Ok(()),
        _ => Err(refuse()),
    }
}

/// `X` of `watch=true&fieldSelector=metadata.name=X` (nothing else selected).
fn watched_name(query: Option<&str>) -> Option<String> {
    let mut watch = false;
    let mut name = None;
    for (k, v) in form_urlencoded::parse(query?.as_bytes()) {
        match k.as_ref() {
            "watch" => watch = v == "true" || v == "1",
            "fieldSelector" => {
                let v = v.trim();
                let n = v.strip_prefix("metadata.name==").or_else(|| v.strip_prefix("metadata.name="))?;
                if n.is_empty() || n.contains(',') {
                    return None;
                }
                name = Some(n.to_string());
            }
            "labelSelector" if !v.is_empty() => return None,
            _ => {}
        }
    }
    watch.then_some(name).flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boot() -> UserInfo {
        UserInfo {
            username: "system:bootstrap:abcdef".into(),
            groups: vec!["system:bootstrappers".into(), "system:authenticated".into()],
        }
    }

    fn node() -> UserInfo {
        UserInfo { username: "system:node:n1".into(), groups: vec!["system:nodes".into(), "system:authenticated".into()] }
    }

    #[test]
    fn create_stamps_the_requester_over_the_body() {
        let mut csr = json!({"spec": {"signerName": FORGE_NODE_SIGNER, "request": "x",
            "username": "system:node:victim", "groups": ["system:nodes"], "uid": "u", "extra": {"a": ["b"]}}});
        on_create(&boot(), &mut csr).unwrap();
        assert_eq!(csr["spec"]["username"], "system:bootstrap:abcdef");
        assert_eq!(csr["spec"]["groups"], json!(["system:bootstrappers", "system:authenticated"]));
        assert!(csr["spec"].get("uid").is_none() && csr["spec"].get("extra").is_none());
        assert_eq!(csr["spec"]["request"], "x");
    }

    #[test]
    fn create_drops_a_client_supplied_status() {
        // Review finding 27b84965dcae: an Approved condition (or a
        // certificate) in the create body was stored as sent.
        let mut csr = json!({"spec": {"signerName": "kubernetes.io/kube-apiserver-client", "request": "x"},
            "status": {"conditions": [{"type": "Approved", "status": "True", "reason": "Self"}],
                       "certificate": "Zm9yZ2Vk"}});
        on_create(&node(), &mut csr).unwrap();
        assert_eq!(csr["status"], json!({}));
    }

    #[test]
    fn a_bootstrapper_asks_only_the_forge_node_signer() {
        for signer in ["kubernetes.io/kube-apiserver-client-kubelet", "kubernetes.io/kube-apiserver-client", "", "storm.io/other"] {
            let mut csr = json!({"spec": {"signerName": signer}});
            let e = on_create(&boot(), &mut csr).unwrap_err();
            assert_eq!(e.status, axum::http::StatusCode::FORBIDDEN, "{signer}");
        }
        let mut no_spec = json!({});
        assert!(on_create(&boot(), &mut no_spec).is_err());
        // A node (renewal) and an admin bootstrapper are not narrowed.
        let mut csr = json!({"spec": {"signerName": FORGE_NODE_SIGNER}});
        on_create(&node(), &mut csr).unwrap();
        assert_eq!(csr["spec"]["username"], "system:node:n1");
        let mut csr = json!({"spec": {"signerName": "kubernetes.io/kube-apiserver-client-kubelet"}});
        on_create(&node(), &mut csr).unwrap();
        let admin = UserInfo { username: "a".into(), groups: vec!["system:bootstrappers".into(), "system:masters".into()] };
        let mut csr = json!({"spec": {"signerName": "kubernetes.io/legacy-unknown"}});
        on_create(&admin, &mut csr).unwrap();
    }

    /// A base64 PEM PKCS#10 request with these subject attributes and SANs.
    fn request(subject: &[(rcgen::DnType, &str)], sans: &[&str]) -> String {
        use base64::Engine;
        use rcgen::{CertificateParams, DistinguishedName, KeyPair};
        let mut params = CertificateParams::new(sans.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap();
        let mut dn = DistinguishedName::new();
        for (t, v) in subject {
            dn.push(t.clone(), *v);
        }
        params.distinguished_name = dn;
        let key = KeyPair::generate().unwrap();
        let pem = params.serialize_request(&key).unwrap().pem().unwrap();
        base64::engine::general_purpose::STANDARD.encode(pem)
    }

    fn stormcentral() -> UserInfo {
        UserInfo {
            username: "system:bootstrap:sc0001".into(),
            groups: vec!["system:bootstrappers".into(), STORMCENTRAL_GROUP.into(), "system:authenticated".into()],
        }
    }

    #[test]
    fn stormcentral_may_ask_for_its_own_client_certificate() {
        use rcgen::DnType::{CommonName as CN, OrganizationName as O, OrganizationalUnitName as OU};
        let csr = |req: String, usages: Value| json!({"spec": {"signerName": CLIENT_SIGNER, "request": req, "usages": usages}});
        let usages = json!(["digital signature", "key encipherment", "client auth"]);
        // Allowed: CN=stormcentral, with O=stormcentral:forge or no O.
        for subject in [vec![(CN, "stormcentral"), (O, "stormcentral:forge")], vec![(CN, "stormcentral")]] {
            let mut c = csr(request(&subject, &[]), usages.clone());
            on_create(&stormcentral(), &mut c).unwrap();
            assert_eq!(c["spec"]["username"], "system:bootstrap:sc0001");
            assert_eq!(c["spec"]["groups"][1], STORMCENTRAL_GROUP);
        }
        let mut c = csr(request(&[(CN, "stormcentral")], &[]), json!(["client auth"]));
        on_create(&stormcentral(), &mut c).unwrap();
        // The forge-node signer is still open to it, as to any bootstrapper.
        let mut c = json!({"spec": {"signerName": FORGE_NODE_SIGNER}});
        on_create(&stormcentral(), &mut c).unwrap();

        let ok = || request(&[(CN, "stormcentral"), (O, "stormcentral:forge")], &[]);
        let refused = [
            ("another CN", csr(request(&[(CN, "admin")], &[]), usages.clone())),
            ("a node CN", csr(request(&[(CN, "system:node:n1"), (O, "system:nodes")], &[]), usages.clone())),
            ("no CN", csr(request(&[(O, "stormcentral:forge")], &[]), usages.clone())),
            ("system:masters", csr(request(&[(CN, "stormcentral"), (O, "system:masters")], &[]), usages.clone())),
            ("an OU", csr(request(&[(CN, "stormcentral"), (OU, "x")], &[]), usages.clone())),
            ("a SAN", csr(request(&[(CN, "stormcentral")], &["stormcentral.example"]), usages.clone())),
            ("server auth", csr(ok(), json!(["client auth", "server auth"]))),
            ("no client auth", csr(ok(), json!(["digital signature"]))),
            ("no usages", csr(ok(), Value::Null)),
            ("a non-string usage", csr(ok(), json!(["client auth", 7]))),
            ("not a request", csr("bm90IGEgcmVxdWVzdA==".into(), usages.clone())),
            ("no request", json!({"spec": {"signerName": CLIENT_SIGNER, "usages": usages.clone()}})),
            ("the kubelet signer", json!({"spec": {"signerName": "kubernetes.io/kube-apiserver-client-kubelet",
                                                   "request": ok(), "usages": usages.clone()}})),
            ("kubelet-serving", json!({"spec": {"signerName": "kubernetes.io/kubelet-serving",
                                                "request": ok(), "usages": usages.clone()}})),
        ];
        for (what, mut c) in refused {
            let e = on_create(&stormcentral(), &mut c).unwrap_err();
            assert_eq!(e.status, axum::http::StatusCode::FORBIDDEN, "{what}");
        }
        // Another bootstrapper may not ask it at all, even for CN=stormcentral.
        let mut c = csr(ok(), usages.clone());
        assert!(on_create(&boot(), &mut c).is_err());
        let other_group = UserInfo { username: "system:bootstrap:abcdef".into(),
            groups: vec!["system:bootstrappers".into(), "system:bootstrappers:forge".into()] };
        let mut c = csr(ok(), usages);
        assert!(on_create(&other_group, &mut c).is_err());
    }

    #[test]
    fn updates_keep_spec_and_what_the_subresource_does_not_own() {
        let stored = json!({"spec": {"username": "u", "signerName": FORGE_NODE_SIGNER},
            "status": {"conditions": [], "certificate": "Y2VydA=="}});
        // Main resource: spec and status both kept.
        let mut put = json!({"spec": {"username": "mallory"}, "status": {"certificate": "Zm9yZ2Vk"}, "metadata": {"labels": {"a": "b"}}});
        keep_stored(None, &mut put, &stored);
        assert_eq!(put["spec"], stored["spec"]);
        assert_eq!(put["status"], stored["status"]);
        assert_eq!(put["metadata"]["labels"]["a"], "b");
        // /approval: conditions change, the certificate does not.
        let mut appr = json!({"spec": {}, "status": {"conditions": [{"type": "Approved"}], "certificate": "Zm9yZ2Vk"}});
        keep_stored(Some("approval"), &mut appr, &stored);
        assert_eq!(appr["status"]["certificate"], "Y2VydA==");
        assert_eq!(appr["status"]["conditions"][0]["type"], "Approved");
        let unsigned = json!({"spec": {}, "status": {}});
        let mut appr = json!({"status": {"conditions": [], "certificate": "Zm9yZ2Vk"}});
        keep_stored(Some("approval"), &mut appr, &unsigned);
        assert!(appr["status"].get("certificate").is_none());
        // /status: the certificate may change (a signer, checked by RBAC).
        let mut st = json!({"spec": {"username": "x"}, "status": {"certificate": "bmV3"}});
        keep_stored(Some("status"), &mut st, &stored);
        assert_eq!(st["status"]["certificate"], "bmV3");
        assert_eq!(st["spec"], stored["spec"]);
    }

    #[test]
    fn watch_pinned_to_a_name() {
        assert_eq!(watched_name(Some("watch=true&fieldSelector=metadata.name%3Dcsr-1")).as_deref(), Some("csr-1"));
        assert_eq!(watched_name(Some("fieldSelector=metadata.name==csr-1&watch=1")).as_deref(), Some("csr-1"));
        assert_eq!(watched_name(Some("fieldSelector=metadata.name%3Dcsr-1")), None, "a list, not a watch");
        assert_eq!(watched_name(Some("watch=true")), None);
        assert_eq!(watched_name(Some("watch=true&fieldSelector=spec.signerName%3Dx")), None);
        assert_eq!(watched_name(Some("watch=true&fieldSelector=metadata.name%3Da,spec.x%3Db")), None);
        assert_eq!(watched_name(Some("watch=true&fieldSelector=metadata.name%3Da&labelSelector=x")), None);
        assert_eq!(watched_name(None), None);
    }
}
