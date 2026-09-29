//! Revisioned Kubernetes LIST/WATCH transport shared by reactive components.
use futures::StreamExt;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::Duration;

const MAX_FRAME: usize = 16 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Read every page. Error Status, malformed lists and inconsistent page
/// revisions fail closed rather than representing an empty collection.
pub async fn list(client: &reqwest::Client, url: &str) -> anyhow::Result<Value> {
    let mut items = Vec::new();
    let mut token = String::new();
    let mut seen = std::collections::HashSet::new();
    let mut revision: Option<String> = None;
    loop {
        let mut request = client
            .get(url)
            .timeout(REQUEST_TIMEOUT)
            .query(&[("limit", "500")]);
        if !token.is_empty() {
            request = request.query(&[("continue", &token)]);
        }
        let page: Value = request.send().await?.error_for_status()?.json().await?;
        let page_items = page["items"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("LIST has no items array: {url}"))?;
        let rv = page["metadata"]["resourceVersion"]
            .as_str()
            .filter(|v| !v.is_empty())
            .ok_or_else(|| anyhow::anyhow!("LIST has no resourceVersion: {url}"))?;
        if let Some(first) = &revision {
            anyhow::ensure!(first == rv, "LIST revision changed between pages: {url}");
        } else {
            revision = Some(rv.to_string());
        }
        items.extend(page_items.iter().cloned());
        token = page["metadata"]["continue"]
            .as_str()
            .unwrap_or("")
            .to_string();
        if token.is_empty() {
            break;
        }
        anyhow::ensure!(
            seen.insert(token.clone()),
            "LIST repeated continue token: {url}"
        );
    }
    Ok(json!({"metadata": {"resourceVersion": revision.unwrap_or_default()}, "items": items}))
}

/// Normalize transport-only fields. Status remains significant.
fn semantic(mut value: Value) -> Value {
    if let Some(meta) = value.get_mut("metadata").and_then(Value::as_object_mut) {
        meta.remove("resourceVersion");
        meta.remove("managedFields");
    }
    // Condition timestamps describe when a transition was reported. They
    // are not a new transition when type/status/reason/message are identical.
    // Do not strip lease renewTime, startTime, or any semantic deadline.
    fn conditions(value: &mut Value) {
        match value {
            Value::Object(map) => {
                if let Some(Value::Array(items)) = map.get_mut("conditions") {
                    for item in items {
                        if let Some(c) = item.as_object_mut() {
                            c.remove("lastTransitionTime");
                            c.remove("lastUpdateTime");
                            c.remove("lastHeartbeatTime");
                        }
                    }
                }
                for child in map.values_mut() {
                    conditions(child);
                }
            }
            Value::Array(items) => {
                for child in items {
                    conditions(child);
                }
            }
            _ => {}
        }
    }
    conditions(&mut value);
    value
}

fn identity(obj: &Value) -> anyhow::Result<String> {
    let name = obj["metadata"]["name"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("watch object has no name"))?;
    let namespace = obj["metadata"]["namespace"].as_str().unwrap_or("");
    Ok(format!("{namespace}/{name}"))
}

#[derive(Default)]
struct Observed {
    revision: String,
    objects: HashMap<String, Value>,
}
impl Observed {
    fn replace(&mut self, list: Value) -> anyhow::Result<()> {
        let mut objects = HashMap::new();
        for obj in list["items"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("missing items"))?
        {
            objects.insert(identity(obj)?, semantic(obj.clone()));
        }
        self.objects = objects;
        self.revision = list["metadata"]["resourceVersion"]
            .as_str()
            .unwrap_or("")
            .to_string();
        Ok(())
    }

    fn apply(&mut self, event: Value) -> anyhow::Result<bool> {
        let obj = &event["object"];
        let kind = event["type"].as_str().unwrap_or("");
        anyhow::ensure!(kind != "ERROR", "watch error: {obj}");
        let rv = obj["metadata"]["resourceVersion"]
            .as_str()
            .filter(|rv| !rv.is_empty())
            .ok_or_else(|| anyhow::anyhow!("watch event has no resourceVersion"))?;
        if kind == "BOOKMARK" {
            self.revision = rv.to_string();
            return Ok(false);
        }
        let key = identity(obj)?;
        let changed = match kind {
            "ADDED" | "MODIFIED" => {
                let normalized = semantic(obj.clone());
                let changed = self.objects.get(&key) != Some(&normalized);
                self.objects.insert(key, normalized);
                changed
            }
            "DELETED" => {
                self.objects.remove(&key);
                true
            }
            _ => anyhow::bail!("unknown watch event type: {kind}"),
        };
        self.revision = rv.to_string();
        Ok(changed)
    }
}

/// Partial frames are retained across chunks; oversized frames fail rather
/// than growing memory indefinitely. Malformed input forces a recovery LIST.
#[derive(Default)]
struct Frames {
    pending: Vec<u8>,
}
impl Frames {
    fn push(&mut self, chunk: &[u8]) -> anyhow::Result<Vec<Value>> {
        let mut events = Vec::new();
        for part in chunk.split_inclusive(|b| *b == b'\n') {
            anyhow::ensure!(
                self.pending.len() + part.len() <= MAX_FRAME,
                "watch frame exceeds limit"
            );
            self.pending.extend_from_slice(part);
            if part.last() == Some(&b'\n') {
                if !self.pending.iter().all(u8::is_ascii_whitespace) {
                    events.push(serde_json::from_slice(&self.pending)?);
                }
                self.pending.clear();
            }
        }
        Ok(events)
    }
}

/// A watch event or recovery snapshot invokes `changed` synchronously. The
/// callback should only enqueue; it must not run a reconcile or spawn a task.
/// Drop the owning task to cancel the stream and all retries.
pub async fn run(client: reqwest::Client, url: String, changed: impl Fn() + Send + Sync) {
    let mut observed = Observed::default();
    let mut retry = Duration::from_millis(100);
    loop {
        if observed.revision.is_empty() {
            match list(&client, &url).await.and_then(|v| observed.replace(v)) {
                Ok(()) => {
                    changed();
                    retry = Duration::from_millis(100);
                }
                Err(error) => {
                    tracing::warn!(%url, %error, "reflector LIST failed");
                    tokio::time::sleep(retry).await;
                    retry = (retry * 2).min(Duration::from_secs(30));
                    continue;
                }
            }
        }
        let started = tokio::time::Instant::now();
        let result = watch(&client, &url, &mut observed, &changed).await;
        match result {
            Ok(()) => {
                // A healthy server-side timeout resumes immediately. Repeated
                // immediate EOFs are a transport failure and must not hot loop.
                if started.elapsed() > Duration::from_secs(1) {
                    retry = Duration::from_millis(100);
                    continue;
                }
            }
            Err(error) => tracing::warn!(%url, %error, "reflector WATCH reconnecting"),
        }
        metrics::counter!("rustkube_watch_reconnects_total").increment(1);
        tokio::time::sleep(retry).await;
        retry = (retry * 2).min(Duration::from_secs(30));
    }
}

async fn watch(
    client: &reqwest::Client,
    url: &str,
    observed: &mut Observed,
    changed: &(impl Fn() + Send + Sync),
) -> anyhow::Result<()> {
    let response = client
        .get(url)
        // Bounds both opening and an unresponsive stream. A watch which
        // expires normally reconnects from its last revision, without a LIST.
        .timeout(Duration::from_secs(330))
        .query(&[
            ("watch", "true"),
            ("allowWatchBookmarks", "true"),
            ("timeoutSeconds", "300"),
            ("resourceVersion", &observed.revision),
        ])
        .send()
        .await?;
    if response.status().as_u16() == 410 {
        observed.revision.clear();
        anyhow::bail!("watch revision expired");
    }
    let mut stream = response.error_for_status()?.bytes_stream();
    let mut frames = Frames::default();
    while let Some(chunk) = stream.next().await {
        let events = match frames.push(&chunk?) {
            Ok(events) => events,
            Err(error) => {
                observed.revision.clear();
                return Err(error);
            }
        };
        for event in events {
            match observed.apply(event) {
                Ok(true) => changed(),
                Ok(false) => {}
                Err(error) => {
                    observed.revision.clear();
                    return Err(error);
                }
            }
        }
    }
    // Do not advance revision on a partial frame; reconnect replays it.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn split_chunks_and_multiple_frames() {
        let mut f = Frames::default();
        assert!(f.push(b"{\"type\":").unwrap().is_empty());
        let values = f.push(b"\"BOOKMARK\"}\n{\"type\":\"ADDED\"}\n").unwrap();
        assert_eq!(values.len(), 2);
        assert_eq!(values[0]["type"], "BOOKMARK");
        assert!(f.push(b"not json\n").is_err());
    }
    #[test]
    fn status_changes_and_uid_recreation_are_significant_but_rv_echoes_are_not() {
        let mut o = Observed::default();
        let mut pod = json!({"metadata":{"name":"p", "uid":"one", "resourceVersion":"1"}, "status":{"phase":"Pending"}});
        assert!(o.apply(json!({"type":"ADDED","object":pod})).unwrap());
        pod["metadata"]["resourceVersion"] = json!("2");
        assert!(!o.apply(json!({"type":"MODIFIED","object":pod})).unwrap());
        pod["status"]["phase"] = json!("Running");
        assert!(o.apply(json!({"type":"MODIFIED","object":pod})).unwrap());
        pod["metadata"]["uid"] = json!("two");
        assert!(o.apply(json!({"type":"ADDED","object":pod})).unwrap());
        assert!(o.apply(json!({"type":"DELETED","object":pod})).unwrap());
        assert!(o.objects.is_empty());
    }
}

#[cfg(test)]
mod transport_tests {
    use super::*;
    use axum::{
        extract::{Query, State},
        response::{IntoResponse, Response},
        routing::get,
        Router,
    };
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    };

    #[derive(Default)]
    struct Server {
        lists: AtomicUsize,
        watches: AtomicUsize,
        revisions: Mutex<Vec<String>>,
    }
    async fn endpoint(
        State(state): State<Arc<Server>>,
        Query(query): Query<HashMap<String, String>>,
    ) -> Response {
        if query.get("watch").map(String::as_str) == Some("true") {
            state
                .revisions
                .lock()
                .unwrap()
                .push(query.get("resourceVersion").cloned().unwrap_or_default());
            let attempt = state.watches.fetch_add(1, Ordering::SeqCst);
            return match attempt {
                0 => "{\"type\":\"ADDED\",\"object\":{\"metadata\":{\"name\":\"p\",\"uid\":\"u\",\"resourceVersion\":\"11\"}}}\n".into_response(),
                1 => axum::http::StatusCode::GONE.into_response(),
                _ => {
                    let body = futures::stream::pending::<Result<bytes::Bytes, std::io::Error>>();
                    axum::body::Body::from_stream(body).into_response()
                },
            };
        }
        let n = state.lists.fetch_add(1, Ordering::SeqCst);
        axum::Json(json!({"metadata":{"resourceVersion":if n == 0 {"10"} else {"12"}},"items":[]}))
            .into_response()
    }

    async fn serve(app: Router) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/pods", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (url, task)
    }

    #[tokio::test]
    async fn reconnect_resumes_revision_and_410_relists() {
        let state = Arc::new(Server::default());
        let (url, server) = serve(
            Router::new()
                .route("/pods", get(endpoint))
                .with_state(state.clone()),
        )
        .await;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(run(reqwest::Client::new(), url, move || {
            let _ = tx.send(());
        }));
        tokio::time::timeout(Duration::from_secs(3), async {
            // Initial LIST, event in the LIST/WATCH gap, 410 recovery LIST.
            for _ in 0..3 {
                rx.recv().await.unwrap();
            }
        })
        .await
        .unwrap();
        assert_eq!(state.lists.load(Ordering::SeqCst), 2);
        let revisions = state.revisions.lock().unwrap().clone();
        assert_eq!(&revisions[..2], &["10", "11"]);
        task.abort();
        server.abort();
    }

    #[tokio::test]
    async fn pagination_is_complete_and_error_status_is_not_empty() {
        async fn pages(Query(query): Query<HashMap<String, String>>) -> Response {
            if let Some(token) = query.get("continue") {
                assert_eq!(token, "a+/=");
                axum::Json(
                    json!({"metadata":{"resourceVersion":"r"},"items":[{"metadata":{"name":"b"}}]}),
                )
                .into_response()
            } else {
                axum::Json(json!({"metadata":{"resourceVersion":"r","continue":"a+/="},"items":[{"metadata":{"name":"a"}}]})).into_response()
            }
        }
        let (url, server) = serve(Router::new().route("/pods", get(pages))).await;
        let value = list(&reqwest::Client::new(), &url).await.unwrap();
        assert_eq!(value["items"].as_array().unwrap().len(), 2);
        server.abort();
        let (url, server) = serve(
            Router::new().route("/pods", get(|| async { axum::http::StatusCode::FORBIDDEN })),
        )
        .await;
        assert!(list(&reqwest::Client::new(), &url).await.is_err());
        server.abort();
    }
}
