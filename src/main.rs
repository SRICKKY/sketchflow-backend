mod assets;
mod auth;
mod authz;
mod billing;
mod cache;
mod config;
mod db;
mod editor;
mod error;
mod extract;
mod models;
mod openapi;
mod profile;
mod request_logging;
mod routes;
mod state;

use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::DefaultBodyLimit,
    middleware,
    routing::{get, patch, post},
    Router,
};
use tower_cookies::CookieManagerLayer;
use tower_http::{
    cors::{AllowOrigin, CorsLayer},
    trace::TraceLayer,
};

use cache::RedisCache;
use config::AppConfig;
use state::AppState;

// Allow the largest accepted file plus multipart fields and framing overhead.
const MAX_MULTIPART_BODY_BYTES: usize = profile::avatar::AVATAR_MAX_BYTES as usize + 1024 * 1024;

fn cors_layer(configured_origins: &[String]) -> CorsLayer {
    let origins: Vec<_> = configured_origins
        .iter()
        .filter_map(|origin| origin.parse().ok())
        .collect();

    CorsLayer::new()
        .allow_origin(AllowOrigin::list(origins))
        .allow_credentials(true)
        .allow_headers([
            axum::http::header::CONTENT_TYPE,
            axum::http::header::AUTHORIZATION,
            axum::http::header::COOKIE,
            axum::http::header::HeaderName::from_static("x-request-id"),
        ])
        .expose_headers([axum::http::header::HeaderName::from_static("x-request-id")])
        .allow_methods([
            axum::http::Method::GET,
            axum::http::Method::POST,
            axum::http::Method::PATCH,
            axum::http::Method::PUT,
            axum::http::Method::DELETE,
            axum::http::Method::OPTIONS,
        ])
        .max_age(Duration::from_secs(86_400))
}

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    init_tracing();

    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("install Rustls ring crypto provider");

    let config = AppConfig::from_env();
    let addr = config.socket_addr();

    let pool = db::connect(&config.database_url)
        .await
        .unwrap_or_else(|error| {
            tracing::error!(
                service = "sketchflow-api",
                environment = request_logging::app_environment(),
                subsystem = "database",
                event = "connection_failed",
                error = %error,
                "Database connection failed"
            );
            panic!("failed to connect to database");
        });

    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .unwrap_or_else(|error| {
            tracing::error!(
                service = "sketchflow-api",
                environment = request_logging::app_environment(),
                subsystem = "database",
                event = "migration_failed",
                error = %error,
                "Database migration failed"
            );
            panic!("failed to run database migrations");
        });

    let collaboration = match config.redis_url.as_deref() {
        Some(redis_url) => routes::collaboration::CollaborationHub::with_redis(redis_url)
            .await
            .unwrap_or_else(|error| {
                tracing::error!(
                    service = "sketchflow-api",
                    environment = request_logging::app_environment(),
                    subsystem = "websocket",
                    event = "redis_initialization_failed",
                    error = %error,
                    "Failed to initialize collaboration Redis"
                );
                panic!("failed to initialize Redis collaboration pub/sub");
            }),
        None => routes::collaboration::CollaborationHub::default(),
    };
    let cache = match config.redis_url.as_deref() {
        Some(redis_url) => RedisCache::connect(redis_url)
            .await
            .unwrap_or_else(|error| {
                tracing::error!(
                    service = "sketchflow-api",
                    environment = request_logging::app_environment(),
                    subsystem = "cache",
                    event = "redis_initialization_failed",
                    error = %error,
                    "Failed to initialize Redis API cache"
                );
                panic!("failed to initialize Redis API cache");
            }),
        None => RedisCache::default(),
    };

    let state = AppState {
        db: pool,
        config: Arc::new(config.clone()),
        collaboration,
        cache,
    };

    let cleanup_db = state.db.clone();
    let cleanup_config = state.config.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        loop {
            interval.tick().await;
            match assets::cleanup_abandoned_uploads(&cleanup_db, &cleanup_config).await {
                Ok(cleaned) if cleaned > 0 => {
                    tracing::info!(
                        service = "sketchflow-api",
                        environment = request_logging::app_environment(),
                        subsystem = "uploads",
                        event = "abandoned_uploads_cleaned",
                        count = cleaned,
                        "Cleaned abandoned image uploads"
                    );
                }
                Ok(_) => {}
                Err(error) => tracing::warn!(
                    service = "sketchflow-api",
                    environment = request_logging::app_environment(),
                    subsystem = "uploads",
                    event = "abandoned_upload_cleanup_failed",
                    error = %error,
                    "Failed to clean abandoned image uploads"
                ),
            }
        }
    });

    let cors = cors_layer(&config.cors_allowed_origins);

    let app = Router::new()
        .route("/health", get(routes::health::health))
        .route("/api/auth/register", post(routes::auth::register))
        .route("/api/auth/login", post(routes::auth::login))
        .route("/api/auth/google/start", get(routes::auth::google_start))
        .route(
            "/api/auth/google/callback",
            get(routes::auth::google_callback),
        )
        .route("/api/auth/github/start", get(routes::auth::github_start))
        .route(
            "/api/auth/github/callback",
            get(routes::auth::github_callback),
        )
        .route("/api/auth/logout", post(routes::auth::logout))
        .route("/api/auth/session", get(routes::auth::get_session))
        .route("/api/profile", patch(routes::profile::update_profile_name))
        .route(
            "/api/profile/password",
            patch(routes::profile::update_profile_password),
        )
        .route(
            "/api/billing/overview",
            get(routes::billing::billing_overview),
        )
        .route(
            "/api/billing/invoices",
            get(routes::invoices::list_invoices),
        )
        .route(
            "/api/billing/annual-upgrade-quote",
            get(routes::billing::annual_upgrade_quote),
        )
        .route(
            "/api/billing/lifetime-upgrade-quote",
            get(routes::billing::lifetime_upgrade_quote),
        )
        .route("/api/create-order", post(routes::billing::create_order))
        .route("/api/verify-payment", post(routes::billing::verify_payment))
        .route(
            "/api/paddle/verify-payment",
            post(routes::billing::verify_paddle_payment),
        )
        .route("/api/paddle/webhook", post(routes::billing::paddle_webhook))
        .route(
            "/api/profile/avatar",
            post(routes::profile::upload_avatar)
                .delete(routes::profile::delete_avatar)
                .layer(DefaultBodyLimit::max(MAX_MULTIPART_BODY_BYTES)),
        )
        .route(
            "/api/uploads/presign",
            post(routes::uploads::presign_upload),
        )
        .route(
            "/api/uploads/{asset_id}/complete",
            post(routes::uploads::complete_upload),
        )
        .route(
            "/api/uploads/{asset_id}/abort",
            post(routes::uploads::abort_upload),
        )
        .route("/api/uploads/{asset_id}", get(routes::uploads::get_asset))
        .route(
            "/api/boards/{board_id}/collaboration",
            get(routes::collaboration::websocket),
        )
        .route(
            "/api/boards/{board_id}/snapshot",
            get(routes::collaboration::snapshot),
        )
        .route(
            "/billing/{invoice_id}/pdf",
            get(routes::invoices::download_invoice_pdf),
        )
        .merge(openapi::swagger_ui())
        .merge(routes::dashboard::router())
        .merge(routes::editor::router())
        .merge(routes::notifications::router())
        .merge(routes::public_links::router())
        .layer(cors)
        .layer(TraceLayer::new_for_http())
        .layer(CookieManagerLayer::new())
        .layer(middleware::from_fn(request_logging::log_request_payload))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| panic!("failed to bind to {addr}: {e}"));
    tracing::info!(
        service = "sketchflow-api",
        environment = request_logging::app_environment(),
        subsystem = "startup",
        event = "service_started",
        address = %listener.local_addr().unwrap(),
        "API server listening"
    );
    axum::serve(listener, app).await.expect("server error");
}

fn init_tracing() {
    use tracing_subscriber::{fmt::time::UtcTime, EnvFilter};

    let production = request_logging::app_environment() == "production";
    let filter = || {
        EnvFilter::try_from_default_env().unwrap_or_else(|_| {
            if production {
                "sketchflow_backend=info,tower_http=info".into()
            } else {
                "sketchflow_backend=debug,tower_http=info".into()
            }
        })
    };

    if production {
        tracing_subscriber::fmt()
            .json()
            .flatten_event(true)
            .with_current_span(true)
            .with_span_list(true)
            .with_ansi(false)
            .with_timer(UtcTime::rfc_3339())
            .with_env_filter(filter())
            .init();
    } else {
        tracing_subscriber::fmt()
            .pretty()
            .with_timer(UtcTime::rfc_3339())
            .with_env_filter(filter())
            .init();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        extract::Multipart,
        http::{header, Request, StatusCode},
    };
    use tower::ServiceExt;

    async fn consume_multipart(mut multipart: Multipart) -> error::AppResult<StatusCode> {
        while let Some(field) = multipart.next_field().await? {
            field.bytes().await?;
        }
        Ok(StatusCode::OK)
    }

    fn multipart_request(file_size: usize) -> Request<Body> {
        let boundary = "sketchflow-test-boundary";
        let mut body = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"image.png\"\r\nContent-Type: image/png\r\n\r\n"
        )
        .into_bytes();
        body.resize(body.len() + file_size, b'x');
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

        Request::builder()
            .uri("/upload")
            .method("POST")
            .header(
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(Body::from(body))
            .unwrap()
    }

    #[tokio::test]
    async fn cors_preflight_caches_permission_for_one_day() {
        let origins = vec!["https://sketchflow.online".to_owned()];
        let app = Router::new()
            .route("/api/test", patch(|| async { StatusCode::NO_CONTENT }))
            .layer(cors_layer(&origins));
        let response = app
            .oneshot(
                Request::builder()
                    .method("OPTIONS")
                    .uri("/api/test")
                    .header("origin", "https://sketchflow.online")
                    .header("access-control-request-method", "PATCH")
                    .header("access-control-request-headers", "content-type")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert!(response.status().is_success());
        assert_eq!(
            response.headers().get("access-control-max-age").unwrap(),
            "86400"
        );
    }

    #[tokio::test]
    async fn upload_multipart_limit_accepts_supported_file_sizes() {
        let app = Router::new().route(
            "/upload",
            post(consume_multipart).layer(DefaultBodyLimit::max(MAX_MULTIPART_BODY_BYTES)),
        );

        let response = app
            .oneshot(multipart_request(
                profile::avatar::AVATAR_MAX_BYTES as usize,
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn upload_multipart_limit_rejects_oversized_requests_with_413() {
        let app = Router::new().route(
            "/upload",
            post(consume_multipart).layer(DefaultBodyLimit::max(MAX_MULTIPART_BODY_BYTES)),
        );

        let response = app
            .oneshot(multipart_request(MAX_MULTIPART_BODY_BYTES))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }
}
