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
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status_obj = Status::new(self.status, &self.reason, &self.message);
        let body = serde_json::to_string(&status_obj).unwrap_or_default();
        (
            self.status,
            [("content-type", "application/json")],
            body,
        )
            .into_response()
    }
}

impl From<rk_core::Error> for ApiError {
    fn from(e: rk_core::Error) -> Self {
        match e {
            rk_core::Error::NotFound(msg) => Self {
                status: StatusCode::NOT_FOUND,
                reason: "NotFound".into(),
                message: msg,
            },
            rk_core::Error::AlreadyExists(msg) => Self {
                status: StatusCode::CONFLICT,
                reason: "AlreadyExists".into(),
                message: msg,
            },
            rk_core::Error::Conflict => Self::conflict("resource version mismatch"),
            rk_core::Error::Gone(rev) => {
                Self::gone(&format!("resource version {rev} has been compacted"))
            }
            rk_core::Error::Unauthorized(msg) => Self {
                status: StatusCode::UNAUTHORIZED,
                reason: "Unauthorized".into(),
                message: msg,
            },
            rk_core::Error::Forbidden(msg) => Self {
                status: StatusCode::FORBIDDEN,
                reason: "Forbidden".into(),
                message: msg,
            },
            rk_core::Error::Invalid(msg) => Self::invalid(&msg),
            _ => Self::internal(&e.to_string()),
        }
    }
}
