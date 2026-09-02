use thiserror::Error;

/// Top-level error type for RustKube.
#[derive(Error, Debug)]
pub enum Error {
    #[error("not found: {0}")]
    NotFound(String),

    #[error("already exists: {0}")]
    AlreadyExists(String),

    #[error("conflict: resource version mismatch")]
    Conflict,

    #[error("gone: resource version {0} has been compacted")]
    Gone(u64),

    #[error("invalid: {0}")]
    Invalid(String),

    #[error("unauthorized: {0}")]
    Unauthorized(String),

    #[error("forbidden: {0}")]
    Forbidden(String),

    #[error("internal: {0}")]
    Internal(String),

    #[error("serialization: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("store: {0}")]
    Store(String),

    /// The datastore is reachable-but-not-serving, or not reachable at all:
    /// a gRPC `Unavailable`/`DeadlineExceeded`/`ResourceExhausted`, or a
    /// transport failure.
    ///
    /// Distinct from [`Error::Store`] because the two mean opposite things to
    /// a client. A `Store` error is the apiserver's problem and terminal; this
    /// one is the backend's, is usually transient or operator-fixable, and
    /// must reach the client as 503 so `client-go` retries with backoff
    /// instead of giving up on a 500.
    #[error("datastore unavailable: {0}")]
    Unavailable(String),

    #[error("raft: {0}")]
    Raft(String),

    #[error("tls: {0}")]
    Tls(String),
}

/// Convenience result type.
pub type Result<T> = std::result::Result<T, Error>;
