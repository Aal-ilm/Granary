use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};

/// Error type returned by all handlers. Carries the HTTP status to send.
#[derive(Debug)]
pub struct AppError {
    pub status: StatusCode,
    pub message: String,
}

impl AppError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    pub fn not_found() -> Self {
        Self::new(StatusCode::NOT_FOUND, "not found")
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    pub fn bad_gateway(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_GATEWAY, message)
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        if self.status.is_server_error() {
            tracing::warn!(status = %self.status, "{}", self.message);
        }
        // Internal errors can mention file paths or database details; keep
        // those in the server log and give clients a generic message.
        let message = if self.status == StatusCode::INTERNAL_SERVER_ERROR {
            "internal error; see the server log for details"
        } else {
            self.message.as_str()
        };
        (self.status, format!("{message}\n")).into_response()
    }
}

impl From<std::io::Error> for AppError {
    fn from(e: std::io::Error) -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("storage error: {e}"),
        )
    }
}

impl From<reqwest::Error> for AppError {
    fn from(e: reqwest::Error) -> Self {
        Self::bad_gateway(format!("upstream error: {e}"))
    }
}

pub type AppResult<T> = Result<T, AppError>;
