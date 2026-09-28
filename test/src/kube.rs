//! The apiserver, with the run's ServiceAccount token, in the run's own
//! namespace. Everything the suites create carries `storm.io/test-run=<id>`.

use crate::env::Env;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

pub const JSON: &str = "application/json";
pub const MERGE: &str = "application/merge-patch+json";
pub const STRATEGIC: &str = "application/strategic-merge-patch+json";
pub const JSON_PATCH: &str = "application/json-patch+json";
pub const APPLY: &str = "application/apply-patch+yaml";

#[derive(Clone)]
pub struct Kube {
    http: reqwest::Client,
    base: String,
    token: String,
    pub ns: String,
    pub run: String,
}

/// A response: its HTTP status and body (JSON, or the text as a string).
pub struct Resp {
    pub status: u16,
    pub body: Value,
}

impl Resp {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

impl Kube {
    pub fn new(env: &Env) -> Result<Kube> {
        let ca = reqwest::Certificate::from_pem(&env.ca).context("the service-account ca.crt is not PEM")?;
        let http = reqwest::Client::builder()
            .use_rustls_tls()
            .tls_built_in_root_certs(false)
            .add_root_certificate(ca)
            .connect_timeout(Duration::from_secs(10))
            .build()?;
        Ok(Kube { http, base: env.api.clone(), token: env.token.clone(), ns: env.namespace.clone(), run: env.run_id.clone() })
    }

    /// One request. `ctype` is the body's content type; `accept`, if given,
    /// replaces `application/json`.
    pub async fn req(&self, method: &str, path: &str, ctype: &str, body: Option<&Value>, accept: Option<&str>) -> Result<Resp> {
        let m = reqwest::Method::from_bytes(method.as_bytes())?;
        let mut r = self
            .http
            .request(m, format!("{}{}", self.base, path))
            .bearer_auth(&self.token)
            .header("accept", accept.unwrap_or(JSON))
            .timeout(Duration::from_secs(30));
        if let Some(b) = body {
            r = r.header("content-type", ctype).body(b.to_string());
        }
        let resp = r.send().await.with_context(|| format!("{method} {path}"))?;
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        Ok(Resp { status, body: serde_json::from_str(&text).unwrap_or(Value::String(text)) })
    }

    pub async fn get(&self, path: &str) -> Result<Option<Value>> {
        let r = self.req("GET", path, JSON, None, None).await?;
        match r.status {
            404 => Ok(None),
            _ if r.ok() => Ok(Some(r.body)),
            s => bail!("GET {path}: {s} {}", brief(&r.body)),
        }
    }

    pub async fn create(&self, path: &str, body: Value) -> Result<Value> {
        let r = self.req("POST", path, JSON, Some(&body), None).await?;
        if !r.ok() {
            bail!("POST {path}: {} {}", r.status, brief(&r.body));
        }
        Ok(r.body)
    }

    pub async fn put(&self, path: &str, body: &Value) -> Result<Resp> {
        self.req("PUT", path, JSON, Some(body), None).await
    }

    pub async fn patch(&self, path: &str, ctype: &str, body: &Value) -> Result<Resp> {
        self.req("PATCH", path, ctype, Some(body), None).await
    }

    /// Delete; already gone is fine.
    pub async fn delete(&self, path: &str) -> Result<()> {
        let r = self.req("DELETE", path, JSON, None, None).await?;
        if r.status == 404 || r.ok() {
            return Ok(());
        }
        bail!("DELETE {path}: {} {}", r.status, brief(&r.body))
    }

    /// A namespaced collection path in the run's namespace: `("", "v1",
    /// "configmaps")` → `/api/v1/namespaces/<ns>/configmaps`.
    pub fn path(&self, group: &str, version: &str, plural: &str) -> String {
        if group.is_empty() {
            format!("/api/{version}/namespaces/{}/{plural}", self.ns)
        } else {
            format!("/apis/{group}/{version}/namespaces/{}/{plural}", self.ns)
        }
    }

    /// Metadata for something this run makes.
    pub fn meta(&self, name: &str) -> Value {
        json!({"name": name, "namespace": self.ns, "labels": {"storm.io/test-run": self.run}})
    }

    /// The API answers, and this run may work in its namespace.
    pub async fn preflight(&self) -> Result<()> {
        self.get(&self.path("", "v1", "configmaps"))
            .await?
            .ok_or_else(|| anyhow!("the run's namespace {} does not exist", self.ns))?;
        Ok(())
    }

    /// Poll `path` until `done` holds of it, or `within` passes. Returns the
    /// last object seen (`Null` when there was none) and whether it held.
    pub async fn wait_for(&self, path: &str, within: Duration, done: impl Fn(&Value) -> bool) -> Result<(Value, bool)> {
        let start = Instant::now();
        loop {
            let v = self.get(path).await?.unwrap_or(Value::Null);
            if done(&v) {
                return Ok((v, true));
            }
            if start.elapsed() >= within {
                return Ok((v, false));
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// Open a watch (`path` carries its query) and hand back the events as
    /// they arrive. Read it with [`Watch::next`].
    pub async fn watch(&self, path: &str) -> Result<Watch> {
        let resp = self
            .http
            .get(format!("{}{}", self.base, path))
            .bearer_auth(&self.token)
            .header("accept", JSON)
            .send()
            .await
            .with_context(|| format!("WATCH {path}"))?;
        if !resp.status().is_success() {
            let s = resp.status().as_u16();
            let t = resp.text().await.unwrap_or_default();
            bail!("WATCH {path}: {s} {}", t.chars().take(200).collect::<String>());
        }
        Ok(Watch { resp, buf: Vec::new() })
    }
}

/// A watch's event stream: newline-separated JSON objects, however the
/// server chunks them.
pub struct Watch {
    resp: reqwest::Response,
    buf: Vec<u8>,
}

impl Watch {
    /// The next event, or `None` when `within` passes first or the server
    /// ends the stream.
    pub async fn next(&mut self, within: Duration) -> Result<Option<Value>> {
        let until = Instant::now() + within;
        loop {
            if let Some(i) = self.buf.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = self.buf.drain(..=i).collect();
                let line = String::from_utf8_lossy(&line);
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                return Ok(Some(serde_json::from_str(line).with_context(|| format!("a watch line that is not JSON: {line}"))?));
            }
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(None);
            }
            match tokio::time::timeout(left, self.resp.chunk()).await {
                Err(_) => return Ok(None),
                Ok(Ok(Some(c))) => self.buf.extend_from_slice(&c),
                Ok(Ok(None)) => return Ok(None),
                Ok(Err(e)) => return Err(e.into()),
            }
        }
    }

    /// Read events until one satisfies `want`; `None` if `within` passes.
    pub async fn until(&mut self, within: Duration, want: impl Fn(&Value) -> bool) -> Result<Option<Value>> {
        let end = Instant::now() + within;
        loop {
            let left = end.saturating_duration_since(Instant::now());
            match self.next(left).await? {
                Some(ev) if want(&ev) => return Ok(Some(ev)),
                Some(_) => continue,
                None => return Ok(None),
            }
        }
    }
}

/// The short form of an API error body: its `message`, or the text.
pub fn brief(v: &Value) -> String {
    let s = v["message"].as_str().map(str::to_string).unwrap_or_else(|| v.to_string());
    s.chars().take(300).collect()
}

/// `metadata.resourceVersion` as a number (the store's revision).
pub fn rv(v: &Value) -> Option<u64> {
    v["metadata"]["resourceVersion"].as_str()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brief_prefers_the_message() {
        assert_eq!(brief(&json!({"message": "nope", "code": 409})), "nope");
        assert_eq!(brief(&json!("plain")), "\"plain\"");
    }

    #[test]
    fn rv_reads_the_revision() {
        assert_eq!(rv(&json!({"metadata": {"resourceVersion": "42"}})), Some(42));
        assert_eq!(rv(&json!({"metadata": {}})), None);
    }
}
