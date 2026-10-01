use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::FromRow;

#[derive(Debug, Clone, FromRow)]
pub struct User {
    pub id: String,
    pub name: Option<String>,
    pub email: String,
    pub email_verified: Option<DateTime<Utc>>,
    pub image: Option<String>,
    pub password_hash: Option<String>,
    pub plan: String,
    pub plan_expires_at: Option<DateTime<Utc>>,
    pub avatar_change_count: i32,
    pub avatar_change_month: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Public-facing user representation (never includes `password_hash`).
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct PublicUser {
    pub id: String,
    pub name: Option<String>,
    pub email: String,
    pub image: Option<String>,
    pub plan: String,
}

impl From<User> for PublicUser {
    fn from(user: User) -> Self {
        Self {
            id: user.id,
            name: user.name,
            email: user.email,
            image: user.image,
            plan: user.plan,
        }
    }
}
