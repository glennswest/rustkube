use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

/// Kubernetes Status object — returned on errors.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub api_version: String,
    pub kind: String,
    pub metadata: serde_json::Value,
    pub status: String,
    pub message: String,
    pub reason: String,
    pub code: u16,
}

impl Status {
    pub fn new(code: StatusCode, reason: &str, message: &str) -> Self {
        Self {
            api_version: "v1".into(),
            kind: "Status".into(),
            metadata: serde_json::json!({}),
            status: "Failure".into(),
            message: message.into(),
            reason: reason.into(),
            code: code.as_u16(),
        }
    }
}

/// API error type that converts to K8s Status responses.
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub reason: String,
    pub message: String,
}

impl ApiError {
    pub fn not_found(resource: &str, name: &str) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            reason: "NotFound".into(),
            message: format!("{resource} \"{name}\" not found"),
        }
    }

    pub fn already_exists(resource: &str, name: &str) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            reason: "AlreadyExists".into(),
            message: format!("{resource} \"{name}\" already exists"),
        }
    }

    /// Did this fail because the key was already there?
    ///
    /// The ClusterIP allocator asks: losing a race for one address is not an
    /// error, it is the signal to try the next one.
    pub fn is_already_exists(&self) -> bool {
        self.reason == "AlreadyExists"
    }

    pub fn conflict(message: &str) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            reason: "Conflict".into(),
            message: message.into(),
        }
    }

    pub fn invalid(message: &str) -> Self {
        Self {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            reason: "Invalid".into(),
            message: message.into(),
        }
    }

    pub fn internal(message: &str) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            reason: "InternalError".into(),
            message: message.into(),
        }
    }

    pub fn gone(message: &str) -> Self {
        Self {
            status: StatusCode::GONE,
            reason: "Gone".into(),
            message: message.into(),
        }
    }

    pub fn unauthorized(message: &str) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            reason: "Unauthorized".into(),
            message: message.into(),
        }
    }

    pub fn forbidden(message: &str) -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            reason: "Forbidden".into(),
            message: message.into(),
        }
    }

    /// The datastore cannot serve right now.
    ///
    /// 503, not 500. The apiserver is working; what is behind it is not, and
    /// the two are not the same fault to page on. `client-go` and `kubectl`
    /// retry a 503 with backoff and surface it as transient — a 500 is
    /// generally terminal for the call.
    pub fn unavailable(message: &str) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            reason: "ServiceUnavailable".into(),
            message: message.into(),
        }
    }
}

/// The longest backend message worth putting in a `Status`.
///
/// A datastore is free to answer with several hundred characters of its own
/// internal structure, and that arrives in `Status.message` — a field a human
/// reads in `kubectl` output and a machine parses. Cap it, and log the whole
/// thing server-side where an operator can go and find it.
const MAX_CAUSE: usize = 160;

fn condense(cause: &str) -> String {
    let cause = cause.trim();
    // A Rust `Debug` dump is one line, but a chained error may not be, and the
    // first line is the one that names the fault.
    let first = cause.lines().next().unwrap_or(cause);

    // Rust error chains render as `outer: source: inner`, and the actionable
    // end of one is the innermost cause — which sits at the *end* of the
    // string. Truncating the head would therefore keep the backend's
    // structure and throw away the fault: on the message that prompted this,
    // 160 characters of `SnapshotSignature { last_log_id: … }` survives and
    // "No space left on device" does not. Take the innermost `source:`
    // instead, and only fall back to truncation when there is no chain.
    let core = match first.rfind("source: ") {
        Some(i) => {
            let tail = &first[i + "source: ".len()..];
            // `, backtrace: None` is noise on every one of these.
            tail.split(", backtrace:").next().unwrap_or(tail).trim().trim_end_matches(',')
        }
        None => first,
    };

    if core.chars().count() <= MAX_CAUSE {
        return core.to_string();
    }
    let cut: String = core.chars().take(MAX_CAUSE).collect();
    format!("{}…", cut.trim_end())
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.reason, self.message)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status_obj = Status::new(self.status, &self.reason, &self.message);
        let body = serde_json::to_string(&status_obj).unwrap_or_default();
        // A backend outage is an operator's problem, not the client's: say so
        // in the way HTTP has a word for, so a retry is paced rather than
        // immediate.
        if self.status == StatusCode::SERVICE_UNAVAILABLE {
            return (
                self.status,
                [("content-type", "application/json"), ("retry-after", "1")],
                body,
            )
                .into_response();
        }
        (
            self.status,
            [("content-type", "application/json")],
            body,
        )
            .into_response()
    }
}

impl From<apimachinery::Error> for ApiError {
    fn from(e: apimachinery::Error) -> Self {
        match e {
            apimachinery::Error::NotFound(msg) => Self {
                status: StatusCode::NOT_FOUND,
                reason: "NotFound".into(),
                message: msg,
            },
            apimachinery::Error::AlreadyExists(msg) => Self {
                status: StatusCode::CONFLICT,
                reason: "AlreadyExists".into(),
                message: msg,
            },
            apimachinery::Error::Conflict => Self::conflict("resource version mismatch"),
            apimachinery::Error::Gone(rev) => {
                Self::gone(&format!("resource version {rev} has been compacted"))
            }
            apimachinery::Error::Unauthorized(msg) => Self {
                status: StatusCode::UNAUTHORIZED,
                reason: "Unauthorized".into(),
                message: msg,
            },
            apimachinery::Error::Forbidden(msg) => Self {
                status: StatusCode::FORBIDDEN,
                reason: "Forbidden".into(),
                message: msg,
            },
            apimachinery::Error::Invalid(msg) => Self::invalid(&msg),
            apimachinery::Error::Unavailable(ref cause) => {
                // The full backend error goes to the log, where it is
                // searchable and nobody has to read it in kubectl output.
                tracing::warn!(error = %e, "datastore unavailable");
                Self::unavailable(&format!("datastore unavailable: {}", condense(cause)))
            }
            _ => Self::internal(&e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The message that prompted this: a full openraft `Debug` dump reaching
    /// `kubectl` because a node ran out of disk.
    const REAL: &str = "linearizable read barrier: when Write Snapshot(Some(SnapshotSignature \
        { last_log_id: Some(LogId { leader_id: LeaderId { term: 1, node_id: 7759039249187390974 }, \
        index: 65036 }), last_membership_log_id: Some(LogId { leader_id: LeaderId { term: 0, \
        node_id: 0 }, index: 0 }), snapshot_id: \"snap-1\" })), verb: Write, source: \
        std::io::error::Error: No space left on device (os error 28), backtrace: None";

    #[test]
    fn an_unavailable_datastore_is_503_not_500() {
        let err: ApiError = apimachinery::Error::Unavailable(REAL.into()).into();
        assert_eq!(err.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(err.reason, "ServiceUnavailable");
    }

    #[test]
    fn the_client_gets_the_fault_not_the_backend_s_internals() {
        let err: ApiError = apimachinery::Error::Unavailable(REAL.into()).into();
        assert!(err.message.starts_with("datastore unavailable: "));
        // The actionable end of the chain survives...
        assert!(err.message.contains("No space left on device"), "{}", err.message);
        // ...and openraft's structure does not.
        assert!(!err.message.contains("SnapshotSignature"), "{}", err.message);
        assert!(!err.message.contains("backtrace"), "{}", err.message);
        assert!(err.message.chars().count() <= MAX_CAUSE + 32, "{}", err.message);
    }

    #[test]
    fn a_cause_with_no_chain_is_left_alone() {
        assert_eq!(condense("no space left on device"), "no space left on device");
        // A long one with nothing to unwrap is truncated rather than dropped.
        let long = "x".repeat(400);
        let out = condense(&long);
        assert!(out.chars().count() <= MAX_CAUSE + 1, "{}", out.chars().count());
        assert!(out.ends_with('…'));
    }

    #[test]
    fn a_real_store_error_is_still_the_apiserver_s_problem() {
        let err: ApiError = apimachinery::Error::Store("bad frame".into()).into();
        assert_eq!(err.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(err.reason, "InternalError");
    }
}
