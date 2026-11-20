//! A refused request's reason, as the apiserver gave it (#340).
//!
//! `reqwest`'s `error_for_status()` keeps the status code and the URL and
//! drops the body, which is where the apiserver says *why*: a `Status` whose
//! `message` reads `pods "x" is forbidden: unable to validate against any
//! security context constraint: …`. A controller that turned that into an
//! event or a `ReplicaFailure` condition said only `HTTP status client error
//! (403 Forbidden) for url (…)`. [`check`] keeps the message; [`status_of`]
//! keeps the code reachable for callers that branch on 404 or 409.

use reqwest::StatusCode;
use serde_json::Value;

/// A non-2xx answer: its code, and the apiserver's `Status` when it sent one.
#[derive(Debug, Clone)]
pub struct ApiStatusError {
    pub status: StatusCode,
    /// The Status `reason` (`Forbidden`, `AlreadyExists`, …), if any.
    pub reason: String,
    /// The Status `message`, if any.
    pub message: String,
    pub url: String,
}

impl std::fmt::Display for ApiStatusError {
    /// The apiserver's message, as upstream's clients print a refused
    /// request (`pods "x" is forbidden: …`); the code and URL when the body
    /// was not a Status with one.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.message.is_empty() {
            write!(f, "the server answered {} for {}", self.status, self.url)
        } else {
            f.write_str(&self.message)
        }
    }
}

impl std::error::Error for ApiStatusError {}

impl ApiStatusError {
    /// From a response's code, URL and body bytes.
    pub fn from_body(status: StatusCode, url: &str, body: &[u8]) -> Self {
        let doc: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
        let field = |k: &str| {
            if doc["kind"] == "Status" {
                doc[k].as_str().unwrap_or("").to_string()
            } else {
                String::new()
            }
        };
        Self { status, reason: field("reason"), message: field("message"), url: url.to_string() }
    }
}

/// The response when it is a success; otherwise an [`ApiStatusError`]
/// carrying the apiserver's message. A body that cannot be read leaves the
/// message empty, never hides the refusal.
pub async fn check(response: reqwest::Response) -> Result<reqwest::Response, ApiStatusError> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let url = response.url().to_string();
    let body = response.bytes().await.unwrap_or_default();
    Err(ApiStatusError::from_body(status, &url, &body))
}

/// The HTTP status of a failed request: an [`ApiStatusError`]'s, or a
/// `reqwest::Error`'s (one raised before a body was read).
pub fn status_of(error: &anyhow::Error) -> Option<StatusCode> {
    error
        .downcast_ref::<ApiStatusError>()
        .map(|e| e.status)
        .or_else(|| error.downcast_ref::<reqwest::Error>().and_then(reqwest::Error::status))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refusal_reads_as_the_apiservers_message() {
        // #340: the SCC refusal behind a FailedCreate.
        let body = br#"{"kind":"Status","apiVersion":"v1","status":"Failure","reason":"Forbidden","code":403,
            "message":"pods \"web-1\" is forbidden: unable to validate against any security context constraint: [runAsUser: Invalid value: 0]"}"#;
        let e = ApiStatusError::from_body(StatusCode::FORBIDDEN, "https://a:6443/api/v1/namespaces/n/pods", body);
        assert_eq!(e.reason, "Forbidden");
        assert_eq!(
            e.to_string(),
            "pods \"web-1\" is forbidden: unable to validate against any security context constraint: [runAsUser: Invalid value: 0]"
        );
        // Through anyhow, as the controllers carry it: the message and the code.
        let any = anyhow::Error::from(e);
        assert!(format!("Error creating: {any}").starts_with("Error creating: pods \"web-1\" is forbidden: "));
        assert_eq!(status_of(&any), Some(StatusCode::FORBIDDEN));
    }

    #[test]
    fn a_body_that_is_no_status_still_names_the_code() {
        for body in [&b"<html>bad gateway</html>"[..], b"", br#"{"kind":"Pod"}"#] {
            let e = ApiStatusError::from_body(StatusCode::BAD_GATEWAY, "https://a/x", body);
            assert_eq!(e.to_string(), "the server answered 502 Bad Gateway for https://a/x");
        }
        assert_eq!(status_of(&anyhow::anyhow!("plain")), None);
    }
}
