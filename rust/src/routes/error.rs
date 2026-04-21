use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
}

impl ApiError {
    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: msg.into(),
        }
    }

    pub fn not_found(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: msg.into(),
        }
    }

    pub fn internal(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: msg.into(),
        }
    }
}

impl From<rusqlite::Error> for ApiError {
    fn from(err: rusqlite::Error) -> Self {
        ApiError::internal(err.to_string())
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(err: anyhow::Error) -> Self {
        let msg = err.to_string();
        let status = if msg.contains("not found") || msg.contains("Not found") {
            StatusCode::NOT_FOUND
        } else if msg.contains("Invalid")
            || msg.contains("required")
            || msg.contains("Cannot")
            || msg.contains("draft plan")
            || msg.contains("cannot be restarted")
            || msg.contains("Multiple active")
        {
            StatusCode::BAD_REQUEST
        } else {
            StatusCode::INTERNAL_SERVER_ERROR
        };
        ApiError {
            status,
            message: msg,
        }
    }
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    stack: Option<String>,
}

fn debug_enabled() -> bool {
    std::env::var("CLAWKET_DEBUG")
        .ok()
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let stack = if debug_enabled() {
            Some(format!("{}\n(status={})", self.message, self.status))
        } else {
            None
        };
        let body = ErrorBody {
            error: self.message,
            stack,
        };
        (self.status, Json(body)).into_response()
    }
}

pub type ApiResult<T> = std::result::Result<T, ApiError>;

pub fn json_or_404<T: Serialize>(value: Option<T>) -> ApiResult<Json<T>> {
    value
        .map(Json)
        .ok_or_else(|| ApiError::not_found("not found"))
}
