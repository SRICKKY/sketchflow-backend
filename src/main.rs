mod auth;
mod billing;
mod config;
mod db;
mod error;
mod extract;
mod models;
mod openapi;
mod routes;
mod state;

use std::sync::Arc;

use axum::{
    routing::{get, post},
    Router,
};
use tower_cookies::CookieManagerLayer;
use tower_http::{
    cors::{AllowOrigin, CorsLayer},
    trace::TraceLayer,
};

use config::AppConfig;
use state::AppState;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "sketchflow_backend=debug,tower_http=info".into()),
        )
        .init();

    let config = AppConfig::from_env();
    let addr = config.socket_addr();

    let pool = db::connect(&config.database_url)
        .await
        .expect("failed to connect to database");

    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .expect("failed to run database migrations");

    let state = AppState {
        db: pool,
        config: Arc::new(config.clone()),
    };

    let origins: Vec<_> = config
        .cors_allowed_origins
        .iter()
        .filter_map(|o| o.parse().ok())
        .collect();
    let cors = CorsLayer::new()
        .allow_origin(AllowOrigin::list(origins))
        .allow_credentials(true)
        .allow_headers([
            axum::http::header::CONTENT_TYPE,
            axum::http::header::AUTHORIZATION,
            axum::http::header::COOKIE,
        ])
        .allow_methods([
            axum::http::Method::GET,
            axum::http::Method::POST,
            axum::http::Method::DELETE,
            axum::http::Method::OPTIONS,
        ]);

    let app = Router::new()
        .route("/health", get(routes::health::health))
        .route("/api/auth/register", post(routes::auth::register))
        .route("/api/auth/login", post(routes::auth::login))
        .route("/api/auth/logout", post(routes::auth::logout))
        .route("/api/auth/session", get(routes::auth::get_session))
        .route("/api/create-order", post(routes::billing::create_order))
        .route("/api/verify-payment", post(routes::billing::verify_payment))
        .merge(openapi::swagger_ui())
        .layer(cors)
        .layer(TraceLayer::new_for_http())
        .layer(CookieManagerLayer::new())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| panic!("failed to bind to {addr}: {e}"));
    tracing::info!("listening on {}", listener.local_addr().unwrap());
    axum::serve(listener, app).await.expect("server error");
}
