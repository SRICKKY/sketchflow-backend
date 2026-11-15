use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;
use serde_json::json;

/// Uniform application error type. Mirrors the SketchFlow Next.js API, which
/// always responds with `{ "error": "<message>" }` on failure.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    Unauthorized(String),
    #[error("{0}")]
    Forbidden(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Conflict(String),
    #[error("An equivalent item already exists in your library.")]
    LibraryItemDuplicate {
        existing_item: serde_json::Value,
        source_metadata: serde_json::Value,
    },
    #[error("{0}")]
    PayloadTooLarge(String),
    #[error("{message}")]
    RevisionConflict {
        code: &'static str,
        message: String,
        revision: i64,
    },
    #[error("{0}")]
    TooManyRequests(String),
    #[error("{message}")]
    LimitReached {
        code: &'static str,
        message: String,
        usage: i64,
        limit: i64,
    },
    #[error("{0}")]
    ServiceUnavailable(String),
    /// Arbitrary upstream-gateway failure (e.g. Razorpay), carrying through
    /// whatever HTTP status the caller decided best represents it.
    #[error("{1}")]
    Gateway(u16, String),
    #[error("internal server error")]
    Internal(#[from] anyhow::Error),
}

#[derive(Serialize)]
pub struct ErrorBody {
    pub error: String,
}

impl AppError {
    fn status_and_message(&self) -> (StatusCode, String) {
        match self {
            AppError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg.clone()),
            AppError::Unauthorized(msg) => (StatusCode::UNAUTHORIZED, msg.clone()),
            AppError::Forbidden(msg) => (StatusCode::FORBIDDEN, msg.clone()),
            AppError::NotFound(msg) => (StatusCode::NOT_FOUND, msg.clone()),
            AppError::Conflict(msg) => (StatusCode::CONFLICT, msg.clone()),
            AppError::LibraryItemDuplicate { .. } => (
                StatusCode::CONFLICT,
                "An equivalent item already exists in your library.".to_string(),
            ),
            AppError::PayloadTooLarge(msg) => (StatusCode::PAYLOAD_TOO_LARGE, msg.clone()),
            AppError::RevisionConflict { message, .. } => (StatusCode::CONFLICT, message.clone()),
            AppError::TooManyRequests(msg) => (StatusCode::TOO_MANY_REQUESTS, msg.clone()),
            AppError::LimitReached { message, .. } => {
                (StatusCode::PAYMENT_REQUIRED, message.clone())
            }
            AppError::ServiceUnavailable(msg) => (StatusCode::SERVICE_UNAVAILABLE, msg.clone()),
            AppError::Gateway(status, msg) => (
                StatusCode::from_u16(*status).unwrap_or(StatusCode::BAD_GATEWAY),
                msg.clone(),
            ),
            AppError::Internal(err) => {
                if let Some(database_error) = err.downcast_ref::<sqlx::Error>() {
                    let event = match database_error {
                        sqlx::Error::PoolTimedOut => "pool_exhausted",
                        sqlx::Error::PoolClosed | sqlx::Error::Io(_) | sqlx::Error::Tls(_) => {
                            "connection_failed"
                        }
                        _ => "query_failed",
                    };
                    tracing::error!(
                        subsystem = "database",
                        event,
                        error = %database_error,
                        "Database operation failed"
                    );
                } else {
                    tracing::error!(
                        subsystem = "api",
                        event = "internal_error",
                        error = %err,
                        "Internal server error"
                    );
                }
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Internal server error".to_string(),
                )
            }
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        if let AppError::RevisionConflict {
            code,
            ref message,
            revision,
        } = self
        {
            return (
                StatusCode::CONFLICT,
                Json(json!({
                    "error": message, "code": code, "revision": revision
                })),
            )
                .into_response();
        }
        if let AppError::LibraryItemDuplicate {
            existing_item,
            source_metadata,
        } = self
        {
            return (
                StatusCode::CONFLICT,
                Json(json!({
                    "error": "An equivalent item already exists in your library.",
                    "code": "LIBRARY_ITEM_DUPLICATE",
                    "existingItem": existing_item,
                    "source": source_metadata
                })),
            )
                .into_response();
        }
        if let AppError::LimitReached {
            code,
            ref message,
            usage,
            limit,
        } = self
        {
            return (
                StatusCode::PAYMENT_REQUIRED,
                Json(json!({
                    "error": message,
                    "code": code,
                    "usage": usage,
                    "limit": limit
                })),
            )
                .into_response();
        }
        let (status, message) = self.status_and_message();
        (status, Json(json!({ "error": message }))).into_response()
    }
}

impl From<sqlx::Error> for AppError {
    fn from(err: sqlx::Error) -> Self {
        AppError::Internal(anyhow::anyhow!(err))
    }
}

impl From<axum::extract::multipart::MultipartError> for AppError {
    fn from(err: axum::extract::multipart::MultipartError) -> Self {
        let message = err.body_text();
        if err.status() == StatusCode::PAYLOAD_TOO_LARGE {
            AppError::PayloadTooLarge(message)
        } else {
            AppError::BadRequest(message)
        }
    }
}

pub type AppResult<T> = Result<T, AppError>;

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    #[tokio::test]
    async fn limit_reached_errors_include_machine_readable_usage() {
        let response = AppError::LimitReached {
            code: "LIBRARY_LIMIT_REACHED",
            message: "Per-item limit reached.".into(),
            usage: 11,
            limit: 10,
        }
        .into_response();
        assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["code"], "LIBRARY_LIMIT_REACHED");
        assert_eq!(value["usage"], 11);
        assert_eq!(value["limit"], 10);
    }

    #[tokio::test]
    async fn duplicate_library_errors_include_item_and_source_metadata() {
        let response = AppError::LibraryItemDuplicate {
            existing_item: json!({
                "id":"item-1",
                "name":"Reusable shape",
                "libraryId":"library-1",
                "contentHash":"abc"
            }),
            source_metadata: json!({
                "scope":"collection",
                "libraryId":"library-1",
                "collectionId":"collection-1",
                "collectionName":"Research"
            }),
        }
        .into_response();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["code"], "LIBRARY_ITEM_DUPLICATE");
        assert_eq!(value["existingItem"]["id"], "item-1");
        assert_eq!(value["source"]["scope"], "collection");
        assert_eq!(value["source"]["collectionName"], "Research");
    }
}
