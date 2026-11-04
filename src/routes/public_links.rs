use std::collections::HashMap;

use axum::{
    extract::{Path, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::{DateTime, Utc};
use hmac::{Hmac, KeyInit, Mac};
use serde::Serialize;
use serde_json::Value;
use sha2::Sha256;
use tower_cookies::Cookies;

use crate::{
    assets,
    auth::session,
    authz::{self, BoardRole},
    editor::store,
    error::{AppError, AppResult},
    state::AppState,
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/api/boards/{board_id}/public-link",
            get(get_public_link)
                .put(enable_public_link)
                .delete(revoke_public_link),
        )
        .route(
            "/api/public/boards/{board_id}/{token}",
            get(get_public_board),
        )
        .route(
            "/api/public/boards/{board_id}/{token}/assets/{asset_id}",
            get(get_public_asset),
        )
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PublicLink {
    enabled: bool,
    token: Option<String>,
}

fn private_link_response(link: PublicLink) -> Response {
    (
        StatusCode::OK,
        [(header::CACHE_CONTROL, "no-store")],
        Json(link),
    )
        .into_response()
}

async fn get_public_link(
    State(state): State<AppState>,
    cookies: Cookies,
    Path(board_id): Path<String>,
) -> AppResult<Response> {
    require_owner(&state, &cookies, &board_id).await?;
    let nonce = sqlx::query_scalar::<_, String>(
        "SELECT token_nonce FROM board_public_links WHERE board_id = $1",
    )
    .bind(&board_id)
    .fetch_optional(&state.db)
    .await?;
    let token = nonce.map(|nonce| public_token(&state, &board_id, &nonce));
    Ok(private_link_response(PublicLink {
        enabled: token.is_some(),
        token,
    }))
}

async fn enable_public_link(
    State(state): State<AppState>,
    cookies: Cookies,
    Path(board_id): Path<String>,
) -> AppResult<Response> {
    let access = require_owner(&state, &cookies, &board_id).await?;
    if !store::owner_is_pro(&state.db, &access.owner_id).await? {
        return Err(AppError::Forbidden(
            "Public sharing requires an active Pro plan.".into(),
        ));
    }

    let nonce = nanoid::nanoid!(32);
    sqlx::query(
        "INSERT INTO board_public_links (board_id, token_nonce, created_by_id)
         VALUES ($1, $2, $3) ON CONFLICT (board_id) DO NOTHING",
    )
    .bind(&board_id)
    .bind(nonce)
    .bind(&access.owner_id)
    .execute(&state.db)
    .await?;

    let nonce = sqlx::query_scalar::<_, String>(
        "SELECT token_nonce FROM board_public_links WHERE board_id = $1",
    )
    .bind(&board_id)
    .fetch_one(&state.db)
    .await?;
    Ok(private_link_response(PublicLink {
        enabled: true,
        token: Some(public_token(&state, &board_id, &nonce)),
    }))
}

fn public_token(state: &AppState, board_id: &str, nonce: &str) -> String {
    derive_public_token(state.config.jwt_secret.as_bytes(), board_id, nonce)
}

fn derive_public_token(secret: &[u8], board_id: &str, nonce: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("HMAC accepts keys of any length");
    mac.update(b"sketchflow-public-board:");
    mac.update(board_id.as_bytes());
    mac.update(b":");
    mac.update(nonce.as_bytes());
    URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
}

fn verify_public_token(state: &AppState, board_id: &str, nonce: &str, token: &str) -> bool {
    verify_token(state.config.jwt_secret.as_bytes(), board_id, nonce, token)
}

fn verify_token(secret: &[u8], board_id: &str, nonce: &str, token: &str) -> bool {
    let Ok(token) = URL_SAFE_NO_PAD.decode(token) else {
        return false;
    };
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("HMAC accepts keys of any length");
    mac.update(b"sketchflow-public-board:");
    mac.update(board_id.as_bytes());
    mac.update(b":");
    mac.update(nonce.as_bytes());
    mac.verify_slice(&token).is_ok()
}

async fn revoke_public_link(
    State(state): State<AppState>,
    cookies: Cookies,
    Path(board_id): Path<String>,
) -> AppResult<Json<PublicLink>> {
    require_owner(&state, &cookies, &board_id).await?;
    sqlx::query("DELETE FROM board_public_links WHERE board_id = $1")
        .bind(&board_id)
        .execute(&state.db)
        .await?;
    Ok(Json(PublicLink {
        enabled: false,
        token: None,
    }))
}

async fn require_owner(
    state: &AppState,
    cookies: &Cookies,
    board_id: &str,
) -> AppResult<authz::BoardAccess> {
    let user = session::require_current_user(cookies, state).await?;
    authz::require_board_role(&state.db, board_id, &user.id, BoardRole::Owner).await
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PublicBoard {
    id: String,
    name: String,
    elements: Vec<Value>,
    comments: Vec<PublicComment>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PublicComment {
    id: String,
    parent_id: Option<String>,
    author_name: Option<String>,
    body: String,
    x: f64,
    y: f64,
    resolved: bool,
    created_at: DateTime<Utc>,
    reactions: Vec<PublicReaction>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PublicReaction {
    emoji: String,
    count: i64,
}

async fn get_public_board(
    State(state): State<AppState>,
    Path((board_id, token)): Path<(String, String)>,
) -> AppResult<Response> {
    let mut snapshot = state.db.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *snapshot)
        .await?;
    let link = sqlx::query_as::<_, (String, String, String)>(
        "SELECT b.id, b.name, link.token_nonce
         FROM board_public_links link
         JOIN boards b ON b.id = link.board_id
         WHERE link.board_id = $1 AND b.deleted_at IS NULL",
    )
    .bind(&board_id)
    .fetch_optional(&mut *snapshot)
    .await?
    .ok_or_else(|| AppError::NotFound("Public board not found.".into()))?;

    if !verify_public_token(&state, &board_id, &link.2, &token) {
        return Err(AppError::NotFound("Public board not found.".into()));
    }

    let elements = store::current_elements(&mut snapshot, &board_id).await?;
    let comment_rows = sqlx::query_as::<
        _,
        (
            String,
            Option<String>,
            Option<String>,
            String,
            f64,
            f64,
            bool,
            DateTime<Utc>,
        ),
    >(
        "SELECT c.id, c.parent_id, u.name, c.body, c.x, c.y, c.resolved, c.created_at
         FROM board_comments c
         JOIN users u ON u.id = c.author_id
         WHERE c.board_id = $1
         ORDER BY c.created_at ASC, c.id ASC",
    )
    .bind(&board_id)
    .fetch_all(&mut *snapshot)
    .await?;
    let reaction_rows = sqlx::query_as::<_, (String, String, i64)>(
        "SELECT r.comment_id, r.emoji, count(*)::bigint
         FROM comment_reactions r
         JOIN board_comments c ON c.id = r.comment_id
         WHERE c.board_id = $1
         GROUP BY r.comment_id, r.emoji
         ORDER BY r.emoji",
    )
    .bind(&board_id)
    .fetch_all(&mut *snapshot)
    .await?;
    snapshot.commit().await?;

    let mut reactions_by_comment: HashMap<String, Vec<PublicReaction>> = HashMap::new();
    for (comment_id, emoji, count) in reaction_rows {
        reactions_by_comment
            .entry(comment_id)
            .or_default()
            .push(PublicReaction { emoji, count });
    }
    let comments = comment_rows
        .into_iter()
        .map(
            |(id, parent_id, author_name, body, x, y, resolved, created_at)| PublicComment {
                reactions: reactions_by_comment.remove(&id).unwrap_or_default(),
                id,
                parent_id,
                author_name,
                body,
                x,
                y,
                resolved,
                created_at,
            },
        )
        .collect();

    Ok((
        StatusCode::OK,
        [
            (header::CACHE_CONTROL, "no-store"),
            (
                header::HeaderName::from_static("referrer-policy"),
                "no-referrer",
            ),
        ],
        Json(PublicBoard {
            id: link.0,
            name: link.1,
            elements,
            comments,
        }),
    )
        .into_response())
}

async fn get_public_asset(
    State(state): State<AppState>,
    Path((board_id, token, asset_id)): Path<(String, String, String)>,
) -> AppResult<Response> {
    let nonce = sqlx::query_scalar::<_, String>(
        "SELECT link.token_nonce
         FROM board_public_links link
         JOIN boards b ON b.id = link.board_id
         WHERE link.board_id = $1 AND b.deleted_at IS NULL",
    )
    .bind(&board_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Public board not found.".into()))?;
    if !verify_public_token(&state, &board_id, &nonce, &token) {
        return Err(AppError::NotFound("Public board not found.".into()));
    }
    let referenced = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (
           SELECT 1
           FROM board_elements element
           JOIN assets asset ON asset.id = $2 AND asset.status = 'complete'
           WHERE element.board_id = $1
             AND element.type = 'image'
             AND element.data->>'assetId' = $2
         )",
    )
    .bind(&board_id)
    .bind(&asset_id)
    .fetch_one(&state.db)
    .await?;
    if !referenced {
        return Err(AppError::NotFound("Image not found.".into()));
    }
    let asset = assets::read_asset(&state.db, &state.config, &asset_id)
        .await?
        .ok_or_else(|| AppError::NotFound("Image not found.".into()))?;
    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, asset.mime_type),
            (header::CACHE_CONTROL, "private, no-store".to_string()),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
            (
                header::HeaderName::from_static("referrer-policy"),
                "no-referrer".to_string(),
            ),
        ],
        asset.buffer,
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_tokens_are_bound_to_board_and_link_nonce() {
        let token = derive_public_token(b"test secret", "board-1", "nonce-1");
        assert!(verify_token(b"test secret", "board-1", "nonce-1", &token));
        assert!(!verify_token(b"test secret", "board-2", "nonce-1", &token));
        assert!(!verify_token(b"test secret", "board-1", "nonce-2", &token));
        assert!(!verify_token(
            b"test secret",
            "board-1",
            "nonce-1",
            "not-a-token"
        ));
    }
}
