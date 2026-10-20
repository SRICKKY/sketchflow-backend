use std::env;

/// Application configuration loaded from environment variables (see
/// `.env.example` for the full list and defaults).
#[derive(Debug, Clone)]
pub struct AppConfig {
    pub host: String,
    pub port: u16,
    pub database_url: String,
    pub jwt_secret: String,
    pub jwt_session_days: i64,
    pub cors_allowed_origins: Vec<String>,
    pub asset_storage_dir: String,
    pub s3_bucket: Option<String>,
    pub s3_region: Option<String>,
    pub s3_access_key_id: Option<String>,
    pub s3_secret_access_key: Option<String>,
    pub s3_session_token: Option<String>,
    pub payment_gateway: String,
    pub razorpay_key_id: Option<String>,
    pub razorpay_key_secret: Option<String>,
    pub paddle_environment: String,
    pub paddle_api_key: Option<String>,
    pub paddle_client_token: Option<String>,
    pub paddle_webhook_secret: Option<String>,
    pub paddle_price_id_monthly: Option<String>,
    pub paddle_price_id_yearly: Option<String>,
    pub plan_price_month_minor: i64,
    pub plan_price_year_minor: i64,
    pub checkout_currency: String,
    pub public_base_url: String,
}

fn env_var(key: &str, default: &str) -> String {
    env::var(key).unwrap_or_else(|_| default.to_string())
}

fn env_option(key: &str) -> Option<String> {
    env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn preferred_env(primary: &str, fallback: &str) -> Option<String> {
    env_option(primary).or_else(|| env_option(fallback))
}

impl AppConfig {
    pub fn from_env() -> Self {
        // Load `.env` if present; ignore errors (e.g. in production where
        // real environment variables are used instead).
        let _ = dotenvy::dotenv();

        let cors_allowed_origins = env_var("CORS_ALLOWED_ORIGINS", "http://localhost:3000")
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        Self {
            host: env_var("HOST", "0.0.0.0"),
            port: env_var("PORT", "8080").parse().unwrap_or(8080),
            database_url: env::var("DATABASE_URL")
                .expect("DATABASE_URL must be set (see .env.example)"),
            jwt_secret: env_var("JWT_SECRET", "change-me-dev-secret"),
            jwt_session_days: env_var("JWT_SESSION_DAYS", "30").parse().unwrap_or(30),
            cors_allowed_origins,
            asset_storage_dir: env_var("ASSET_STORAGE_DIR", "./uploads"),
            s3_bucket: env_option("S3_BUCKET"),
            s3_region: preferred_env("S3_REGION", "AWS_REGION")
                .or_else(|| env_option("AWS_DEFAULT_REGION")),
            s3_access_key_id: preferred_env("S3_ACCESS_KEY_ID", "AWS_ACCESS_KEY_ID"),
            s3_secret_access_key: preferred_env("S3_SECRET_ACCESS_KEY", "AWS_SECRET_ACCESS_KEY"),
            s3_session_token: preferred_env("S3_SESSION_TOKEN", "AWS_SESSION_TOKEN"),
            payment_gateway: env_var("PAYMENT_GATEWAY", "razorpay")
                .trim()
                .to_ascii_lowercase(),
            razorpay_key_id: env::var("RAZORPAY_KEY_ID").ok().filter(|s| !s.is_empty()),
            razorpay_key_secret: env::var("RAZORPAY_KEY_SECRET")
                .ok()
                .filter(|s| !s.is_empty()),
            paddle_environment: env_var("PADDLE_ENV", "sandbox").trim().to_ascii_lowercase(),
            paddle_api_key: env::var("PADDLE_API_KEY").ok().filter(|s| !s.is_empty()),
            paddle_client_token: env::var("PADDLE_CLIENT_TOKEN")
                .ok()
                .filter(|s| !s.is_empty()),
            paddle_webhook_secret: env::var("PADDLE_WEBHOOK_SECRET")
                .ok()
                .filter(|s| !s.is_empty()),
            paddle_price_id_monthly: env::var("PADDLE_PRICE_ID_MONTHLY")
                .ok()
                .filter(|s| !s.is_empty()),
            paddle_price_id_yearly: env::var("PADDLE_PRICE_ID_YEARLY")
                .ok()
                .filter(|s| !s.is_empty()),
            plan_price_month_minor: env_var("PLAN_PRICE_MONTH_MINOR", "50000")
                .parse()
                .unwrap_or(50000),
            plan_price_year_minor: env_var("PLAN_PRICE_YEAR_MINOR", "500000")
                .parse()
                .unwrap_or(500000),
            checkout_currency: env_var("CHECKOUT_CURRENCY", "INR"),
            public_base_url: env_var("PUBLIC_BASE_URL", "http://localhost:8080"),
        }
    }

    pub fn socket_addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}
