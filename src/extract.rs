use axum::{
    extract::{FromRequest, Request},
    Json,
};
use serde::de::DeserializeOwned;

use crate::error::AppError;

/// A drop-in replacement for `axum::Json` as a request extractor that maps
/// deserialization failures to the uniform `{"error": "Invalid JSON body."}`
/// 400 response the Next.js API returns, instead of axum's default plain-text
/// 422 rejection.
pub struct AppJson<T>(pub T);

impl<T, S> FromRequest<S> for AppJson<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(value)) => Ok(AppJson(value)),
            Err(_) => Err(AppError::BadRequest("Invalid JSON body.".to_string())),
        }
    }
}
