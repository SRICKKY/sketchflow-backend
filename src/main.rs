use axum::{routing::get, Router};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let app = Router::new().route("/health", get(|| async { "ok" }));

    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080")
        .await
        .expect("failed to bind to port 8080");
    tracing::info!("listening on {}", listener.local_addr().unwrap());
    axum::serve(listener, app).await.expect("server error");
}
