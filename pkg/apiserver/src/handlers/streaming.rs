//! `exec`, `attach` and `portforward` — the streaming subresources.
//!
//! These are not request/response endpoints. The client asks to upgrade the
//! connection (SPDY/3.1 from an older `kubectl`, WebSocket from a newer one),
//! the kubelet answers `101 Switching Protocols`, and from then on it is a
//! multiplexed byte stream carrying stdin, stdout, stderr and a resize channel.
//! The apiserver's job in the middle is to be **transparent**: authenticate the
//! caller, find the node, open the same upgrade to that kubelet, and then get
//! out of the way and copy bytes.
//!
//! Transparency is what makes one implementation serve both protocols. Nothing
//! here parses SPDY frames or WebSocket frames — the client's headers are
//! forwarded verbatim (including `Sec-WebSocket-Key`, so the accept value the
//! kubelet computes is the one the client is waiting for), the kubelet's
//! response headers come back verbatim, and after the 101 both directions are
//! spliced. Upstream's kube-apiserver does the same thing for the same reason.
//!
//! The one translation that must happen is the query. `kubectl` speaks the
//! apiserver's `PodExecOptions` spelling — `stdin`, `stdout`, `stderr` — and
//! the kubelet's endpoint expects `input`, `output`, `error`. A proxy that
//! forwarded the query unchanged would open an exec session with no streams
//! attached, which hangs rather than fails.

use crate::error::ApiError;
use crate::handlers::AppState;
use crate::storage::ResourceStorage;
use axum::extract::{Extension, Path, RawQuery, Request, State};
use axum::http::{HeaderMap, HeaderName, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::Value;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{debug, warn};

/// The kubelet's read/write port.
const KUBELET_PORT: u16 = 10250;

/// Cap on the response head we will read from the kubelet before giving up.
const MAX_HEAD: usize = 64 * 1024;

/// Headers we set ourselves, or that describe the hop rather than the request.
const HOP_HEADERS: &[&str] = &["host", "authorization", "content-length", "transfer-encoding"];

/// `POST|GET /api/v1/namespaces/{namespace}/pods/{name}/exec`
pub async fn pod_exec(
    State(state): State<AppState>,
    Extension(keys): Extension<crate::auth::SigningKeys>,
    Path((namespace, name)): Path<(String, String)>,
    RawQuery(query): RawQuery,
    req: Request,
) -> Response {
    let query = query.unwrap_or_default();
    let pod = match load_pod(&state, &namespace, &name).await {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };
    let container = match pick_container(&pod, &query, &name) {
        Ok(c) => c,
        Err(e) => return e.into_response(),
    };
    let path = format!("/exec/{namespace}/{name}/{container}");
    proxy(state, keys, &pod, &namespace, &name, path, stream_query(&query, true), req).await
}

/// `POST|GET /api/v1/namespaces/{namespace}/pods/{name}/attach`
pub async fn pod_attach(
    State(state): State<AppState>,
    Extension(keys): Extension<crate::auth::SigningKeys>,
    Path((namespace, name)): Path<(String, String)>,
    RawQuery(query): RawQuery,
    req: Request,
) -> Response {
    let query = query.unwrap_or_default();
    let pod = match load_pod(&state, &namespace, &name).await {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };
    let container = match pick_container(&pod, &query, &name) {
        Ok(c) => c,
        Err(e) => return e.into_response(),
    };
    let path = format!("/attach/{namespace}/{name}/{container}");
    proxy(state, keys, &pod, &namespace, &name, path, stream_query(&query, false), req).await
}

/// `POST|GET /api/v1/namespaces/{namespace}/pods/{name}/portforward`
pub async fn pod_portforward(
    State(state): State<AppState>,
    Extension(keys): Extension<crate::auth::SigningKeys>,
    Path((namespace, name)): Path<(String, String)>,
    RawQuery(query): RawQuery,
    req: Request,
) -> Response {
    let query = query.unwrap_or_default();
    let pod = match load_pod(&state, &namespace, &name).await {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };
    let path = format!("/portForward/{namespace}/{name}");
    proxy(state, keys, &pod, &namespace, &name, path, portforward_query(&query), req).await
}

async fn load_pod(state: &AppState, namespace: &str, name: &str) -> Result<Value, ApiError> {
    state
        .storage
        .get(&ResourceStorage::namespaced_key("pods", namespace, name))
        .await
        .map_err(|_| ApiError::not_found("pods", name))
}

/// Which container the session is for.
///
/// Upstream requires the name when a pod has more than one and lists the
/// choices in the error, which is the difference between a usable message and
/// a puzzle.
fn pick_container(pod: &Value, query: &str, pod_name: &str) -> Result<String, ApiError> {
    let containers: Vec<String> = pod["spec"]["containers"]
        .as_array()
        .map(|v| v.as_slice())
        .unwrap_or(&[])
        .iter()
        .filter_map(|c| c["name"].as_str().map(str::to_string))
        .collect();
    let asked = form_urlencoded::parse(query.as_bytes())
        .find(|(k, _)| k == "container")
        .map(|(_, v)| v.to_string())
        .filter(|c| !c.is_empty());
    match asked {
        Some(c) => {
            // Ephemeral debug containers are real targets for exec — `kubectl
            // debug` attaches to one — so they count as valid names.
            let ephemeral: Vec<String> = pod["spec"]["ephemeralContainers"]
                .as_array()
                .map(|v| v.as_slice())
                .unwrap_or(&[])
                .iter()
                .filter_map(|c| c["name"].as_str().map(str::to_string))
                .collect();
            if !containers.contains(&c) && !ephemeral.contains(&c) {
                return Err(ApiError {
                    status: StatusCode::BAD_REQUEST,
                    reason: "BadRequest".into(),
                    message: format!(
                        "container {c} is not valid for pod {pod_name}; choose one of [{}]",
                        containers.join(", ")
                    ),
                });
            }
            Ok(c)
        }
        None if containers.len() == 1 => Ok(containers[0].clone()),
        None => Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            reason: "BadRequest".into(),
            message: format!(
                "a container name must be specified for pod {pod_name}, choose one of: [{}]",
                containers.join(", ")
            ),
        }),
    }
}

/// Translate the apiserver's stream options into the kubelet's spelling.
///
/// `stdin`/`stdout`/`stderr` on this side are `input`/`output`/`error` on the
/// kubelet's, and `command` (repeatable, order-significant) passes through.
/// This is the whole reason the proxy cannot be a blind URL rewrite.
fn stream_query(query: &str, with_command: bool) -> String {
    let mut out: Vec<String> = Vec::new();
    for (k, v) in form_urlencoded::parse(query.as_bytes()) {
        let on = matches!(v.as_ref(), "1" | "true" | "True");
        match k.as_ref() {
            "stdin" | "input" if on => out.push("input=1".into()),
            "stdout" | "output" if on => out.push("output=1".into()),
            "stderr" | "error" if on => out.push("error=1".into()),
            "tty" if on => out.push("tty=1".into()),
            "command" if with_command => {
                out.push(format!("command={}", urlencode(&v)));
            }
            _ => {}
        }
    }
    out.join("&")
}

/// `ports=8080,9090` (what a client sends) becomes `port=8080&port=9090`
/// (what the kubelet reads).
fn portforward_query(query: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for (k, v) in form_urlencoded::parse(query.as_bytes()) {
        if k != "ports" && k != "port" {
            continue;
        }
        for part in v.split(',').filter(|p| !p.is_empty()) {
            out.push(format!("port={}", urlencode(part)));
        }
    }
    out.join("&")
}

fn urlencode(s: &str) -> String {
    form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

/// Open the same upgrade to the pod's kubelet and splice the two connections.
#[allow(clippy::too_many_arguments)]
async fn proxy(
    state: AppState,
    keys: crate::auth::SigningKeys,
    pod: &Value,
    namespace: &str,
    name: &str,
    path: String,
    query: String,
    req: Request,
) -> Response {
    let Some(node_name) = pod["spec"]["nodeName"].as_str().filter(|s| !s.is_empty()) else {
        return ApiError {
            status: StatusCode::BAD_REQUEST,
            reason: "BadRequest".into(),
            message: format!("pod {namespace}/{name} is not assigned to a node yet"),
        }
        .into_response();
    };
    let Some(addr) = crate::handlers::logs::node_address(&state.storage, node_name).await else {
        return ApiError::internal(&format!("no usable address for node {node_name}"))
            .into_response();
    };

    // The same identity the log proxy uses: the kubelet validates it by
    // TokenReview against this apiserver, so nothing has to be baked into the
    // node image.
    let bearer = keys
        .create_token("system:kube-apiserver", &["system:masters".to_string()])
        .unwrap_or_default();

    match open_upstream(&addr, &path, &query, &bearer, req.headers()).await {
        Ok(Upstream::Upgraded {
            headers,
            leftover,
            stream,
        }) => {
            // Hand the client the kubelet's own handshake — subprotocol,
            // accept key and all — then splice.
            let mut response = Response::builder().status(StatusCode::SWITCHING_PROTOCOLS);
            for (k, v) in headers.iter() {
                if k == "content-length" || k == "transfer-encoding" {
                    continue;
                }
                response = response.header(k, v);
            }
            let response = match response.body(axum::body::Body::empty()) {
                Ok(r) => r,
                Err(e) => return ApiError::internal(&format!("building upgrade: {e}")).into_response(),
            };

            tokio::spawn(async move {
                match hyper::upgrade::on(req).await {
                    Ok(upgraded) => {
                        let mut client = hyper_util::rt::TokioIo::new(upgraded);
                        let mut upstream = stream;
                        // Anything the kubelet already sent after its headers
                        // belongs to the client, and it is the first frame of
                        // the stream — losing it hangs the session.
                        if !leftover.is_empty() {
                            if let Err(e) = client.write_all(&leftover).await {
                                debug!("streaming: first write to client failed: {e}");
                                return;
                            }
                        }
                        match tokio::io::copy_bidirectional(&mut client, &mut upstream).await {
                            Ok((from_client, from_kubelet)) => debug!(
                                "streaming session closed: {from_client}B up, {from_kubelet}B down"
                            ),
                            Err(e) => debug!("streaming session ended: {e}"),
                        }
                    }
                    Err(e) => warn!("streaming: client never upgraded: {e}"),
                }
            });
            response
        }
        // The kubelet refused before upgrading — a missing container, a pod
        // that is not running. Its answer is the useful one, so it is passed
        // through rather than replaced.
        Ok(Upstream::Refused { status, body }) => (
            StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
            body,
        )
            .into_response(),
        Err(e) => ApiError::internal(&format!("reaching kubelet at {addr}: {e}")).into_response(),
    }
}

enum Upstream {
    Upgraded {
        headers: HeaderMap,
        leftover: Vec<u8>,
        stream: tokio_rustls::client::TlsStream<TcpStream>,
    },
    Refused {
        status: u16,
        body: String,
    },
}

/// Send the upgrade request to the kubelet and read its response head.
async fn open_upstream(
    addr: &str,
    path: &str,
    query: &str,
    bearer: &str,
    client_headers: &HeaderMap,
) -> anyhow::Result<Upstream> {
    let target = format!("{addr}:{KUBELET_PORT}");
    let tcp = TcpStream::connect(&target).await?;
    tcp.set_nodelay(true)?;
    let mut stream = connect_tls(tcp, addr).await?;

    let url = if query.is_empty() {
        path.to_string()
    } else {
        format!("{path}?{query}")
    };
    let mut head = format!(
        "POST {url} HTTP/1.1\r\nHost: {target}\r\nAuthorization: Bearer {bearer}\r\n\
         Content-Length: 0\r\n"
    );
    // The client's own headers carry the protocol negotiation: `Connection`,
    // `Upgrade`, `X-Stream-Protocol-Version`, and for WebSocket the key whose
    // accept value the client will check. Forwarded verbatim.
    for (k, v) in client_headers.iter() {
        if HOP_HEADERS.contains(&k.as_str()) {
            continue;
        }
        if let Ok(value) = v.to_str() {
            head.push_str(&format!("{k}: {value}\r\n"));
        }
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await?;
    stream.flush().await?;

    // Read until the end of the response head.
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(pos) = find_head_end(&buf) {
            break pos;
        }
        if buf.len() > MAX_HEAD {
            anyhow::bail!("kubelet response head exceeded {MAX_HEAD} bytes");
        }
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            anyhow::bail!("kubelet closed the connection without answering");
        }
        buf.extend_from_slice(&chunk[..n]);
    };

    let (status, headers) = parse_head(&buf[..head_end])?;
    let rest = buf[head_end..].to_vec();

    if status != 101 {
        // Not an upgrade: read the body so the client sees the reason.
        let len = headers
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<usize>().ok());
        let mut body = rest;
        match len {
            Some(len) => {
                while body.len() < len {
                    let n = stream.read(&mut chunk).await?;
                    if n == 0 {
                        break;
                    }
                    body.extend_from_slice(&chunk[..n]);
                }
            }
            None => {
                // No length: read until the kubelet closes, bounded.
                while body.len() < MAX_HEAD {
                    match stream.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => body.extend_from_slice(&chunk[..n]),
                    }
                }
            }
        }
        return Ok(Upstream::Refused {
            status,
            body: String::from_utf8_lossy(&body).to_string(),
        });
    }

    Ok(Upstream::Upgraded {
        headers,
        leftover: rest,
        stream,
    })
}

/// TLS to the kubelet.
///
/// The kubelet serves a self-signed certificate whose SANs are the node name
/// and IP; upstream verifies it against the cluster CA. Until certificates are
/// issued (#20) the connection is encrypted but not verified, which is stated
/// here rather than left for a reader to infer from a flag.
async fn connect_tls(
    tcp: TcpStream,
    host: &str,
) -> anyhow::Result<tokio_rustls::client::TlsStream<TcpStream>> {
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .dangerous()
    .with_custom_certificate_verifier(Arc::new(AcceptAnyKubelet))
    .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    // The name is only used for SNI here; verification is off (see above).
    let server_name = rustls::pki_types::ServerName::try_from(host.to_string())
        .unwrap_or(rustls::pki_types::ServerName::IpAddress(
            std::net::IpAddr::from([127, 0, 0, 1]).into(),
        ));
    Ok(connector.connect(server_name, tcp).await?)
}

#[derive(Debug)]
struct AcceptAnyKubelet;

impl rustls::client::danger::ServerCertVerifier for AcceptAnyKubelet {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Index just past the blank line ending an HTTP head.
fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
}

/// Parse a response head into its status code and headers.
fn parse_head(head: &[u8]) -> anyhow::Result<(u16, HeaderMap)> {
    let text = String::from_utf8_lossy(head);
    let mut lines = text.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| anyhow::anyhow!("unparseable status line from kubelet: {status_line}"))?;
    let mut headers = HeaderMap::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        if let (Ok(name), Ok(value)) = (
            k.trim().parse::<HeaderName>(),
            v.trim().parse::<axum::http::HeaderValue>(),
        ) {
            headers.insert(name, value);
        }
    }
    Ok((status, headers))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn exec_options_are_translated_to_the_kubelets_spelling() {
        let q = "container=web&command=ls&command=-l&stdin=true&stdout=true&stderr=true&tty=true";
        let got = stream_query(q, true);
        // Order is preserved, which matters: the command is argv.
        assert_eq!(got, "command=ls&command=-l&input=1&output=1&error=1&tty=1");
    }

    #[test]
    fn options_that_are_off_are_not_forwarded() {
        // `stdin=false` must not become `input=1` — the kubelet reads the
        // presence of the parameter, so forwarding it would attach a stream
        // the client is not going to write to, and the session would hang
        // waiting for it.
        assert_eq!(stream_query("stdin=false&stdout=true", true), "output=1");
    }

    #[test]
    fn attach_carries_no_command() {
        assert_eq!(
            stream_query("command=sh&stdout=true", false),
            "output=1",
            "attach has no command; forwarding one would be a different call"
        );
    }

    #[test]
    fn a_command_with_spaces_survives() {
        let got = stream_query("command=sh&command=-c&command=echo hi", true);
        assert_eq!(got, "command=sh&command=-c&command=echo+hi");
    }

    #[test]
    fn ports_become_repeated_port_parameters() {
        assert_eq!(portforward_query("ports=8080"), "port=8080");
        assert_eq!(portforward_query("ports=8080,9090"), "port=8080&port=9090");
        assert_eq!(portforward_query("port=443&ports=80"), "port=443&port=80");
    }

    #[test]
    fn the_container_is_required_when_there_is_more_than_one() {
        let pod = json!({"spec": {"containers": [{"name": "a"}, {"name": "b"}]}});
        let err = pick_container(&pod, "", "p").unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert!(err.message.contains("choose one of: [a, b]"), "{}", err.message);

        assert_eq!(pick_container(&pod, "container=b", "p").unwrap(), "b");
        assert!(pick_container(&pod, "container=zz", "p").is_err());
    }

    #[test]
    fn a_single_container_needs_no_naming() {
        let pod = json!({"spec": {"containers": [{"name": "only"}]}});
        assert_eq!(pick_container(&pod, "", "p").unwrap(), "only");
    }

    #[test]
    fn an_ephemeral_debug_container_is_a_valid_target() {
        // `kubectl debug` execs into a container that is not in spec.containers.
        let pod = json!({"spec": {
            "containers": [{"name": "app"}],
            "ephemeralContainers": [{"name": "debugger"}]}});
        assert_eq!(pick_container(&pod, "container=debugger", "p").unwrap(), "debugger");
    }

    #[test]
    fn response_heads_are_parsed_including_the_websocket_accept() {
        let head = b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\
                     Connection: Upgrade\r\nSec-WebSocket-Accept: abc=\r\n\r\n";
        let end = find_head_end(head).unwrap();
        assert_eq!(end, head.len());
        let (status, headers) = parse_head(&head[..end]).unwrap();
        assert_eq!(status, 101);
        assert_eq!(headers.get("upgrade").unwrap(), "websocket");
        assert_eq!(headers.get("sec-websocket-accept").unwrap(), "abc=");
    }

    #[test]
    fn bytes_after_the_head_are_kept() {
        let buf = b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: SPDY/3.1\r\n\r\n\x00\x01first-frame";
        let end = find_head_end(buf).unwrap();
        assert_eq!(&buf[end..], b"\x00\x01first-frame");
    }

    #[test]
    fn a_refusal_is_recognised_by_its_status() {
        let head = b"HTTP/1.1 404 Not Found\r\nContent-Length: 21\r\n\r\ncontainer not found\r\n";
        let end = find_head_end(head).unwrap();
        let (status, headers) = parse_head(&head[..end]).unwrap();
        assert_eq!(status, 404);
        assert_eq!(headers.get("content-length").unwrap(), "21");
    }
}
