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
pub(crate) fn semantic(mut value: Value) -> Value {
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
/// It returns whether it could apply the change: a snapshot or event its
/// consumer rejected forces a recovery LIST, because the consumer's view is
/// no longer complete and nothing else would ever repair it.
/// Drop the owning task to cancel the stream and all retries.
#[derive(Clone, Debug)]
pub enum Change {
    /// Complete snapshot, never a partial LIST or an error interpreted as empty.
    Reset {
        snapshot: Value,
        started: std::time::Instant,
    },
    Connected,
    /// Every revision is delivered, even when only transport fields changed.
    Applied {
        event: Value,
        semantic_change: bool,
    },
    Unavailable,
}

pub async fn run(client: reqwest::Client, url: String, changed: impl Fn() + Send + Sync) {
    run_events(client, url, move |event| {
        match event {
            Change::Reset { .. }
            | Change::Applied {
                semantic_change: true,
                ..
            } => changed(),
            _ => {}
        }
        true
    })
    .await;
}

pub async fn run_events(
    client: reqwest::Client,
    url: String,
    changed: impl Fn(Change) -> bool + Send + Sync,
) {
    run_events_with(client, url, changed, WatchTiming::default()).await;
}

/// How long one WATCH lasts. The server is asked to end it at `server`
/// (`timeoutSeconds`); the reflector ends it itself at `client` if the
/// server has not, so a silent connection cannot hang forever. Either end
/// is routine: the watch resumes from its last revision at once.
#[derive(Clone, Copy, Debug)]
pub(crate) struct WatchTiming {
    server: Duration,
    client: Duration,
}
impl Default for WatchTiming {
    fn default() -> Self {
        Self {
            server: Duration::from_secs(300),
            client: Duration::from_secs(330),
        }
    }
}

/// How a WATCH that did not fail ended.
enum Ended {
    /// The server closed the stream.
    Closed,
    /// The reflector's own deadline passed with the stream still open.
    Deadline,
}

pub(crate) async fn run_events_with(
    client: reqwest::Client,
    url: String,
    changed: impl Fn(Change) -> bool + Send + Sync,
    timing: WatchTiming,
) {
    let mut observed = Observed::default();
    let mut retry = Duration::from_millis(100);
    loop {
        if observed.revision.is_empty() {
            let list_started = std::time::Instant::now();
            match list(&client, &url).await.and_then(|v| {
                observed.replace(v.clone())?;
                Ok(v)
            }) {
                Ok(snapshot) => {
                    if !changed(Change::Reset {
                        snapshot,
                        started: list_started,
                    }) {
                        observed.revision.clear();
                        tracing::warn!(%url, "reflector snapshot rejected; relisting");
                        tokio::time::sleep(retry).await;
                        retry = (retry * 2).min(Duration::from_secs(30));
                        continue;
                    }
                    retry = Duration::from_millis(100);
                }
                Err(error) => {
                    changed(Change::Unavailable);
                    tracing::warn!(%url, %error, "reflector LIST failed");
                    tokio::time::sleep(retry).await;
                    retry = (retry * 2).min(Duration::from_secs(30));
                    continue;
                }
            }
        }
        let started = tokio::time::Instant::now();
        let result = watch(&client, &url, &mut observed, &changed, timing).await;
        match result {
            // The stream was healthy until our own deadline (#207): a server
            // that overran `timeoutSeconds` (#165), not an outage. Resume
            // from the last revision with the feed still synchronized.
            Ok(Ended::Deadline) => {
                tracing::debug!(%url, revision = %observed.revision, "reflector WATCH deadline; resuming");
                retry = Duration::from_millis(100);
                continue;
            }
            Ok(Ended::Closed) => {
                // A healthy server-side timeout resumes immediately. Repeated
                // immediate EOFs are a transport failure and must not hot loop.
                if started.elapsed() > Duration::from_secs(1) {
                    retry = Duration::from_millis(100);
                    continue;
                }
            }
            Err(error) => tracing::warn!(%url, %error, "reflector WATCH reconnecting"),
        }
        changed(Change::Unavailable);
        metrics::counter!("rustkube_watch_reconnects_total").increment(1);
        tokio::time::sleep(retry).await;
        retry = (retry * 2).min(Duration::from_secs(30));
    }
}

async fn watch(
    client: &reqwest::Client,
    url: &str,
    observed: &mut Observed,
    changed: &(impl Fn(Change) -> bool + Send + Sync),
    timing: WatchTiming,
) -> anyhow::Result<Ended> {
    // One deadline bounds both opening and an unresponsive stream. Failing
    // to open by then is an outage; an open stream reaching it is a routine
    // end, told apart from a transport error (#207).
    let deadline = tokio::time::Instant::now() + timing.client;
    let server_timeout = timing.server.as_secs().max(1).to_string();
    let request = client.get(url).query(&[
        ("watch", "true"),
        ("allowWatchBookmarks", "true"),
        ("timeoutSeconds", server_timeout.as_str()),
        ("resourceVersion", &observed.revision),
    ]);
    let response = tokio::time::timeout_at(deadline, request.send())
        .await
        .map_err(|_| anyhow::anyhow!("watch did not open within {:?}", timing.client))??;
    if response.status().as_u16() == 410 {
        observed.revision.clear();
        anyhow::bail!("watch revision expired");
    }
    let mut stream = response.error_for_status()?.bytes_stream();
    changed(Change::Connected);
    let mut frames = Frames::default();
    loop {
        let chunk = match tokio::time::timeout_at(deadline, stream.next()).await {
            Err(_) => return Ok(Ended::Deadline),
            Ok(None) => break,
            Ok(Some(chunk)) => chunk,
        };
        let events = match frames.push(&chunk?) {
            Ok(events) => events,
            Err(error) => {
                observed.revision.clear();
                return Err(error);
            }
        };
        for event in events {
            match observed.apply(event.clone()) {
                Ok(semantic_change) => {
                    if !changed(Change::Applied {
                        event,
                        semantic_change,
                    }) {
                        observed.revision.clear();
                        anyhow::bail!("watch event rejected by its consumer; relisting");
                    }
                }
                Err(error) => {
                    observed.revision.clear();
                    return Err(error);
                }
            }
        }
    }
    // Do not advance revision on a partial frame; reconnect replays it.
    Ok(Ended::Closed)
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

    /// A consumer that cannot apply an event (its view is now incomplete)
    /// gets a recovery LIST, and the watch resumes from that LIST's revision
    /// rather than from the rejected event's.
    #[tokio::test]
    async fn a_rejected_event_forces_a_relist() {
        let state = Arc::new(Server::default());
        let (url, server) = serve(
            Router::new()
                .route("/pods", get(endpoint))
                .with_state(state.clone()),
        )
        .await;
        let task = tokio::spawn(run_events(reqwest::Client::new(), url, |change| {
            !matches!(change, Change::Applied { .. })
        }));
        tokio::time::timeout(Duration::from_secs(3), async {
            while state.revisions.lock().unwrap().len() < 2 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(state.lists.load(Ordering::SeqCst) >= 2);
        assert_eq!(&state.revisions.lock().unwrap()[..2], &["10", "12"]);
        task.abort();
        server.abort();
    }

    /// The reflector's own deadline on a stream the server never closes
    /// (#207, server side #165) is a routine end: it resumes from the last
    /// revision at once, with no LIST, no `Unavailable` and no backoff.
    #[tokio::test]
    async fn own_deadline_resumes_without_an_outage() {
        async fn overrun(
            State(state): State<Arc<Server>>,
            Query(query): Query<HashMap<String, String>>,
        ) -> Response {
            if query.get("watch").map(String::as_str) != Some("true") {
                state.lists.fetch_add(1, Ordering::SeqCst);
                return axum::Json(json!({"metadata":{"resourceVersion":"10"},"items":[]}))
                    .into_response();
            }
            assert_eq!(query.get("timeoutSeconds").map(String::as_str), Some("1"));
            state
                .revisions
                .lock()
                .unwrap()
                .push(query.get("resourceVersion").cloned().unwrap_or_default());
            let first = state.watches.fetch_add(1, Ordering::SeqCst) == 0;
            // Ignores timeoutSeconds: never closes. The first carries an event.
            let head = if first {
                "{\"type\":\"ADDED\",\"object\":{\"metadata\":{\"name\":\"p\",\"uid\":\"u\",\"resourceVersion\":\"11\"}}}\n"
            } else {
                "{\"type\":\"BOOKMARK\",\"object\":{\"metadata\":{\"resourceVersion\":\"11\"}}}\n"
            };
            let body = futures::stream::once(async move {
                Ok::<_, std::io::Error>(bytes::Bytes::from_static(head.as_bytes()))
            })
            .chain(futures::stream::pending());
            axum::body::Body::from_stream(body).into_response()
        }
        let state = Arc::new(Server::default());
        let (url, server) = serve(
            Router::new()
                .route("/pods", get(overrun))
                .with_state(state.clone()),
        )
        .await;
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        let timing = WatchTiming {
            server: Duration::from_secs(1),
            client: Duration::from_millis(200),
        };
        let started = std::time::Instant::now();
        let task = tokio::spawn(run_events_with(
            reqwest::Client::new(),
            url,
            move |change| {
                log.lock().unwrap().push(match change {
                    Change::Reset { .. } => "reset",
                    Change::Connected => "connected",
                    Change::Applied { .. } => "applied",
                    Change::Unavailable => "unavailable",
                });
                true
            },
            timing,
        ));
        tokio::time::timeout(Duration::from_secs(5), async {
            while state.watches.load(Ordering::SeqCst) < 5 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        // Five watches of 200 ms each: resumed at once, no 100 ms+ backoff
        // growing between them.
        assert!(started.elapsed() < Duration::from_millis(1500));
        task.abort();
        server.abort();
        let seen = seen.lock().unwrap().clone();
        assert!(!seen.contains(&"unavailable"), "{seen:?}");
        assert_eq!(seen.iter().filter(|c| **c == "reset").count(), 1, "{seen:?}");
        assert_eq!(state.lists.load(Ordering::SeqCst), 1);
        let revisions = state.revisions.lock().unwrap().clone();
        assert_eq!(&revisions[..3], &["10", "11", "11"]);
    }

    /// A watch that does not even open by the deadline is an outage.
    #[tokio::test]
    async fn a_watch_that_never_opens_is_unavailable() {
        async fn silent(Query(query): Query<HashMap<String, String>>) -> Response {
            if query.get("watch").map(String::as_str) == Some("true") {
                futures::future::pending::<()>().await;
            }
            axum::Json(json!({"metadata":{"resourceVersion":"10"},"items":[]})).into_response()
        }
        let (url, server) = serve(Router::new().route("/pods", get(silent))).await;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let timing = WatchTiming {
            server: Duration::from_secs(1),
            client: Duration::from_millis(100),
        };
        let task = tokio::spawn(run_events_with(
            reqwest::Client::new(),
            url,
            move |change| {
                if matches!(change, Change::Unavailable | Change::Connected) {
                    let _ = tx.send(matches!(change, Change::Unavailable));
                }
                true
            },
            timing,
        ));
        let unavailable = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(unavailable, "a watch that never opened reported Connected");
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
