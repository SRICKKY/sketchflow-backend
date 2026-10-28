mod assets;
mod auth;
mod authz;
mod billing;
mod config;
mod db;
mod editor;
mod error;
mod extract;
mod models;
mod openapi;
mod profile;
mod routes;
mod state;

use std::sync::Arc;

use axum::{
    extract::DefaultBodyLimit,
    routing::{get, patch, post},
    Router,
};
use tower_cookies::CookieManagerLayer;
use tower_http::{
    cors::{AllowOrigin, CorsLayer},
    trace::TraceLayer,
};

use config::AppConfig;
use state::AppState;

// Allow the largest accepted file plus multipart fields and framing overhead.
const MAX_MULTIPART_BODY_BYTES: usize = profile::avatar::AVATAR_MAX_BYTES as usize + 1024 * 1024;

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

    let collaboration = match config.redis_url.as_deref() {
        Some(redis_url) => routes::collaboration::CollaborationHub::with_redis(redis_url)
            .await
            .unwrap_or_else(|_| panic!("failed to initialize Redis collaboration pub/sub")),
        None => routes::collaboration::CollaborationHub::default(),
    };

    let state = AppState {
        db: pool,
        config: Arc::new(config.clone()),
        collaboration,
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
            axum::http::Method::PATCH,
            axum::http::Method::PUT,
            axum::http::Method::DELETE,
            axum::http::Method::OPTIONS,
        ]);

    let app = Router::new()
        .route("/health", get(routes::health::health))
        .route("/api/auth/register", post(routes::auth::register))
        .route("/api/auth/login", post(routes::auth::login))
        .route("/api/auth/logout", post(routes::auth::logout))
        .route("/api/auth/session", get(routes::auth::get_session))
        .route("/api/profile", patch(routes::profile::update_profile_name))
        .route(
            "/api/profile/password",
            patch(routes::profile::update_profile_password),
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
            "/api/uploads",
            post(routes::uploads::upload_image)
                .layer(DefaultBodyLimit::max(MAX_MULTIPART_BODY_BYTES)),
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
