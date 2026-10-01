use chrono::{Duration, Utc};
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};

use crate::error::AppError;

pub const SESSION_COOKIE_NAME: &str = "sketchflow_session";

#[derive(Debug, Serialize, Deserialize)]
pub struct Claims {
    /// Subject: the authenticated user's id.
    pub sub: String,
    /// Expiry (unix timestamp, seconds).
    pub exp: i64,
    /// Issued-at (unix timestamp, seconds).
    pub iat: i64,
}

pub fn issue_token(user_id: &str, secret: &str, session_days: i64) -> Result<String, AppError> {
    let now = Utc::now();
    let claims = Claims {
        sub: user_id.to_string(),
        iat: now.timestamp(),
        exp: (now + Duration::days(session_days)).timestamp(),
    };
    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .map_err(|e| AppError::Internal(anyhow::anyhow!("failed to issue session token: {e}")))
}

pub fn verify_token(token: &str, secret: &str) -> Result<Claims, AppError> {
    decode::<Claims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &Validation::default(),
    )
    .map(|data| data.claims)
    .map_err(|_| AppError::Unauthorized("Invalid or expired session.".to_string()))
}
