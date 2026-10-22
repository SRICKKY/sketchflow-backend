use std::sync::Arc;

use sqlx::PgPool;

use crate::config::AppConfig;
use crate::routes::collaboration::CollaborationHub;

/// Shared application state injected into every handler via `State`.
#[derive(Clone)]
pub struct AppState {
    pub db: PgPool,
    pub config: Arc<AppConfig>,
    pub collaboration: CollaborationHub,
}
