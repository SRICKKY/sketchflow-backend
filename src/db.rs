use std::time::Duration;

use sqlx::postgres::{PgPool, PgPoolOptions};

pub async fn connect(database_url: &str) -> Result<PgPool, sqlx::Error> {
    PgPoolOptions::new()
        .max_connections(10)
        .acquire_slow_threshold(Duration::from_millis(50))
        .connect(database_url)
        .await
}
