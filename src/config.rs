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
    pub razorpay_key_id: Option<String>,
    pub razorpay_key_secret: Option<String>,
    pub plan_price_month_minor: i64,
    pub plan_price_year_minor: i64,
    pub checkout_currency: String,
    pub public_base_url: String,
}

fn env_var(key: &str, default: &str) -> String {
    env::var(key).unwrap_or_else(|_| default.to_string())
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
            razorpay_key_id: env::var("RAZORPAY_KEY_ID").ok().filter(|s| !s.is_empty()),
            razorpay_key_secret: env::var("RAZORPAY_KEY_SECRET")
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
