use tower_cookies::{
    cookie::{time::Duration as CookieDuration, SameSite},
    Cookie, Cookies,
};

use crate::{
    auth::jwt::{self, SESSION_COOKIE_NAME},
    error::AppError,
    models::User,
    state::AppState,
};

fn is_https(url: &str) -> bool {
    url.starts_with("https://")
}

/// Sets the signed-session cookie on the response after a successful
/// register/login.
pub fn set_session_cookie(cookies: &Cookies, state: &AppState, user_id: &str) -> Result<(), AppError> {
    let token = jwt::issue_token(
        user_id,
        &state.config.jwt_secret,
        state.config.jwt_session_days,
    )?;
    let cookie = Cookie::build((SESSION_COOKIE_NAME, token))
        .path("/")
        .http_only(true)
        .same_site(SameSite::Lax)
        .secure(is_https(&state.config.public_base_url))
        .max_age(CookieDuration::days(state.config.jwt_session_days))
        .build();
    cookies.add(cookie);
    Ok(())
}

/// Clears the session cookie (logout).
pub fn clear_session_cookie(cookies: &Cookies, state: &AppState) {
    let mut cookie = Cookie::new(SESSION_COOKIE_NAME, "");
    cookie.set_path("/");
    cookie.set_http_only(true);
    cookie.set_same_site(SameSite::Lax);
    cookie.set_secure(is_https(&state.config.public_base_url));
    cookies.remove(cookie);
}

/// Resolves the currently authenticated user from the session cookie, if any.
pub async fn optional_current_user(
    cookies: &Cookies,
    state: &AppState,
) -> Result<Option<User>, AppError> {
    let Some(cookie) = cookies.get(SESSION_COOKIE_NAME) else {
        return Ok(None);
    };
    let claims = match jwt::verify_token(cookie.value(), &state.config.jwt_secret) {
        Ok(claims) => claims,
        Err(_) => return Ok(None),
    };
    let user = sqlx::query_as::<_, User>("SELECT * FROM users WHERE id = $1")
        .bind(&claims.sub)
        .fetch_optional(&state.db)
        .await?;
    Ok(user)
}

/// Resolves the currently authenticated user, returning 401 if there is none.
pub async fn require_current_user(cookies: &Cookies, state: &AppState) -> Result<User, AppError> {
    optional_current_user(cookies, state)
        .await?
        .ok_or_else(|| AppError::Unauthorized("Sign in to continue.".to_string()))
}
