use axum::{
    extract::{Form, Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
    Json, Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tower_cookies::Cookies;
use uuid::Uuid;

use crate::{
    auth::session,
    error::{AppError, AppResult},
    state::AppState,
};

use super::{MCP_READ_SCOPE, MCP_WRITE_SCOPE};

const AUTHORIZATION_REQUEST_LIFETIME_MINUTES: i64 = 10;
const AUTHORIZATION_CODE_LIFETIME_SECONDS: i64 = 60;
const ACCESS_TOKEN_LIFETIME_SECONDS: i64 = 3600;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route(
            "/.well-known/oauth-protected-resource",
            get(protected_resource_metadata),
        )
        .route(
            "/.well-known/oauth-protected-resource/api/mcp",
            get(protected_resource_metadata),
        )
        .route(
            "/.well-known/oauth-authorization-server",
            get(authorization_server_metadata),
        )
        .route("/api/mcp/oauth/register", post(register_client))
        .route("/api/mcp/oauth/authorize", get(authorize).post(consent))
        .route("/api/mcp/oauth/token", post(token))
        .route("/api/mcp/oauth/revoke", post(revoke_token))
        .route("/api/mcp/oauth/grants", get(list_grants))
        .route(
            "/api/mcp/oauth/grants/{grant_id}",
            axum::routing::delete(revoke_grant),
        )
        .with_state(state)
}

fn valid_pkce_challenge(challenge: &str) -> bool {
    challenge.len() == 43
        && URL_SAFE_NO_PAD
            .decode(challenge)
            .is_ok_and(|decoded| decoded.len() == 32)
}

fn valid_pkce_verifier(verifier: &str) -> bool {
    (43..=128).contains(&verifier.len())
        && verifier
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~'))
}

fn mcp_resource(state: &AppState) -> String {
    format!(
        "{}/api/mcp",
        state.config.public_base_url.trim_end_matches('/')
    )
}

async fn protected_resource_metadata(State(state): State<AppState>) -> Json<Value> {
    Json(json!({
        "resource": mcp_resource(&state),
        "authorization_servers": [state.config.public_base_url.trim_end_matches('/')],
        "scopes_supported": [MCP_READ_SCOPE, MCP_WRITE_SCOPE],
        "bearer_methods_supported": ["header"],
    }))
}

async fn authorization_server_metadata(State(state): State<AppState>) -> Json<Value> {
    let base = state.config.public_base_url.trim_end_matches('/');
    Json(json!({
        "issuer": base,
        "authorization_endpoint": format!("{base}/api/mcp/oauth/authorize"),
        "token_endpoint": format!("{base}/api/mcp/oauth/token"),
        "registration_endpoint": format!("{base}/api/mcp/oauth/register"),
        "revocation_endpoint": format!("{base}/api/mcp/oauth/revoke"),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"],
        "scopes_supported": [MCP_READ_SCOPE, MCP_WRITE_SCOPE],
    }))
}

#[derive(Debug, Deserialize)]
struct ClientRegistration {
    client_name: String,
    redirect_uris: Vec<String>,
    grant_types: Option<Vec<String>>,
    response_types: Option<Vec<String>>,
    token_endpoint_auth_method: Option<String>,
}

#[derive(Debug, Serialize)]
struct ClientRegistrationResponse {
    client_id: String,
    client_id_issued_at: i64,
    client_name: String,
    redirect_uris: Vec<String>,
    grant_types: Vec<String>,
    response_types: Vec<String>,
    token_endpoint_auth_method: &'static str,
}

async fn register_client(
    State(state): State<AppState>,
    Json(input): Json<ClientRegistration>,
) -> AppResult<impl IntoResponse> {
    let client_name = input.client_name.trim();
    if client_name.is_empty() || client_name.chars().count() > 120 {
        return Err(AppError::BadRequest(
            "Client name must contain 1 to 120 characters.".into(),
        ));
    }
    if input.redirect_uris.is_empty()
        || input.redirect_uris.len() > 20
        || input
            .redirect_uris
            .iter()
            .any(|uri| uri.len() > 256 || !valid_redirect_uri(uri))
    {
        return Err(AppError::BadRequest(
            "The client must register 1 to 20 valid redirect URIs.".into(),
        ));
    }
    if input
        .grant_types
        .as_ref()
        .is_some_and(|values| values != &["authorization_code"])
        || input
            .response_types
            .as_ref()
            .is_some_and(|values| values != &["code"])
        || input
            .token_endpoint_auth_method
            .as_deref()
            .is_some_and(|value| value != "none")
    {
        return Err(AppError::BadRequest(
            "Only public authorization-code clients using PKCE are supported.".into(),
        ));
    }

    let client_id = Uuid::new_v4().simple().to_string();
    sqlx::query(
        "INSERT INTO mcp_oauth_clients (client_id, client_name, redirect_uris)
         VALUES ($1, $2, $3)",
    )
    .bind(&client_id)
    .bind(client_name)
    .bind(json!(input.redirect_uris))
    .execute(&state.db)
    .await?;

    Ok((
        StatusCode::CREATED,
        Json(ClientRegistrationResponse {
            client_id,
            client_id_issued_at: Utc::now().timestamp(),
            client_name: client_name.to_owned(),
            redirect_uris: input.redirect_uris,
            grant_types: vec!["authorization_code".into()],
            response_types: vec!["code".into()],
            token_endpoint_auth_method: "none",
        }),
    ))
}

fn valid_redirect_uri(value: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(value) else {
        return false;
    };
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        return false;
    }
    match url.scheme() {
        "https" => url.host_str().is_some(),
        "http" => matches!(
            url.host_str(),
            Some("localhost" | "127.0.0.1" | "[::1]" | "::1")
        ),
        _ => false,
    }
}

#[derive(Debug, Deserialize)]
struct AuthorizationQuery {
    response_type: String,
    client_id: String,
    redirect_uri: String,
    state: String,
    code_challenge: String,
    code_challenge_method: String,
    scope: String,
    resource: String,
}

#[derive(Debug, sqlx::FromRow)]
struct ClientRow {
    client_name: String,
    redirect_uris: Value,
}

async fn get_client(state: &AppState, client_id: &str) -> AppResult<ClientRow> {
    sqlx::query_as::<_, ClientRow>(
        "SELECT client_name, redirect_uris
         FROM mcp_oauth_clients WHERE client_id = $1",
    )
    .bind(client_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::BadRequest("Unknown OAuth client.".into()))
}

fn client_allows_redirect(client: &ClientRow, redirect_uri: &str) -> bool {
    client
        .redirect_uris
        .as_array()
        .is_some_and(|uris| uris.iter().any(|uri| uri.as_str() == Some(redirect_uri)))
}

fn requested_scopes(value: &str) -> AppResult<Vec<String>> {
    let scopes: Vec<_> = value.split_ascii_whitespace().collect();
    if !scopes.contains(&MCP_READ_SCOPE)
        || scopes.len() > 2
        || scopes
            .iter()
            .any(|scope| *scope != MCP_READ_SCOPE && *scope != MCP_WRITE_SCOPE)
    {
        return Err(AppError::BadRequest(
            "Supported scopes are boards:read and boards:write; boards:read is required.".into(),
        ));
    }
    let mut result = vec![MCP_READ_SCOPE.to_owned()];
    if scopes.contains(&MCP_WRITE_SCOPE) {
        result.push(MCP_WRITE_SCOPE.to_owned());
    }
    Ok(result)
}

fn authorize_url(state: &AppState, query: &AuthorizationQuery) -> AppResult<String> {
    let base = reqwest::Url::parse(&state.config.public_base_url)
        .map_err(|error| AppError::Internal(anyhow::anyhow!(error)))?;
    let mut url = base
        .join("/api/mcp/oauth/authorize")
        .map_err(|error| AppError::Internal(anyhow::anyhow!(error)))?;
    {
        let mut pairs = url.query_pairs_mut();
        pairs
            .append_pair("response_type", &query.response_type)
            .append_pair("client_id", &query.client_id)
            .append_pair("redirect_uri", &query.redirect_uri)
            .append_pair("state", &query.state)
            .append_pair("code_challenge", &query.code_challenge)
            .append_pair("code_challenge_method", &query.code_challenge_method)
            .append_pair("scope", &query.scope)
            .append_pair("resource", &query.resource);
    }
    Ok(url.into())
}

fn redirect_to_login(state: &AppState, callback: &str) -> AppResult<Redirect> {
    let mut login = reqwest::Url::parse(&format!(
        "{}/login",
        state.config.frontend_base_url.trim_end_matches('/')
    ))
    .map_err(|error| AppError::Internal(anyhow::anyhow!(error)))?;
    login.query_pairs_mut().append_pair("callbackUrl", callback);
    Ok(Redirect::temporary(login.as_str()))
}

async fn authorize(
    State(state): State<AppState>,
    cookies: Cookies,
    Query(query): Query<AuthorizationQuery>,
) -> AppResult<Response> {
    let client = get_client(&state, &query.client_id).await?;
    if query.response_type != "code"
        || !valid_redirect_uri(&query.redirect_uri)
        || !client_allows_redirect(&client, &query.redirect_uri)
        || query.code_challenge_method != "S256"
        || query.state.is_empty()
        || query.state.len() > 256
        || query.scope.len() > 128
        || query.redirect_uri.len() > 256
        || !valid_pkce_challenge(&query.code_challenge)
        || query.resource != mcp_resource(&state)
    {
        return Err(AppError::BadRequest(
            "Invalid OAuth authorization request.".into(),
        ));
    }
    let scopes = requested_scopes(&query.scope)?;
    let Some(user) = session::optional_current_user(&cookies, &state).await? else {
        return Ok(redirect_to_login(&state, &authorize_url(&state, &query)?)?.into_response());
    };

    let request_id = Uuid::new_v4().simple().to_string();
    sqlx::query(
        "INSERT INTO mcp_oauth_pending_authorizations
            (id, user_id, client_id, redirect_uri, state, code_challenge, scopes, expires_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, now() + ($8 * interval '1 minute'))",
    )
    .bind(&request_id)
    .bind(&user.id)
    .bind(&query.client_id)
    .bind(&query.redirect_uri)
    .bind(&query.state)
    .bind(&query.code_challenge)
    .bind(&scopes)
    .bind(AUTHORIZATION_REQUEST_LIFETIME_MINUTES)
    .execute(&state.db)
    .await?;

    let write_scope = if scopes.iter().any(|scope| scope == MCP_WRITE_SCOPE) {
        format!(
            "<label><input type=\"checkbox\" name=\"allow_write\" value=\"yes\"> Allow this client to draw and edit canvas elements</label>"
        )
    } else {
        String::new()
    };
    let html = format!(
        "<!doctype html><html lang=\"en\"><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <title>Authorize MCP client</title>\
         <style>body{{font:16px system-ui;max-width:38rem;margin:10vh auto;padding:0 1rem;color:#1c1917}}\
         button{{padding:.6rem 1rem;margin:1rem .5rem 0 0}}\
         label{{display:block;margin:1rem 0}}</style>\
         <h1>Connect an AI client</h1>\
         <p><strong>{}</strong> wants access to your SketchFlow boards.</p>\
         <p>Read access is required. Drawing and editing access is optional.</p>\
         <form method=\"post\" action=\"/api/mcp/oauth/authorize\">\
         <input type=\"hidden\" name=\"request_id\" value=\"{}\">\
         {}\
         <button type=\"submit\" name=\"decision\" value=\"approve\">Allow</button>\
         <button type=\"submit\" name=\"decision\" value=\"deny\">Deny</button>\
         </form></html>",
        html_escape(&client.client_name),
        request_id,
        write_scope
    );
    let mut response = Html(html).into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        "no-store".parse().expect("static header is valid"),
    );
    response.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'"
            .parse()
            .expect("static header is valid"),
    );
    Ok(response)
}

#[derive(Debug, Deserialize)]
struct ConsentForm {
    request_id: String,
    decision: String,
    allow_write: Option<String>,
}

#[derive(Debug, sqlx::FromRow)]
struct PendingAuthorization {
    client_id: String,
    redirect_uri: String,
    state: String,
    code_challenge: String,
    scopes: Vec<String>,
}

async fn consent(
    State(state): State<AppState>,
    cookies: Cookies,
    headers: HeaderMap,
    Form(form): Form<ConsentForm>,
) -> AppResult<Response> {
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| AppError::Forbidden("Missing consent origin.".into()))?;
    let expected_origin = reqwest::Url::parse(&state.config.public_base_url)
        .map_err(|error| AppError::Internal(anyhow::anyhow!(error)))?
        .origin()
        .ascii_serialization();
    if origin != expected_origin {
        return Err(AppError::Forbidden("Invalid consent origin.".into()));
    }
    let user = session::require_current_user(&cookies, &state).await?;
    let mut tx = state.db.begin().await?;
    let pending = sqlx::query_as::<_, PendingAuthorization>(
        "SELECT client_id, redirect_uri, state, code_challenge, scopes
         FROM mcp_oauth_pending_authorizations
         WHERE id = $1 AND user_id = $2 AND expires_at > now()
         FOR UPDATE",
    )
    .bind(&form.request_id)
    .bind(&user.id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| AppError::BadRequest("Authorization request expired.".into()))?;

    tracing::info!(
        subsystem = "mcp_oauth",
        event = "oauth_consent_submitted",
        client_id = %pending.client_id,
        redirect_uri = %pending.redirect_uri,
        state_present = !pending.state.is_empty(),
        scope = %pending.scopes.join(" "),
        "OAuth consent submitted"
    );

    if form.decision == "deny" {
        sqlx::query("DELETE FROM mcp_oauth_pending_authorizations WHERE id = $1")
            .bind(&form.request_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        return Ok(no_store(
            Redirect::to(&oauth_redirect(
                &pending.redirect_uri,
                &[("error", "access_denied"), ("state", &pending.state)],
            )?)
            .into_response(),
        ));
    }
    if form.decision != "approve" {
        return Err(AppError::BadRequest("Invalid consent decision.".into()));
    }
    let scopes: Vec<String> = pending
        .scopes
        .iter()
        .filter(|scope| {
            scope.as_str() == MCP_READ_SCOPE
                || (scope.as_str() == MCP_WRITE_SCOPE && form.allow_write.as_deref() == Some("yes"))
        })
        .cloned()
        .collect();
    if !scopes.iter().any(|scope| scope == MCP_READ_SCOPE) {
        return Err(AppError::BadRequest("Read access is required.".into()));
    }

    let grant_id = Uuid::new_v4().simple().to_string();
    sqlx::query(
        "INSERT INTO mcp_oauth_grants (id, user_id, client_id, scopes)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(&grant_id)
    .bind(&user.id)
    .bind(&pending.client_id)
    .bind(&scopes)
    .execute(&mut *tx)
    .await?;

    let code = Uuid::new_v4().simple().to_string();
    sqlx::query(
        "INSERT INTO mcp_oauth_codes
            (code_hash, grant_id, client_id, redirect_uri, code_challenge, expires_at)
         VALUES ($1, $2, $3, $4, $5, now() + ($6 * interval '1 second'))",
    )
    .bind(hash_secret(&code))
    .bind(&grant_id)
    .bind(&pending.client_id)
    .bind(&pending.redirect_uri)
    .bind(&pending.code_challenge)
    .bind(AUTHORIZATION_CODE_LIFETIME_SECONDS)
    .execute(&mut *tx)
    .await?;
    tracing::info!(
        subsystem = "mcp_oauth",
        event = "oauth_authorization_code_created",
        client_id = %pending.client_id,
        redirect_uri = %pending.redirect_uri,
        "Authorization code generated"
    );
    sqlx::query("DELETE FROM mcp_oauth_pending_authorizations WHERE id = $1")
        .bind(&form.request_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    tracing::info!(
        subsystem = "mcp_oauth",
        event = "oauth_authorization_redirect",
        redirect_uri = %pending.redirect_uri,
        "Redirecting to OAuth client callback"
    );
    Ok(no_store(
        Redirect::to(&oauth_redirect(
            &pending.redirect_uri,
            &[("code", &code), ("state", &pending.state)],
        )?)
        .into_response(),
    ))
}

#[derive(Debug, Deserialize)]
struct TokenForm {
    grant_type: String,
    code: String,
    redirect_uri: String,
    client_id: String,
    code_verifier: String,
    resource: String,
}

#[derive(Debug, sqlx::FromRow)]
struct AuthorizationCode {
    grant_id: String,
    code_challenge: String,
    scopes: Vec<String>,
}

#[derive(Debug, Serialize)]
struct TokenResponse {
    access_token: String,
    token_type: &'static str,
    expires_in: i64,
    scope: String,
}

async fn token(State(state): State<AppState>, Form(form): Form<TokenForm>) -> Response {
    match issue_token(&state, form).await {
        Ok(response) => (
            [
                (header::CACHE_CONTROL, "no-store"),
                (header::PRAGMA, "no-cache"),
            ],
            Json(response),
        )
            .into_response(),
        Err((status, error, description)) => oauth_error(status, error, description),
    }
}

async fn issue_token(
    state: &AppState,
    form: TokenForm,
) -> Result<TokenResponse, (StatusCode, &'static str, &'static str)> {
    if form.grant_type != "authorization_code"
        || form.resource != mcp_resource(state)
        || !valid_pkce_verifier(&form.code_verifier)
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "Invalid authorization-code token request.",
        ));
    }
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(form.code_verifier.as_bytes()));
    let code_hash = hash_secret(&form.code);
    let mut tx = state.db.begin().await.map_err(|error| {
        tracing::error!(subsystem = "mcp_oauth", event = "token_transaction_failed", %error);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            "Token service unavailable.",
        )
    })?;
    let code = sqlx::query_as::<_, AuthorizationCode>(
        "SELECT c.grant_id, c.code_challenge, g.scopes
         FROM mcp_oauth_codes c
         JOIN mcp_oauth_grants g ON g.id = c.grant_id
         WHERE c.code_hash = $1 AND c.client_id = $2 AND c.redirect_uri = $3
           AND c.expires_at > now() AND c.consumed_at IS NULL AND g.revoked_at IS NULL
         FOR UPDATE OF c",
    )
    .bind(&code_hash)
    .bind(&form.client_id)
    .bind(&form.redirect_uri)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|error| {
        tracing::error!(subsystem = "mcp_oauth", event = "code_lookup_failed", %error);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            "Token service unavailable.",
        )
    })?
    .ok_or((
        StatusCode::BAD_REQUEST,
        "invalid_grant",
        "Authorization code is invalid or expired.",
    ))?;
    if code.code_challenge != challenge {
        return Err((
            StatusCode::BAD_REQUEST,
            "invalid_grant",
            "PKCE verification failed.",
        ));
    }
    sqlx::query("UPDATE mcp_oauth_codes SET consumed_at = now() WHERE code_hash = $1")
        .bind(&code_hash)
        .execute(&mut *tx)
        .await
        .map_err(|error| {
            tracing::error!(subsystem = "mcp_oauth", event = "code_consume_failed", %error);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                "Token service unavailable.",
            )
        })?;

    let access_token = format!("skf_mcp_{}", Uuid::new_v4().simple());
    sqlx::query(
        "INSERT INTO mcp_oauth_access_tokens (token_hash, grant_id, expires_at)
         VALUES ($1, $2, now() + ($3 * interval '1 second'))",
    )
    .bind(hash_secret(&access_token))
    .bind(&code.grant_id)
    .bind(ACCESS_TOKEN_LIFETIME_SECONDS)
    .execute(&mut *tx)
    .await
    .map_err(|error| {
        tracing::error!(subsystem = "mcp_oauth", event = "token_store_failed", %error);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            "Token service unavailable.",
        )
    })?;
    tx.commit().await.map_err(|error| {
        tracing::error!(subsystem = "mcp_oauth", event = "token_commit_failed", %error);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            "Token service unavailable.",
        )
    })?;
    Ok(TokenResponse {
        access_token,
        token_type: "Bearer",
        expires_in: ACCESS_TOKEN_LIFETIME_SECONDS,
        scope: code.scopes.join(" "),
    })
}

#[derive(Debug, Deserialize)]
struct RevokeTokenForm {
    token: String,
    client_id: String,
}

async fn revoke_token(
    State(state): State<AppState>,
    Form(form): Form<RevokeTokenForm>,
) -> Response {
    if let Err(error) = sqlx::query(
        "UPDATE mcp_oauth_access_tokens t
         SET revoked_at = COALESCE(t.revoked_at, now())
         FROM mcp_oauth_grants g
         WHERE t.grant_id = g.id AND t.token_hash = $1 AND g.client_id = $2",
    )
    .bind(hash_secret(&form.token))
    .bind(form.client_id)
    .execute(&state.db)
    .await
    {
        tracing::error!(subsystem = "mcp_oauth", event = "token_revocation_failed", %error);
        return oauth_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            "Token revocation failed.",
        );
    }
    StatusCode::OK.into_response()
}

#[derive(Debug, Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
struct GrantSummary {
    id: String,
    client_name: String,
    scopes: Vec<String>,
    created_at: chrono::DateTime<Utc>,
}

async fn list_grants(
    State(state): State<AppState>,
    cookies: Cookies,
) -> AppResult<Json<Vec<GrantSummary>>> {
    let user = session::require_current_user(&cookies, &state).await?;
    let grants = sqlx::query_as::<_, GrantSummary>(
        "SELECT g.id, c.client_name, g.scopes, g.created_at
         FROM mcp_oauth_grants g
         JOIN mcp_oauth_clients c ON c.client_id = g.client_id
         WHERE g.user_id = $1 AND g.revoked_at IS NULL
         ORDER BY g.created_at DESC LIMIT 100",
    )
    .bind(user.id)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(grants))
}

async fn revoke_grant(
    State(state): State<AppState>,
    cookies: Cookies,
    Path(grant_id): Path<String>,
) -> AppResult<StatusCode> {
    let user = session::require_current_user(&cookies, &state).await?;
    sqlx::query(
        "UPDATE mcp_oauth_grants SET revoked_at = COALESCE(revoked_at, now())
         WHERE id = $1 AND user_id = $2",
    )
    .bind(grant_id)
    .bind(user.id)
    .execute(&state.db)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

fn oauth_redirect(redirect_uri: &str, pairs: &[(&str, &str)]) -> AppResult<String> {
    let mut url = reqwest::Url::parse(redirect_uri)
        .map_err(|error| AppError::BadRequest(format!("Invalid redirect URI: {error}")))?;
    url.query_pairs_mut().extend_pairs(pairs.iter().copied());
    Ok(url.into())
}

fn oauth_error(status: StatusCode, error: &str, description: &str) -> Response {
    (
        status,
        [
            (header::CACHE_CONTROL, "no-store"),
            (header::PRAGMA, "no-cache"),
        ],
        Json(json!({
            "error": error,
            "error_description": description,
        })),
    )
        .into_response()
}

fn no_store(mut response: Response) -> Response {
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        "no-store".parse().expect("static header is valid"),
    );
    response
}

fn hash_secret(secret: &str) -> String {
    hex::encode(Sha256::digest(secret.as_bytes()))
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirect_uri_allows_https_and_loopback_http_only() {
        assert!(valid_redirect_uri("https://agent.example/callback"));
        assert!(valid_redirect_uri("http://127.0.0.1:49152/callback"));
        assert!(valid_redirect_uri("http://localhost/callback"));
        assert!(!valid_redirect_uri("http://agent.example/callback"));
        assert!(!valid_redirect_uri(
            "https://user:pass@agent.example/callback"
        ));
        assert!(!valid_redirect_uri(
            "https://agent.example/callback#fragment"
        ));
        assert!(!valid_redirect_uri("javascript:alert(1)"));
    }

    #[test]
    fn scope_request_requires_read_and_rejects_unknown_scopes() {
        assert_eq!(
            requested_scopes("boards:read boards:write").unwrap(),
            vec![MCP_READ_SCOPE, MCP_WRITE_SCOPE]
        );
        assert!(requested_scopes("boards:write").is_err());
        assert!(requested_scopes("boards:read admin").is_err());
    }

    #[test]
    fn pkce_s256_uses_unpadded_base64url() {
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(b"valid-code-verifier"));
        assert!(!challenge.contains('='));
        assert_eq!(challenge.len(), 43);
        assert!(valid_pkce_challenge(&challenge));
        assert!(!valid_pkce_challenge(&format!("{}!", &challenge[..42])));
        assert!(valid_pkce_verifier(
            "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ012"
        ));
        assert!(!valid_pkce_verifier("short"));
        assert!(!valid_pkce_verifier(
            "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ012!"
        ));
    }

    #[test]
    fn client_names_are_html_escaped() {
        assert_eq!(
            html_escape("<agent & \"client\">"),
            "&lt;agent &amp; &quot;client&quot;&gt;"
        );
    }
}
