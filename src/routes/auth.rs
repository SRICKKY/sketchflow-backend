use axum::{extract::State, Json};
use serde::{Deserialize, Serialize};
use tower_cookies::Cookies;

use crate::{
    auth::{password, session},
    error::{AppError, AppResult},
    models::{PublicUser, User},
    state::AppState,
};

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct RegisterRequest {
    /// Optional display name.
    pub name: Option<String>,
    pub email: String,
    /// Minimum 8 characters.
    pub password: String,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct LoginRequest {
    pub email: String,
    pub password: String,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct AuthResponse {
    pub user: PublicUser,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct SessionResponse {
    pub user: Option<PublicUser>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct LogoutResponse {
    pub success: bool,
}

fn normalize_email(email: &str) -> String {
    email.trim().to_lowercase()
}

fn validate_credentials(email: &str, password: &str) -> AppResult<()> {
    if email.is_empty() || !email.contains('@') {
        return Err(AppError::BadRequest("Enter a valid email address.".to_string()));
    }
    if password.len() < 8 {
        return Err(AppError::BadRequest(
            "Password must be at least 8 characters.".to_string(),
        ));
    }
    Ok(())
}

/// Register a new account with email + password.
#[utoipa::path(
    post,
    path = "/api/auth/register",
    tag = "auth",
    request_body = RegisterRequest,
    responses(
        (status = 201, description = "Account created", body = AuthResponse),
        (status = 400, description = "Invalid input"),
        (status = 409, description = "Email already registered")
    )
)]
pub async fn register(
    State(state): State<AppState>,
    cookies: Cookies,
    Json(body): Json<RegisterRequest>,
) -> AppResult<Json<AuthResponse>> {
    let email = normalize_email(&body.email);
    validate_credentials(&email, &body.password)?;

    let existing = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM users WHERE email = $1")
        .bind(&email)
        .fetch_one(&state.db)
        .await?;
    if existing > 0 {
        return Err(AppError::Conflict(
            "An account with this email already exists.".to_string(),
        ));
    }

    let password_hash = password::hash_password(&body.password)?;
    let id = cuid2::create_id();
    let name = body.name.as_deref().map(str::trim).filter(|s| !s.is_empty());

    let user = sqlx::query_as::<_, User>(
        r#"
        INSERT INTO users (id, name, email, password_hash)
        VALUES ($1, $2, $3, $4)
        RETURNING *
        "#,
    )
    .bind(&id)
    .bind(name)
    .bind(&email)
    .bind(&password_hash)
    .fetch_one(&state.db)
    .await?;

    session::set_session_cookie(&cookies, &state, &user.id)?;

    Ok(Json(AuthResponse {
        user: user.into(),
    }))
}

/// Sign in with email + password.
#[utoipa::path(
    post,
    path = "/api/auth/login",
    tag = "auth",
    request_body = LoginRequest,
    responses(
        (status = 200, description = "Signed in", body = AuthResponse),
        (status = 401, description = "Invalid email or password")
    )
)]
pub async fn login(
    State(state): State<AppState>,
    cookies: Cookies,
    Json(body): Json<LoginRequest>,
) -> AppResult<Json<AuthResponse>> {
    let email = normalize_email(&body.email);
    let invalid = || AppError::Unauthorized("Invalid email or password.".to_string());

    let user = sqlx::query_as::<_, User>("SELECT * FROM users WHERE email = $1")
        .bind(&email)
        .fetch_optional(&state.db)
        .await?
        .ok_or_else(invalid)?;

    let Some(hash) = user.password_hash.as_deref() else {
        return Err(invalid());
    };
    if !password::verify_password(&body.password, hash) {
        return Err(invalid());
    }

    session::set_session_cookie(&cookies, &state, &user.id)?;

    Ok(Json(AuthResponse {
        user: user.into(),
    }))
}

/// Sign out and clear the session cookie.
#[utoipa::path(
    post,
    path = "/api/auth/logout",
    tag = "auth",
    responses((status = 200, description = "Signed out", body = LogoutResponse))
)]
pub async fn logout(State(state): State<AppState>, cookies: Cookies) -> Json<LogoutResponse> {
    session::clear_session_cookie(&cookies, &state);
    Json(LogoutResponse { success: true })
}

/// Get the current session, if any.
#[utoipa::path(
    get,
    path = "/api/auth/session",
    tag = "auth",
    responses((status = 200, description = "Current session", body = SessionResponse))
)]
pub async fn get_session(
    State(state): State<AppState>,
    cookies: Cookies,
) -> AppResult<Json<SessionResponse>> {
    let user = session::optional_current_user(&cookies, &state).await?;
    Ok(Json(SessionResponse {
        user: user.map(Into::into),
    }))
}
