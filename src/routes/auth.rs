use axum::{
    extract::{Query, State},
    response::Redirect,
    Json,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::Utc;
use hmac::{Hmac, KeyInit, Mac};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tower_cookies::{
    cookie::{time::Duration as CookieDuration, SameSite},
    Cookie, Cookies,
};
use uuid::Uuid;

use crate::{
    auth::{password, session},
    error::{AppError, AppResult},
    extract::AppJson,
    models::{PublicUser, User},
    state::AppState,
};

const GOOGLE_STATE_COOKIE: &str = "sketchflow_google_oauth_state";
const GITHUB_STATE_COOKIE: &str = "sketchflow_github_oauth_state";
const OAUTH_STATE_MAX_AGE_SECONDS: i64 = 600;
type OAuthStateMac = Hmac<Sha256>;

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

#[derive(Debug, Deserialize)]
pub struct GoogleStartQuery {
    mode: Option<String>,
    return_to: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct GoogleCallbackQuery {
    state: Option<String>,
    code: Option<String>,
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct GithubStartQuery {
    mode: Option<String>,
    return_to: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct GithubCallbackQuery {
    state: Option<String>,
    code: Option<String>,
    error: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct OAuthState {
    nonce: String,
    code_verifier: Option<String>,
    mode: String,
    return_to: String,
    issued_at: i64,
}

#[derive(Debug, Deserialize)]
struct GoogleTokenResponse {
    access_token: String,
}

#[derive(Debug, Deserialize)]
struct GoogleUserInfo {
    sub: String,
    email: String,
    email_verified: bool,
    name: Option<String>,
    picture: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GithubTokenResponse {
    access_token: String,
}

#[derive(Debug, Deserialize)]
struct GithubProfile {
    id: u64,
    login: String,
    name: Option<String>,
    avatar_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GithubEmail {
    email: String,
    primary: bool,
    verified: bool,
}

struct GithubUserInfo {
    id: String,
    email: String,
    name: Option<String>,
    avatar_url: Option<String>,
}

fn normalize_email(email: &str) -> String {
    email.trim().to_lowercase()
}

fn safe_return_to(value: Option<&str>) -> &str {
    value
        .filter(|value| {
            value.len() <= 2048
                && value.starts_with('/')
                && !value.starts_with("//")
                && !value.contains('\\')
                && !value.chars().any(char::is_control)
        })
        .unwrap_or("/dashboard")
}

fn safe_auth_mode(value: Option<&str>) -> &str {
    if value == Some("register") {
        "register"
    } else {
        "login"
    }
}

fn sign_oauth_state(value: &OAuthState, secret: &str) -> AppResult<String> {
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(value).map_err(|error| {
        AppError::Internal(anyhow::anyhow!("failed to serialize OAuth state: {error}"))
    })?);
    let mut mac = OAuthStateMac::new_from_slice(secret.as_bytes()).map_err(|error| {
        AppError::Internal(anyhow::anyhow!("invalid OAuth signing key: {error}"))
    })?;
    mac.update(payload.as_bytes());
    let signature = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
    Ok(format!("{payload}.{signature}"))
}

fn verify_oauth_state(value: &str, secret: &str) -> Option<OAuthState> {
    let (payload, signature) = value.split_once('.')?;
    let signature = URL_SAFE_NO_PAD.decode(signature).ok()?;
    let mut mac = OAuthStateMac::new_from_slice(secret.as_bytes()).ok()?;
    mac.update(payload.as_bytes());
    mac.verify_slice(&signature).ok()?;

    let payload = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let state: OAuthState = serde_json::from_slice(&payload).ok()?;
    let age = Utc::now().timestamp().checked_sub(state.issued_at)?;
    if !(0..=OAUTH_STATE_MAX_AGE_SECONDS).contains(&age)
        || safe_return_to(Some(&state.return_to)) != state.return_to
        || !matches!(state.mode.as_str(), "login" | "register")
    {
        return None;
    }
    Some(state)
}

fn frontend_url(state: &AppState, path: &str) -> AppResult<Url> {
    let base = Url::parse(&state.config.frontend_base_url).map_err(|error| {
        AppError::Internal(anyhow::anyhow!("invalid FRONTEND_BASE_URL: {error}"))
    })?;
    if !matches!(base.scheme(), "http" | "https")
        || base.host_str().is_none()
        || !base.username().is_empty()
        || base.password().is_some()
    {
        return Err(AppError::Internal(anyhow::anyhow!(
            "FRONTEND_BASE_URL must be an HTTP(S) origin"
        )));
    }
    let url = base.join(path).map_err(|error| {
        AppError::Internal(anyhow::anyhow!(
            "invalid Google OAuth redirect path: {error}"
        ))
    })?;
    if url.origin() != base.origin() {
        return Err(AppError::Internal(anyhow::anyhow!(
            "Google OAuth redirect must stay on FRONTEND_BASE_URL"
        )));
    }
    Ok(url)
}

fn oauth_error_redirect(
    state: &AppState,
    mode: &str,
    return_to: &str,
    error: &str,
    provider: &str,
) -> AppResult<Redirect> {
    tracing::warn!(
        subsystem = "auth",
        event = "oauth_authentication_failed",
        provider,
        reason = error,
        "OAuth authentication failed"
    );
    let path = if mode == "register" {
        "/register"
    } else {
        "/login"
    };
    let mut url = frontend_url(state, path)?;
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("oauth_error", error);
        query.append_pair("oauth_provider", provider);
        if return_to != "/dashboard" {
            query.append_pair("callbackUrl", return_to);
        }
    }
    Ok(Redirect::temporary(url.as_str()))
}

fn google_error_redirect(
    state: &AppState,
    mode: &str,
    return_to: &str,
    error: &str,
) -> AppResult<Redirect> {
    oauth_error_redirect(state, mode, return_to, error, "google")
}

fn github_error_redirect(
    state: &AppState,
    mode: &str,
    return_to: &str,
    error: &str,
) -> AppResult<Redirect> {
    oauth_error_redirect(state, mode, return_to, error, "github")
}

fn google_callback_uri(state: &AppState) -> AppResult<String> {
    let base = Url::parse(&state.config.public_base_url)
        .map_err(|error| AppError::Internal(anyhow::anyhow!("invalid PUBLIC_BASE_URL: {error}")))?;
    let callback = base.join("/api/auth/google/callback").map_err(|error| {
        AppError::Internal(anyhow::anyhow!(
            "invalid Google OAuth callback URL: {error}"
        ))
    })?;
    Ok(callback.to_string())
}

fn github_callback_uri(state: &AppState) -> AppResult<String> {
    let base = Url::parse(&state.config.public_base_url)
        .map_err(|error| AppError::Internal(anyhow::anyhow!("invalid PUBLIC_BASE_URL: {error}")))?;
    let callback = base.join("/api/auth/github/callback").map_err(|error| {
        AppError::Internal(anyhow::anyhow!(
            "invalid GitHub OAuth callback URL: {error}"
        ))
    })?;
    Ok(callback.to_string())
}

fn clear_google_state_cookie(cookies: &Cookies, state: &AppState) {
    let mut cookie = Cookie::new(GOOGLE_STATE_COOKIE, "");
    cookie.set_path("/api/auth/google");
    cookie.set_http_only(true);
    cookie.set_same_site(SameSite::Lax);
    cookie.set_secure(state.config.public_base_url.starts_with("https://"));
    cookies.remove(cookie);
}

fn clear_github_state_cookie(cookies: &Cookies, state: &AppState) {
    let mut cookie = Cookie::new(GITHUB_STATE_COOKIE, "");
    cookie.set_path("/api/auth/github");
    cookie.set_http_only(true);
    cookie.set_same_site(SameSite::Lax);
    cookie.set_secure(state.config.public_base_url.starts_with("https://"));
    cookies.remove(cookie);
}

fn validate_credentials(email: &str, password: &str) -> AppResult<()> {
    if email.is_empty() || !email.contains('@') {
        return Err(AppError::BadRequest(
            "Enter a valid email address.".to_string(),
        ));
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
    AppJson(body): AppJson<RegisterRequest>,
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
    let name = body
        .name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());

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

    Ok(Json(AuthResponse { user: user.into() }))
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
    AppJson(body): AppJson<LoginRequest>,
) -> AppResult<Json<AuthResponse>> {
    let email = normalize_email(&body.email);
    let invalid = || {
        tracing::warn!(
            subsystem = "auth",
            event = "login_failed",
            reason = "invalid_credentials",
            "Login failed"
        );
        AppError::Unauthorized("Invalid email or password.".to_string())
    };

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
    tracing::info!(
        subsystem = "auth",
        event = "login_succeeded",
        user_id = %user.id,
        "Login succeeded"
    );

    Ok(Json(AuthResponse { user: user.into() }))
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

/// Begin Google OAuth and bind the callback to a signed, short-lived state cookie.
#[utoipa::path(
    get,
    path = "/api/auth/google/start",
    tag = "auth",
    params(
        ("mode" = Option<String>, Query, description = "login or register"),
        ("return_to" = Option<String>, Query, description = "Safe UI-relative path after sign-in")
    ),
    responses((status = 307, description = "Redirect to Google's authorization page"))
)]
pub async fn google_start(
    State(state): State<AppState>,
    cookies: Cookies,
    Query(query): Query<GoogleStartQuery>,
) -> AppResult<Redirect> {
    let mode = safe_auth_mode(query.mode.as_deref()).to_string();
    let return_to = safe_return_to(query.return_to.as_deref()).to_string();
    let (Some(client_id), Some(_)) = (
        state.config.google_client_id.as_deref(),
        state.config.google_client_secret.as_deref(),
    ) else {
        return google_error_redirect(&state, &mode, &return_to, "not_configured");
    };

    let code_verifier = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let code_challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(code_verifier.as_bytes()));
    let oauth_state = OAuthState {
        nonce: Uuid::new_v4().simple().to_string(),
        code_verifier: Some(code_verifier),
        mode,
        return_to,
        issued_at: Utc::now().timestamp(),
    };
    let cookie_value = sign_oauth_state(&oauth_state, &state.config.jwt_secret)?;
    let cookie = Cookie::build((GOOGLE_STATE_COOKIE, cookie_value))
        .path("/api/auth/google")
        .http_only(true)
        .same_site(SameSite::Lax)
        .secure(state.config.public_base_url.starts_with("https://"))
        .max_age(CookieDuration::seconds(OAUTH_STATE_MAX_AGE_SECONDS))
        .build();
    cookies.add(cookie);

    let mut authorization_url = Url::parse("https://accounts.google.com/o/oauth2/v2/auth")
        .map_err(|error| AppError::Internal(anyhow::anyhow!(error)))?;
    {
        let mut query = authorization_url.query_pairs_mut();
        query
            .append_pair("client_id", client_id)
            .append_pair("redirect_uri", &google_callback_uri(&state)?)
            .append_pair("response_type", "code")
            .append_pair("scope", "openid email profile")
            .append_pair("code_challenge", &code_challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", &oauth_state.nonce);
    }
    Ok(Redirect::temporary(authorization_url.as_str()))
}

/// Complete Google OAuth, link only a verified Google email, and set the API session cookie.
#[utoipa::path(
    get,
    path = "/api/auth/google/callback",
    tag = "auth",
    params(
        ("state" = Option<String>, Query, description = "OAuth state returned by Google"),
        ("code" = Option<String>, Query, description = "Authorization code returned by Google"),
        ("error" = Option<String>, Query, description = "OAuth error returned by Google")
    ),
    responses((status = 307, description = "Redirect to the UI after authentication"))
)]
pub async fn google_callback(
    State(state): State<AppState>,
    cookies: Cookies,
    Query(query): Query<GoogleCallbackQuery>,
) -> AppResult<Redirect> {
    let cookie_value = cookies
        .get(GOOGLE_STATE_COOKIE)
        .map(|cookie| cookie.value().to_string());
    clear_google_state_cookie(&cookies, &state);

    let Some(oauth_state) = cookie_value
        .as_deref()
        .and_then(|value| verify_oauth_state(value, &state.config.jwt_secret))
    else {
        return google_error_redirect(&state, "login", "/dashboard", "invalid_state");
    };
    if query.state.as_deref() != Some(oauth_state.nonce.as_str()) {
        return google_error_redirect(
            &state,
            &oauth_state.mode,
            &oauth_state.return_to,
            "invalid_state",
        );
    }
    if let Some(provider_error) = query.error.as_deref() {
        let error = if provider_error == "access_denied" {
            "cancelled"
        } else {
            "failed"
        };
        return google_error_redirect(&state, &oauth_state.mode, &oauth_state.return_to, error);
    }
    let Some(code) = query.code.as_deref() else {
        return google_error_redirect(&state, &oauth_state.mode, &oauth_state.return_to, "failed");
    };
    let (Some(client_id), Some(client_secret)) = (
        state.config.google_client_id.as_deref(),
        state.config.google_client_secret.as_deref(),
    ) else {
        return google_error_redirect(
            &state,
            &oauth_state.mode,
            &oauth_state.return_to,
            "not_configured",
        );
    };

    let Some(code_verifier) = oauth_state.code_verifier.as_deref() else {
        return google_error_redirect(
            &state,
            &oauth_state.mode,
            &oauth_state.return_to,
            "invalid_state",
        );
    };
    let profile =
        match google_user_info(code, client_id, client_secret, code_verifier, &state).await {
            Ok(profile) => profile,
            Err(error) => {
                tracing::warn!(
                    subsystem = "auth",
                    event = "oauth_profile_request_failed",
                    provider = "google",
                    %error,
                    "Google OAuth user info request failed"
                );
                return google_error_redirect(
                    &state,
                    &oauth_state.mode,
                    &oauth_state.return_to,
                    "failed",
                );
            }
        };
    let user = match find_or_create_oauth_user(
        &state,
        "google",
        &profile.sub,
        &profile.email,
        profile.name.as_deref(),
        profile.picture.as_deref(),
        "Google",
    )
    .await
    {
        Ok(user) => user,
        Err(error) => {
            tracing::error!(
                subsystem = "auth",
                event = "oauth_account_provisioning_failed",
                provider = "google",
                error = %error,
                "Google OAuth account provisioning failed"
            );
            let code = if matches!(error, AppError::Conflict(_)) {
                "account_exists"
            } else {
                "failed"
            };
            return google_error_redirect(&state, &oauth_state.mode, &oauth_state.return_to, code);
        }
    };
    session::set_session_cookie(&cookies, &state, &user.id)?;
    tracing::info!(
        subsystem = "auth",
        event = "oauth_login_succeeded",
        provider = "google",
        user_id = %user.id,
        "OAuth login succeeded"
    );
    let destination = frontend_url(&state, &oauth_state.return_to)?;
    Ok(Redirect::temporary(destination.as_str()))
}

/// Begin GitHub OAuth and bind the callback to a signed, short-lived state cookie.
#[utoipa::path(
    get,
    path = "/api/auth/github/start",
    tag = "auth",
    params(
        ("mode" = Option<String>, Query, description = "login or register"),
        ("return_to" = Option<String>, Query, description = "Safe UI-relative path after sign-in")
    ),
    responses((status = 307, description = "Redirect to GitHub's authorization page"))
)]
pub async fn github_start(
    State(state): State<AppState>,
    cookies: Cookies,
    Query(query): Query<GithubStartQuery>,
) -> AppResult<Redirect> {
    let mode = safe_auth_mode(query.mode.as_deref()).to_string();
    let return_to = safe_return_to(query.return_to.as_deref()).to_string();
    let (Some(client_id), Some(_)) = (
        state.config.github_client_id.as_deref(),
        state.config.github_client_secret.as_deref(),
    ) else {
        return github_error_redirect(&state, &mode, &return_to, "not_configured");
    };

    let oauth_state = OAuthState {
        nonce: Uuid::new_v4().simple().to_string(),
        code_verifier: None,
        mode,
        return_to,
        issued_at: Utc::now().timestamp(),
    };
    let cookie_value = sign_oauth_state(&oauth_state, &state.config.jwt_secret)?;
    let cookie = Cookie::build((GITHUB_STATE_COOKIE, cookie_value))
        .path("/api/auth/github")
        .http_only(true)
        .same_site(SameSite::Lax)
        .secure(state.config.public_base_url.starts_with("https://"))
        .max_age(CookieDuration::seconds(OAUTH_STATE_MAX_AGE_SECONDS))
        .build();
    cookies.add(cookie);

    let mut authorization_url = Url::parse("https://github.com/login/oauth/authorize")
        .map_err(|error| AppError::Internal(anyhow::anyhow!(error)))?;
    {
        let mut query = authorization_url.query_pairs_mut();
        query
            .append_pair("client_id", client_id)
            .append_pair("redirect_uri", &github_callback_uri(&state)?)
            .append_pair("scope", "read:user user:email")
            .append_pair("state", &oauth_state.nonce);
    }
    Ok(Redirect::temporary(authorization_url.as_str()))
}

/// Complete GitHub OAuth using a verified primary email and the existing API session.
#[utoipa::path(
    get,
    path = "/api/auth/github/callback",
    tag = "auth",
    params(
        ("state" = Option<String>, Query, description = "OAuth state returned by GitHub"),
        ("code" = Option<String>, Query, description = "Authorization code returned by GitHub"),
        ("error" = Option<String>, Query, description = "OAuth error returned by GitHub")
    ),
    responses((status = 307, description = "Redirect to the UI after authentication"))
)]
pub async fn github_callback(
    State(state): State<AppState>,
    cookies: Cookies,
    Query(query): Query<GithubCallbackQuery>,
) -> AppResult<Redirect> {
    let cookie_value = cookies
        .get(GITHUB_STATE_COOKIE)
        .map(|cookie| cookie.value().to_string());
    clear_github_state_cookie(&cookies, &state);

    let Some(oauth_state) = cookie_value
        .as_deref()
        .and_then(|value| verify_oauth_state(value, &state.config.jwt_secret))
    else {
        return github_error_redirect(&state, "login", "/dashboard", "invalid_state");
    };
    if query.state.as_deref() != Some(oauth_state.nonce.as_str()) {
        return github_error_redirect(
            &state,
            &oauth_state.mode,
            &oauth_state.return_to,
            "invalid_state",
        );
    }
    if let Some(provider_error) = query.error.as_deref() {
        let error = if provider_error == "access_denied" {
            "cancelled"
        } else {
            "failed"
        };
        return github_error_redirect(&state, &oauth_state.mode, &oauth_state.return_to, error);
    }
    let Some(code) = query.code.as_deref() else {
        return github_error_redirect(&state, &oauth_state.mode, &oauth_state.return_to, "failed");
    };
    let (Some(client_id), Some(client_secret)) = (
        state.config.github_client_id.as_deref(),
        state.config.github_client_secret.as_deref(),
    ) else {
        return github_error_redirect(
            &state,
            &oauth_state.mode,
            &oauth_state.return_to,
            "not_configured",
        );
    };

    let profile = match github_user_info(code, client_id, client_secret, &state).await {
        Ok(profile) => profile,
        Err(error) => {
            tracing::warn!(
                subsystem = "auth",
                event = "oauth_profile_request_failed",
                provider = "github",
                %error,
                "GitHub OAuth user info request failed"
            );
            return github_error_redirect(
                &state,
                &oauth_state.mode,
                &oauth_state.return_to,
                "failed",
            );
        }
    };
    let user = match find_or_create_oauth_user(
        &state,
        "github",
        &profile.id,
        &profile.email,
        profile.name.as_deref(),
        profile.avatar_url.as_deref(),
        "GitHub",
    )
    .await
    {
        Ok(user) => user,
        Err(error) => {
            tracing::error!(
                subsystem = "auth",
                event = "oauth_account_provisioning_failed",
                provider = "github",
                error = %error,
                "GitHub OAuth account provisioning failed"
            );
            let code = if matches!(error, AppError::Conflict(_)) {
                "account_exists"
            } else {
                "failed"
            };
            return github_error_redirect(&state, &oauth_state.mode, &oauth_state.return_to, code);
        }
    };
    session::set_session_cookie(&cookies, &state, &user.id)?;
    tracing::info!(
        subsystem = "auth",
        event = "oauth_login_succeeded",
        provider = "github",
        user_id = %user.id,
        "OAuth login succeeded"
    );
    let destination = frontend_url(&state, &oauth_state.return_to)?;
    Ok(Redirect::temporary(destination.as_str()))
}

async fn google_user_info(
    code: &str,
    client_id: &str,
    client_secret: &str,
    code_verifier: &str,
    state: &AppState,
) -> AppResult<GoogleUserInfo> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|error| {
            AppError::Internal(anyhow::anyhow!(
                "failed to initialize Google OAuth HTTP client: {error}"
            ))
        })?;
    let token_response = client
        .post("https://oauth2.googleapis.com/token")
        .form(&[
            ("code", code),
            ("client_id", client_id),
            ("client_secret", client_secret),
            ("redirect_uri", google_callback_uri(state)?.as_str()),
            ("grant_type", "authorization_code"),
            ("code_verifier", code_verifier),
        ])
        .send()
        .await
        .map_err(|error| {
            tracing::warn!(
                subsystem = "auth",
                event = "oauth_token_exchange_failed",
                provider = "google",
                %error,
                "Google OAuth token exchange failed"
            );
            AppError::Gateway(502, "Google sign-in could not be completed.".to_string())
        })?;
    if !token_response.status().is_success() {
        tracing::warn!(
            subsystem = "auth",
            event = "oauth_token_exchange_rejected",
            provider = "google",
            status = %token_response.status(),
            "Google OAuth token exchange was rejected"
        );
        return Err(AppError::Gateway(
            502,
            "Google sign-in could not be completed.".to_string(),
        ));
    }
    let token = token_response
        .json::<GoogleTokenResponse>()
        .await
        .map_err(|error| {
            tracing::warn!(
                subsystem = "auth",
                event = "oauth_token_response_invalid",
                provider = "google",
                %error,
                "Google OAuth token response was invalid"
            );
            AppError::Gateway(502, "Google sign-in could not be completed.".to_string())
        })?;

    let profile_response = client
        .get("https://openidconnect.googleapis.com/v1/userinfo")
        .bearer_auth(token.access_token)
        .send()
        .await
        .map_err(|error| {
            tracing::warn!(
                subsystem = "auth",
                event = "oauth_profile_request_failed",
                provider = "google",
                %error,
                "Google user info request failed"
            );
            AppError::Gateway(502, "Google sign-in could not be completed.".to_string())
        })?;
    if !profile_response.status().is_success() {
        tracing::warn!(
            subsystem = "auth",
            event = "oauth_profile_request_rejected",
            provider = "google",
            status = %profile_response.status(),
            "Google user info request was rejected"
        );
        return Err(AppError::Gateway(
            502,
            "Google sign-in could not be completed.".to_string(),
        ));
    }
    let profile = profile_response
        .json::<GoogleUserInfo>()
        .await
        .map_err(|error| {
            tracing::warn!(
                subsystem = "auth",
                event = "oauth_profile_response_invalid",
                provider = "google",
                %error,
                "Google user info response was invalid"
            );
            AppError::Gateway(502, "Google sign-in could not be completed.".to_string())
        })?;
    if !profile.email_verified
        || profile.sub.trim().is_empty()
        || !normalize_email(&profile.email).contains('@')
    {
        return Err(AppError::Unauthorized(
            "Google did not verify this email address.".to_string(),
        ));
    }
    Ok(profile)
}

async fn github_user_info(
    code: &str,
    client_id: &str,
    client_secret: &str,
    state: &AppState,
) -> AppResult<GithubUserInfo> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|error| {
            AppError::Internal(anyhow::anyhow!(
                "failed to initialize GitHub OAuth HTTP client: {error}"
            ))
        })?;
    let token_response = client
        .post("https://github.com/login/oauth/access_token")
        .header(reqwest::header::ACCEPT, "application/json")
        .form(&[
            ("code", code),
            ("client_id", client_id),
            ("client_secret", client_secret),
            ("redirect_uri", github_callback_uri(state)?.as_str()),
        ])
        .send()
        .await
        .map_err(|error| {
            tracing::warn!(
                subsystem = "auth",
                event = "oauth_token_exchange_failed",
                provider = "github",
                %error,
                "GitHub OAuth token exchange failed"
            );
            AppError::Gateway(502, "GitHub sign-in could not be completed.".to_string())
        })?;
    if !token_response.status().is_success() {
        tracing::warn!(
            subsystem = "auth",
            event = "oauth_token_exchange_rejected",
            provider = "github",
            status = %token_response.status(),
            "GitHub OAuth token exchange was rejected"
        );
        return Err(AppError::Gateway(
            502,
            "GitHub sign-in could not be completed.".to_string(),
        ));
    }
    let token = token_response
        .json::<GithubTokenResponse>()
        .await
        .map_err(|error| {
            tracing::warn!(
                subsystem = "auth",
                event = "oauth_token_response_invalid",
                provider = "github",
                %error,
                "GitHub OAuth token response was invalid"
            );
            AppError::Gateway(502, "GitHub sign-in could not be completed.".to_string())
        })?;

    let profile_response = client
        .get("https://api.github.com/user")
        .bearer_auth(&token.access_token)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header(reqwest::header::USER_AGENT, "SketchFlow")
        .send()
        .await
        .map_err(|error| {
            tracing::warn!(
                subsystem = "auth",
                event = "oauth_profile_request_failed",
                provider = "github",
                %error,
                "GitHub profile request failed"
            );
            AppError::Gateway(502, "GitHub sign-in could not be completed.".to_string())
        })?;
    if !profile_response.status().is_success() {
        tracing::warn!(
            subsystem = "auth",
            event = "oauth_profile_request_rejected",
            provider = "github",
            status = %profile_response.status(),
            "GitHub profile request was rejected"
        );
        return Err(AppError::Gateway(
            502,
            "GitHub sign-in could not be completed.".to_string(),
        ));
    }
    let profile = profile_response
        .json::<GithubProfile>()
        .await
        .map_err(|error| {
            tracing::warn!(
                subsystem = "auth",
                event = "oauth_profile_response_invalid",
                provider = "github",
                %error,
                "GitHub profile response was invalid"
            );
            AppError::Gateway(502, "GitHub sign-in could not be completed.".to_string())
        })?;
    if profile.id == 0 || profile.login.trim().is_empty() {
        return Err(AppError::Unauthorized(
            "GitHub did not return a valid account.".to_string(),
        ));
    }

    let emails_response = client
        .get("https://api.github.com/user/emails")
        .bearer_auth(&token.access_token)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header(reqwest::header::USER_AGENT, "SketchFlow")
        .send()
        .await
        .map_err(|error| {
            tracing::warn!(
                subsystem = "auth",
                event = "oauth_email_request_failed",
                provider = "github",
                %error,
                "GitHub email request failed"
            );
            AppError::Gateway(502, "GitHub sign-in could not be completed.".to_string())
        })?;
    if !emails_response.status().is_success() {
        tracing::warn!(
            subsystem = "auth",
            event = "oauth_email_request_rejected",
            provider = "github",
            status = %emails_response.status(),
            "GitHub email request was rejected"
        );
        return Err(AppError::Gateway(
            502,
            "GitHub sign-in could not be completed.".to_string(),
        ));
    }
    let emails = emails_response
        .json::<Vec<GithubEmail>>()
        .await
        .map_err(|error| {
            tracing::warn!(
                subsystem = "auth",
                event = "oauth_email_response_invalid",
                provider = "github",
                %error,
                "GitHub email response was invalid"
            );
            AppError::Gateway(502, "GitHub sign-in could not be completed.".to_string())
        })?;
    let email = verified_primary_github_email(emails).ok_or_else(|| {
        AppError::Unauthorized(
            "GitHub did not provide a verified primary email address.".to_string(),
        )
    })?;

    let name = profile
        .name
        .filter(|name| !name.trim().is_empty())
        .or(Some(profile.login));
    Ok(GithubUserInfo {
        id: profile.id.to_string(),
        email,
        name,
        avatar_url: profile.avatar_url,
    })
}

fn verified_primary_github_email(emails: Vec<GithubEmail>) -> Option<String> {
    emails
        .into_iter()
        .find(|email| email.primary && email.verified)
        .map(|email| normalize_email(&email.email))
        .filter(|email| {
            !email.chars().any(char::is_whitespace)
                && email.split_once('@').is_some_and(|(local, domain)| {
                    !local.is_empty()
                        && !domain.contains('@')
                        && domain.contains('.')
                        && !domain.starts_with('.')
                        && !domain.ends_with('.')
                })
        })
}

async fn find_or_create_oauth_user(
    state: &AppState,
    provider: &str,
    provider_account_id: &str,
    email: &str,
    name: Option<&str>,
    image: Option<&str>,
    provider_name: &str,
) -> AppResult<User> {
    let email = normalize_email(email);
    let mut transaction = state.db.begin().await?;
    if let Some(user) = sqlx::query_as::<_, User>(
        r#"
        SELECT users.*
        FROM users
        JOIN accounts ON accounts.user_id = users.id
        WHERE accounts.provider = $1
          AND accounts.provider_account_id = $2
        "#,
    )
    .bind(provider)
    .bind(provider_account_id)
    .fetch_optional(&mut *transaction)
    .await?
    {
        transaction.commit().await?;
        return Ok(user);
    }

    let existing_user = sqlx::query_scalar::<_, String>("SELECT id FROM users WHERE email = $1")
        .bind(&email)
        .fetch_optional(&mut *transaction)
        .await?;
    if existing_user.is_some() {
        return Err(AppError::Conflict(
            "An account with this email already exists.".to_string(),
        ));
    }

    let name = name.map(str::trim).filter(|value| !value.is_empty());
    let image = image.filter(|value| value.starts_with("https://"));
    let user = sqlx::query_as::<_, User>(
        r#"
        INSERT INTO users (id, name, email, email_verified, image)
        VALUES ($1, $2, $3, now(), $4)
        ON CONFLICT (email) DO NOTHING
        RETURNING *
        "#,
    )
    .bind(cuid2::create_id())
    .bind(name)
    .bind(&email)
    .bind(image)
    .fetch_optional(&mut *transaction)
    .await?;
    let Some(user) = user else {
        let concurrently_linked_user = sqlx::query_as::<_, User>(
            r#"
            SELECT users.*
            FROM users
            JOIN accounts ON accounts.user_id = users.id
            WHERE accounts.provider = $1
              AND accounts.provider_account_id = $2
            "#,
        )
        .bind(provider)
        .bind(provider_account_id)
        .fetch_optional(&mut *transaction)
        .await?;
        if let Some(user) = concurrently_linked_user {
            transaction.commit().await?;
            return Ok(user);
        }
        return Err(AppError::Conflict(
            "An account with this email already exists.".to_string(),
        ));
    };

    sqlx::query(
        r#"
        INSERT INTO accounts (user_id, type, provider, provider_account_id)
        VALUES ($1, 'oauth', $2, $3)
        ON CONFLICT (provider, provider_account_id) DO NOTHING
        "#,
    )
    .bind(&user.id)
    .bind(provider)
    .bind(provider_account_id)
    .execute(&mut *transaction)
    .await?;
    let linked_user_id = sqlx::query_scalar::<_, String>(
        "SELECT user_id FROM accounts WHERE provider = $1 AND provider_account_id = $2",
    )
    .bind(provider)
    .bind(provider_account_id)
    .fetch_one(&mut *transaction)
    .await?;
    if linked_user_id != user.id {
        return Err(AppError::Conflict(format!(
            "This {provider_name} account is linked to a different SketchFlow account."
        )));
    }
    transaction.commit().await?;
    Ok(user)
}

#[cfg(test)]
mod oauth_tests {
    use super::*;

    #[test]
    fn rejects_external_and_malformed_return_paths() {
        assert_eq!(
            safe_return_to(Some("/dashboard?tab=recent")),
            "/dashboard?tab=recent"
        );
        assert_eq!(safe_return_to(Some("//attacker.example")), "/dashboard");
        assert_eq!(
            safe_return_to(Some("https://attacker.example")),
            "/dashboard"
        );
        assert_eq!(safe_return_to(Some("/\\attacker.example")), "/dashboard");
    }

    #[test]
    fn signed_oauth_state_round_trips_and_rejects_tampering() {
        let state = OAuthState {
            nonce: "random-oauth-state".to_string(),
            code_verifier: Some("a".repeat(64)),
            mode: "register".to_string(),
            return_to: "/dashboard?tab=recent".to_string(),
            issued_at: Utc::now().timestamp(),
        };
        let cookie = sign_oauth_state(&state, "test-secret").unwrap();

        assert_eq!(
            verify_oauth_state(&cookie, "test-secret").map(|decoded| decoded.nonce),
            Some("random-oauth-state".to_string())
        );
        assert!(verify_oauth_state(&cookie, "different-secret").is_none());
    }

    #[test]
    fn github_email_must_be_verified_and_primary() {
        let emails = vec![
            GithubEmail {
                email: "private@example.com".to_string(),
                primary: true,
                verified: false,
            },
            GithubEmail {
                email: "secondary@example.com".to_string(),
                primary: false,
                verified: true,
            },
            GithubEmail {
                email: "USER@Example.COM".to_string(),
                primary: true,
                verified: true,
            },
        ];

        assert_eq!(
            verified_primary_github_email(emails),
            Some("user@example.com".to_string())
        );
    }

    #[test]
    fn rejects_github_email_list_without_a_verified_primary_address() {
        let emails = vec![GithubEmail {
            email: "not-an-email".to_string(),
            primary: true,
            verified: true,
        }];

        assert_eq!(verified_primary_github_email(emails), None);
    }
}
