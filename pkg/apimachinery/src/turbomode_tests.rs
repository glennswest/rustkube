//! Integration tests for the turbomode runtime (#143): the real reflector,
//! shared indexed feeds (`informers::Hub`) and reactive workers
//! (`reactor::WatchHub`) against a scripted API server, over real HTTP.
//!
//! The server answers LIST from a value the test sets and each WATCH from the
//! next scripted reply: a status code, an immediate EOF, or a stream the test
//! writes raw frames into. It counts LISTs, records every WATCH's
//! `resourceVersion` and how many watch streams are still open, so a test
//! can see sharing, resumption, relists and cancellation from the outside.
use crate::informer::Delta;
use crate::{informers, reactor};
use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

enum Watch {
    Status(u16),
    Eof,
    Stream(UnboundedReceiver<String>),
}

#[derive(Default)]
struct Api {
    list: Mutex<Value>,
    list_status: Mutex<Option<u16>>,
    watches: Mutex<VecDeque<Watch>>,
    lists: AtomicUsize,
    watch_rvs: Mutex<Vec<String>>,
    open: AtomicUsize,
}

impl Api {
    fn new(rv: &str, items: &[Value]) -> Arc<Self> {
        let api = Arc::new(Self::default());
        api.set_list(rv, items);
        api
    }
    fn set_list(&self, rv: &str, items: &[Value]) {
        *self.list.lock().unwrap() = json!({"metadata": {"resourceVersion": rv}, "items": items});
    }
    fn then(&self, watch: Watch) {
        self.watches.lock().unwrap().push_back(watch);
    }
    /// Script the next WATCH as a stream; frames sent here reach the client.
    fn stream(&self) -> UnboundedSender<String> {
        let (tx, rx) = unbounded_channel();
        self.then(Watch::Stream(rx));
        tx
    }
    fn watch_rvs(&self) -> Vec<String> {
        self.watch_rvs.lock().unwrap().clone()
    }
}

/// Counts a watch stream as open until the server drops its body.
struct Open(Arc<Api>);
impl Drop for Open {
    fn drop(&mut self) {
        self.0.open.fetch_sub(1, SeqCst);
    }
}

fn streamed(api: Arc<Api>, rx: Option<UnboundedReceiver<String>>) -> Response {
    api.open.fetch_add(1, SeqCst);
    let body = futures::stream::unfold((rx, Open(api)), |(mut rx, open)| async move {
        let frame = match rx.as_mut() {
            Some(rx) => rx.recv().await,
            // Unscripted: a healthy, quiet watch.
            None => std::future::pending().await,
        };
        frame.map(|f| (Ok::<_, std::io::Error>(bytes::Bytes::from(f)), (rx, open)))
    });
    axum::body::Body::from_stream(body).into_response()
}

async fn endpoint(
    State(api): State<Arc<Api>>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    if query.get("watch").map(String::as_str) == Some("true") {
        api.watch_rvs
            .lock()
            .unwrap()
            .push(query.get("resourceVersion").cloned().unwrap_or_default());
        let next = api.watches.lock().unwrap().pop_front();
        return match next {
            Some(Watch::Status(code)) => StatusCode::from_u16(code).unwrap().into_response(),
            Some(Watch::Eof) => "".into_response(),
            Some(Watch::Stream(rx)) => streamed(api, Some(rx)),
            None => streamed(api, None),
        };
    }
    api.lists.fetch_add(1, SeqCst);
    if let Some(code) = *api.list_status.lock().unwrap() {
        return StatusCode::from_u16(code).unwrap().into_response();
    }
    let list = api.list.lock().unwrap().clone();
    axum::Json(list).into_response()
}

async fn serve(api: &Arc<Api>) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/api/v1/pods", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/api/v1/pods", get(endpoint))
        .with_state(api.clone());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (url, task)
}

fn pod(name: &str, uid: &str, rv: &str, phase: &str) -> Value {
    json!({"metadata": {"name": name, "namespace": "ns", "uid": uid, "resourceVersion": rv},
           "status": {"phase": phase}})
}

fn frame(kind: &str, object: Value) -> String {
    format!("{}\n", json!({"type": kind, "object": object}))
}

/// What subscribers were told, as "ADD a" / "MOD a" / "DEL a".
#[derive(Clone, Default)]
struct Seen(Arc<Mutex<Vec<String>>>);
impl Seen {
    fn callback(&self) -> impl Fn(&[Delta], bool) + Send + Sync + 'static {
        let seen = self.0.clone();
        move |changes: &[Delta], _reset: bool| {
            for change in changes {
                let (verb, object) = match (&change.old, &change.new) {
                    (None, Some(new)) => ("ADD", new),
                    (Some(_), Some(new)) => ("MOD", new),
                    (Some(old), None) => ("DEL", old),
                    (None, None) => continue,
                };
                let name = object["metadata"]["name"].as_str().unwrap_or("?");
                seen.lock().unwrap().push(format!("{verb} {name}"));
            }
        }
    }
    fn get(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
    fn has(&self, entry: &str) -> bool {
        self.get().iter().any(|e| e == entry)
    }
    fn clear(&self) {
        self.0.lock().unwrap().clear();
    }
}

/// Test-side wait for an observable condition (the code under test does not
/// poll; the test has to look from outside).
async fn eventually(what: &str, cond: impl Fn() -> bool) {
    let result = tokio::time::timeout(Duration::from_secs(10), async {
        while !cond() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(result.is_ok(), "timed out waiting for: {what}");
}

fn names(feed: &informers::Feed) -> Vec<String> {
    let mut names: Vec<String> = feed
        .list()
        .unwrap()
        .iter()
        .map(|o| o["metadata"]["name"].as_str().unwrap().to_string())
        .collect();
    names.sort();
    names
}

#[tokio::test]
async fn subscribers_share_one_list_and_watch_and_a_late_one_is_seeded() {
    let api = Api::new("10", &[pod("a", "ua", "10", "Pending")]);
    let tx = api.stream();
    let (url, server) = serve(&api).await;
    let client = reqwest::Client::new();
    let hub = informers::Hub::default();

    let first = Seen::default();
    let s1 = hub.subscribe(&client, url.clone(), first.callback());
    eventually("first subscriber synced", || s1.feed.ensure_synced().is_ok()).await;
    assert_eq!(first.get(), ["ADD a"]);

    // Joins the running feed: seeded synchronously, no second LIST or WATCH.
    let late = Seen::default();
    let s2 = hub.subscribe(&client, url.clone(), late.callback());
    assert_eq!(late.get(), ["ADD a"]);
    assert!(Arc::ptr_eq(&s1.feed, &s2.feed));

    // A revision-only echo is no change; a status change and a create are.
    tx.send(frame("MODIFIED", pod("a", "ua", "11", "Pending"))).unwrap();
    tx.send(frame("MODIFIED", pod("a", "ua", "12", "Running"))).unwrap();
    tx.send(frame("ADDED", pod("b", "ub", "13", "Pending"))).unwrap();
    eventually("both subscribers see b", || first.has("ADD b") && late.has("ADD b")).await;
    assert_eq!(first.get(), ["ADD a", "MOD a", "ADD b"]);
    assert_eq!(late.get(), ["ADD a", "MOD a", "ADD b"]);
    assert_eq!(names(&s1.feed), ["a", "b"]);

    assert_eq!(api.lists.load(SeqCst), 1);
    assert_eq!(api.watch_rvs(), ["10"]);
    assert_eq!(api.open.load(SeqCst), 1);
    server.abort();
}

#[tokio::test]
async fn a_watch_outage_is_unsynchronized_not_empty_and_resumes_without_a_list() {
    let api = Api::new("10", &[pod("a", "ua", "10", "Pending")]);
    let first = api.stream();
    api.then(Watch::Status(503));
    api.then(Watch::Status(503));
    let second = api.stream();
    let (url, server) = serve(&api).await;
    let hub = informers::Hub::default();
    let seen = Seen::default();
    let sub = hub.subscribe(&reqwest::Client::new(), url, seen.callback());
    eventually("synced", || sub.feed.ensure_synced().is_ok()).await;

    first.send(frame("ADDED", pod("b", "ub", "11", "Pending"))).unwrap();
    eventually("b applied", || seen.has("ADD b")).await;
    // The connection drops; two reconnects are refused.
    drop(first);
    eventually("outage observed", || sub.feed.ensure_synced().is_err()).await;
    assert!(sub.feed.list().is_err(), "an outage never reads as an empty collection");
    assert!(sub.feed.select(&crate::informer::Index::Namespace("ns".into())).is_err());

    eventually("reconnected", || sub.feed.ensure_synced().is_ok()).await;
    second.send(frame("ADDED", pod("c", "uc", "12", "Pending"))).unwrap();
    eventually("c applied", || seen.has("ADD c")).await;
    assert_eq!(names(&sub.feed), ["a", "b", "c"]);
    // Every reconnect resumed from the last applied revision; no relist.
    assert_eq!(api.watch_rvs(), ["10", "11", "11", "11"]);
    assert_eq!(api.lists.load(SeqCst), 1);
    server.abort();
}

#[tokio::test]
async fn an_expired_watch_relists_and_reports_what_changed_in_the_gap() {
    let api = Api::new(
        "10",
        &[pod("a", "ua", "10", "Pending"), pod("b", "ub", "10", "Pending")],
    );
    let tx = api.stream();
    let (url, server) = serve(&api).await;
    let hub = informers::Hub::default();
    let seen = Seen::default();
    let sub = hub.subscribe(&reqwest::Client::new(), url, seen.callback());
    eventually("synced", || sub.feed.ensure_synced().is_ok()).await;
    seen.clear();

    // While the watch fell behind: b deleted, c created, a untouched.
    api.set_list(
        "20",
        &[pod("a", "ua", "10", "Pending"), pod("c", "uc", "15", "Pending")],
    );
    tx.send(frame(
        "ERROR",
        json!({"kind": "Status", "code": 410, "reason": "Expired", "message": "too old"}),
    ))
    .unwrap();
    eventually("relisted", || seen.has("ADD c")).await;
    eventually("synced again", || sub.feed.ensure_synced().is_ok()).await;
    let mut deltas = seen.get();
    deltas.sort();
    assert_eq!(deltas, ["ADD c", "DEL b"]);
    assert_eq!(names(&sub.feed), ["a", "c"]);
    assert_eq!(api.lists.load(SeqCst), 2);
    eventually("watch resumed", || api.watch_rvs().len() == 2).await;
    assert_eq!(api.watch_rvs(), ["10", "20"]);
    server.abort();
}

#[tokio::test]
async fn a_list_outage_fails_closed_and_backs_off() {
    let api = Api::new("10", &[pod("a", "ua", "10", "Pending")]);
    *api.list_status.lock().unwrap() = Some(500);
    let (url, server) = serve(&api).await;
    let hub = informers::Hub::default();
    let seen = Seen::default();
    let sub = hub.subscribe(&reqwest::Client::new(), url, seen.callback());

    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(sub.feed.list().is_err(), "a failed LIST is not an empty collection");
    assert!(seen.get().is_empty());
    // 100, 200, 400 ms apart: four attempts in the first second, not a hot loop.
    let attempts = api.lists.load(SeqCst);
    assert!((2..=5).contains(&attempts), "{attempts} LISTs in 1 s");
    assert!(api.watch_rvs().is_empty(), "no WATCH without a snapshot");

    *api.list_status.lock().unwrap() = None;
    eventually("recovered", || sub.feed.ensure_synced().is_ok()).await;
    assert_eq!(seen.get(), ["ADD a"]);
    server.abort();
}

#[tokio::test]
async fn watches_that_end_at_once_back_off_instead_of_hot_looping() {
    let api = Api::new("10", &[]);
    for _ in 0..100 {
        api.then(Watch::Eof);
    }
    let (url, server) = serve(&api).await;
    let hub = informers::Hub::default();
    let _sub = hub.subscribe(&reqwest::Client::new(), url, |_: &[Delta], _| {});
    tokio::time::sleep(Duration::from_secs(1)).await;
    let watches = api.watch_rvs().len();
    assert!((2..=5).contains(&watches), "{watches} WATCHes in 1 s");
    assert!(api.watch_rvs().iter().all(|rv| rv == "10"));
    assert_eq!(api.lists.load(SeqCst), 1, "an empty stream is not a reason to relist");
    server.abort();
}

#[tokio::test]
async fn a_malformed_frame_forces_a_relist() {
    let api = Api::new("10", &[pod("a", "ua", "10", "Pending")]);
    let tx = api.stream();
    let (url, server) = serve(&api).await;
    let hub = informers::Hub::default();
    let sub = hub.subscribe(&reqwest::Client::new(), url, |_: &[Delta], _| {});
    eventually("synced", || sub.feed.ensure_synced().is_ok()).await;
    // A frame split across chunks is fine; garbage is not.
    tx.send("{\"type\":\"ADDED\",".into()).unwrap();
    tx.send(format!("\"object\":{}}}\n", pod("b", "ub", "11", "Pending")))
        .unwrap();
    eventually("split frame applied", || {
        sub.feed.list().is_ok_and(|items| items.len() == 2)
    })
    .await;
    tx.send("not json\n".into()).unwrap();
    eventually("relisted", || api.lists.load(SeqCst) == 2).await;
    eventually("watch reopened", || api.watch_rvs().len() == 2).await;
    assert_eq!(api.watch_rvs(), ["10", "10"]);
    eventually("synced from the relist", || sub.feed.ensure_synced().is_ok()).await;
    // The relist is authoritative: b was never in the LIST.
    assert_eq!(names(&sub.feed), ["a"]);
    server.abort();
}

#[tokio::test]
async fn the_last_subscriber_leaving_closes_the_watch() {
    let api = Api::new("10", &[]);
    let (url, server) = serve(&api).await;
    let client = reqwest::Client::new();
    let hub = informers::Hub::default();
    let s1 = hub.subscribe(&client, url.clone(), |_: &[Delta], _| {});
    let s2 = hub.subscribe(&client, url.clone(), |_: &[Delta], _| {});
    eventually("watch open", || api.open.load(SeqCst) == 1).await;

    drop(s1);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(api.open.load(SeqCst), 1, "one subscriber still needs it");

    drop(s2);
    eventually("watch closed", || api.open.load(SeqCst) == 0).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(api.lists.load(SeqCst), 1);
    assert_eq!(api.watch_rvs().len(), 1, "a cancelled feed does not reconnect");

    // A new subscriber starts a fresh feed.
    let s3 = hub.subscribe(&client, url, |_: &[Delta], _| {});
    eventually("fresh feed synced", || s3.feed.ensure_synced().is_ok()).await;
    assert_eq!(api.lists.load(SeqCst), 2);
    server.abort();
}

/// Events that arrive while a pass runs cannot be consumed by that pass:
/// they buy exactly one more. An idle watch buys nothing.
#[tokio::test]
async fn events_during_a_pass_run_exactly_one_more_and_an_idle_watch_none() {
    let api = Api::new("10", &[pod("a", "ua", "10", "Pending")]);
    let tx = api.stream();
    let (url, server) = serve(&api).await;
    let client = reqwest::Client::new();
    let hub = reactor::WatchHub::default();
    let worker = hub.worker("test");
    let idle = &worker;
    let quiet = move || async move {
        tokio::time::timeout(Duration::from_millis(300), idle.next())
            .await
            .is_err()
    };

    // Pass 1 registers the watch; its initial LIST lands during the pass.
    let work = worker.next().await;
    worker
        .run(async {
            hub.observe(&client, url.clone());
            eventually("watch open", || api.open.load(SeqCst) == 1).await;
        })
        .await;
    drop(work);

    // Pass 2: two changes arrive mid-pass.
    let work = tokio::time::timeout(Duration::from_secs(1), worker.next())
        .await
        .expect("the LIST during pass 1 buys pass 2");
    worker
        .run(async {
            tx.send(frame("MODIFIED", pod("a", "ua", "11", "Running"))).unwrap();
            tx.send(frame("ADDED", pod("b", "ub", "12", "Pending"))).unwrap();
            tokio::time::sleep(Duration::from_millis(200)).await;
        })
        .await;
    drop(work);

    // Pass 3, once, for both.
    let work = tokio::time::timeout(Duration::from_secs(1), worker.next())
        .await
        .expect("changes during pass 2 buy pass 3");
    worker.run(async {}).await;
    drop(work);
    assert!(quiet().await, "two events, one extra pass");

    // A revision-only echo and bookmarks wake nobody.
    tx.send(frame("MODIFIED", pod("b", "ub", "13", "Pending"))).unwrap();
    tx.send(frame("BOOKMARK", json!({"metadata": {"resourceVersion": "14"}})))
        .unwrap();
    assert!(quiet().await, "no semantic change, no pass");

    // A change after the pass wakes the idle worker.
    tx.send(frame("DELETED", pod("b", "ub", "15", "Pending"))).unwrap();
    let work = tokio::time::timeout(Duration::from_secs(1), worker.next())
        .await
        .expect("an event wakes an idle worker");
    worker.run(async {}).await;
    drop(work);
    assert!(quiet().await);

    // The worker going away (a lost leadership term) closes its watch.
    assert_eq!(api.open.load(SeqCst), 1);
    drop(worker);
    eventually("watch closed with its worker", || api.open.load(SeqCst) == 0).await;
    assert_eq!(api.lists.load(SeqCst), 1);
    assert_eq!(api.watch_rvs(), ["10"]);
    server.abort();
}
