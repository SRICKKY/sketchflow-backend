use std::collections::HashMap;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::{get, patch, post, put},
    Json, Router,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tower_cookies::Cookies;

use crate::{
    auth::session,
    authz::{self, BoardRole},
    editor::{schema, store},
    error::{AppError, AppResult},
    extract::AppJson,
    routes::collaboration::{self, ElementBatch},
    routes::notifications::{self, NewNotification, NotificationType},
    state::AppState,
};

const COMMENT_EMOJIS: [&str; 11] = [
    "👍", "👀", "😊", "❤️", "🎉", "🙏", "✅", "🔥", "🚀", "👏", "😕",
];

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/boards/{board_id}/detail", get(board_detail))
        .route("/api/boards/{board_id}/elements", patch(patch_elements))
        .route("/api/boards/{board_id}/document", put(put_document))
        .route(
            "/api/boards/{board_id}/presentation",
            patch(patch_presentation),
        )
        .route(
            "/api/boards/{board_id}/versions",
            get(list_versions).post(create_version),
        )
        .route(
            "/api/boards/{board_id}/versions/{version_id}/restore",
            post(restore_version),
        )
        .route(
            "/api/boards/{board_id}/comments",
            get(list_comments).post(create_comment),
        )
        .route(
            "/api/boards/{board_id}/comments/{comment_id}",
            patch(update_comment).delete(delete_comment),
        )
        .route(
            "/api/boards/{board_id}/comments/{comment_id}/reactions",
            post(toggle_reaction),
        )
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BoardDetail {
    id: String,
    name: String,
    workspace_name: Option<String>,
    owner_id: String,
    role: &'static str,
    updated_at: DateTime<Utc>,
    created_at: DateTime<Utc>,
    elements: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    presentation: Option<Value>,
    members: Vec<BoardMember>,
    owner_is_pro: bool,
    revision: i64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BoardMember {
    id: String,
    user_id: String,
    email: String,
    image: Option<String>,
    name: Option<String>,
    role: String,
}

async fn board_detail(
    State(state): State<AppState>,
    cookies: Cookies,
    Path(board_id): Path<String>,
) -> AppResult<Json<BoardDetail>> {
    let user = session::require_current_user(&cookies, &state).await?;
    let access =
        authz::require_board_role(&state.db, &board_id, &user.id, BoardRole::Viewer).await?;
    let mut snapshot = state.db.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *snapshot)
        .await?;
    let row = sqlx::query_as::<
        _,
        (
            String,
            String,
            String,
            Option<String>,
            DateTime<Utc>,
            DateTime<Utc>,
            Option<Value>,
            i64,
        ),
    >(
        "SELECT b.id, b.name, b.owner_id, w.name, b.updated_at, b.created_at,
                b.presentation, COALESCE(s.revision, 0)
         FROM boards b
         LEFT JOIN workspaces w ON w.id = b.workspace_id
         LEFT JOIN board_collaboration_state s ON s.board_id = b.id
         WHERE b.id = $1",
    )
    .bind(&board_id)
    .fetch_optional(&mut *snapshot)
    .await?
    .ok_or_else(|| AppError::NotFound("Board not found".into()))?;
    let members = sqlx::query_as::<
        _,
        (
            String,
            String,
            String,
            Option<String>,
            Option<String>,
            String,
        ),
    >(
        "SELECT COALESCE(m.id, owner.id), owner.id, owner.email, owner.image, owner.name,
                'OWNER'
         FROM boards b
         JOIN users owner ON owner.id = b.owner_id
         LEFT JOIN board_members m ON m.board_id = b.id AND m.user_id = owner.id
         WHERE b.id = $1
         UNION ALL
         SELECT m.id, u.id, u.email, u.image, u.name, m.role::text
         FROM board_members m
         JOIN users u ON u.id = m.user_id
         JOIN boards b ON b.id = m.board_id
         WHERE m.board_id = $1 AND m.user_id <> b.owner_id
         ORDER BY 6, 3",
    )
    .bind(&board_id)
    .fetch_all(&mut *snapshot)
    .await?
    .into_iter()
    .map(|(id, user_id, email, image, name, role)| BoardMember {
        id,
        user_id,
        email,
        image,
        name,
        role,
    })
    .collect();
    let elements = store::current_elements(&mut snapshot, &board_id).await?;
    snapshot.commit().await?;
    let owner_is_pro = store::owner_is_pro(&state.db, &access.owner_id).await?;
    Ok(Json(BoardDetail {
        id: row.0,
        name: row.1,
        owner_id: row.2,
        workspace_name: row.3,
        updated_at: row.4,
        created_at: row.5,
        presentation: row.6,
        revision: row.7,
        members,
        elements,
        role: role_name(access.role),
        owner_is_pro,
    }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PatchElementsInput {
    base_revision: i64,
    client_operation_id: String,
    upserts: Vec<Value>,
    deleted_ids: Vec<String>,
}

async fn patch_elements(
    State(state): State<AppState>,
    cookies: Cookies,
    Path(board_id): Path<String>,
    AppJson(input): AppJson<PatchElementsInput>,
) -> AppResult<Json<Value>> {
    let user = session::require_current_user(&cookies, &state).await?;
    authz::require_board_role(&state.db, &board_id, &user.id, BoardRole::Editor).await?;
    store::validate_client_operation_id(&input.client_operation_id)?;
    let upserts = schema::elements(&input.upserts, "Invalid board data")?;
    let batch = collaboration::validate_operation(
        &input.client_operation_id,
        ElementBatch {
            upserts,
            deleted_ids: input.deleted_ids,
        },
    )?;
    let operation = store::apply_patch(
        &state.db,
        &board_id,
        &user.id,
        &input.client_operation_id,
        Some(input.base_revision),
        batch,
    )
    .await?;
    collaboration::broadcast_editor_operation(&state.collaboration, &board_id, &operation, false)
        .await;
    if !operation.replayed {
        store::maybe_create_auto_version(&state.db, &board_id, &user.id).await;
    }
    Ok(Json(json!({ "revision": operation.revision })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DocumentInput {
    base_revision: i64,
    client_operation_id: String,
    elements: Vec<Value>,
    presentation: Option<Value>,
}

async fn put_document(
    State(state): State<AppState>,
    cookies: Cookies,
    Path(board_id): Path<String>,
    AppJson(input): AppJson<DocumentInput>,
) -> AppResult<Json<Value>> {
    let user = session::require_current_user(&cookies, &state).await?;
    authz::require_board_role(&state.db, &board_id, &user.id, BoardRole::Editor).await?;
    store::validate_client_operation_id(&input.client_operation_id)?;
    let elements = schema::elements(&input.elements, "Invalid board data")?;
    if elements.len() > 5_000 {
        return Err(AppError::BadRequest(
            "Boards support at most 5000 elements.".into(),
        ));
    }
    let presentation = input
        .presentation
        .as_ref()
        .map(schema::presentation)
        .transpose()?;
    let mut tx = state.db.begin().await?;
    let current = store::lock_revision(&mut tx, &board_id).await?;
    let request = json!({
        "kind": "document",
        "elements": elements,
        "presentation": presentation,
    });
    let operation = store::replace_elements(
        &mut tx,
        &state.db,
        &board_id,
        current,
        &user.id,
        &input.client_operation_id,
        Some(input.base_revision),
        elements,
        &request,
    )
    .await?;
    if let Some(presentation) = presentation {
        sqlx::query("UPDATE boards SET presentation = $2 WHERE id = $1")
            .bind(&board_id)
            .bind(presentation)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    collaboration::broadcast_editor_operation(
        &state.collaboration,
        &board_id,
        &operation,
        input.presentation.is_some(),
    )
    .await;
    if !operation.replayed {
        store::maybe_create_auto_version(&state.db, &board_id, &user.id).await;
    }
    Ok(Json(json!({ "revision": operation.revision })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PresentationInput {
    base_revision: i64,
    client_operation_id: String,
    presentation: Value,
}

async fn patch_presentation(
    State(state): State<AppState>,
    cookies: Cookies,
    Path(board_id): Path<String>,
    AppJson(input): AppJson<PresentationInput>,
) -> AppResult<Json<Value>> {
    let user = session::require_current_user(&cookies, &state).await?;
    authz::require_board_role(&state.db, &board_id, &user.id, BoardRole::Editor).await?;
    store::validate_client_operation_id(&input.client_operation_id)?;
    let presentation = schema::presentation(&input.presentation)?;
    let operation = store::save_presentation(
        &state.db,
        &board_id,
        &user.id,
        &input.client_operation_id,
        input.base_revision,
        presentation,
    )
    .await?;
    collaboration::broadcast_editor_operation(&state.collaboration, &board_id, &operation, true)
        .await;
    if !operation.replayed {
        store::maybe_create_auto_version(&state.db, &board_id, &user.id).await;
    }
    Ok(Json(json!({ "revision": operation.revision })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CreateVersionInput {
    label: Option<String>,
}

async fn list_versions(
    State(state): State<AppState>,
    cookies: Cookies,
    Path(board_id): Path<String>,
) -> AppResult<Json<Vec<Value>>> {
    let user = session::require_current_user(&cookies, &state).await?;
    let access =
        authz::require_board_role(&state.db, &board_id, &user.id, BoardRole::Viewer).await?;
    let cap = store::version_cap(store::owner_is_pro(&state.db, &access.owner_id).await?) as i64;
    let rows = sqlx::query_as::<
        _,
        (
            String,
            Option<String>,
            String,
            i32,
            DateTime<Utc>,
            Option<String>,
        ),
    >(
        "SELECT v.id, v.label, v.source, v.element_count, v.created_at, u.name
         FROM board_versions v JOIN users u ON u.id = v.created_by_id
         WHERE v.board_id = $1
         ORDER BY v.created_at DESC LIMIT $2",
    )
    .bind(&board_id)
    .bind(cap)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|(id, label, source, count, created_at, name)| {
                json!({
                    "id": id,
                    "label": label,
                    "source": source,
                    "elementCount": count,
                    "createdAt": created_at,
                    "createdByName": name,
                })
            })
            .collect(),
    ))
}

async fn create_version(
    State(state): State<AppState>,
    cookies: Cookies,
    Path(board_id): Path<String>,
    AppJson(input): AppJson<CreateVersionInput>,
) -> AppResult<StatusCode> {
    let user = session::require_current_user(&cookies, &state).await?;
    authz::require_board_role(&state.db, &board_id, &user.id, BoardRole::Editor).await?;
    let label = input.label.map(|label| label.trim().to_owned());
    if label
        .as_ref()
        .is_some_and(|label| label.chars().count() > 80)
    {
        return Err(AppError::BadRequest("Version name is too long.".into()));
    }
    let mut tx = state.db.begin().await?;
    store::lock_revision(&mut tx, &board_id).await?;
    let created = store::snapshot_board(
        &mut tx,
        &state.db,
        &board_id,
        &user.id,
        "manual",
        label.as_deref(),
    )
    .await?;
    tx.commit().await?;
    if created.is_none() {
        return Err(AppError::BadRequest(
            "Cannot save a version of an empty board.".into(),
        ));
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RestoreVersionInput {
    base_revision: i64,
    client_operation_id: String,
}

async fn restore_version(
    State(state): State<AppState>,
    cookies: Cookies,
    Path((board_id, version_id)): Path<(String, String)>,
    AppJson(input): AppJson<RestoreVersionInput>,
) -> AppResult<Json<Value>> {
    let user = session::require_current_user(&cookies, &state).await?;
    let access =
        authz::require_board_role(&state.db, &board_id, &user.id, BoardRole::Editor).await?;
    store::validate_client_operation_id(&input.client_operation_id)?;
    let cap = store::version_cap(store::owner_is_pro(&state.db, &access.owner_id).await?) as i64;
    let version = sqlx::query_as::<_, (sqlx::types::Json<Vec<Value>>, Option<Value>)>(
        "SELECT elements, presentation FROM board_versions
         WHERE id = $1 AND board_id = $2
           AND id IN (SELECT id FROM board_versions WHERE board_id = $2 ORDER BY created_at DESC LIMIT $3)",
    )
    .bind(&version_id)
    .bind(&board_id)
    .bind(cap)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Version not found".into()))?;
    let elements = schema::elements(&version.0 .0, "Stored version is invalid")?;
    let presentation = version
        .1
        .map(|value| schema::presentation(&value))
        .transpose()?;
    let mut tx = state.db.begin().await?;
    let current = store::lock_revision(&mut tx, &board_id).await?;
    let already_applied = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM board_operations
         WHERE board_id = $1 AND actor_id = $2 AND client_operation_id = $3)",
    )
    .bind(&board_id)
    .bind(&user.id)
    .bind(&input.client_operation_id)
    .fetch_one(&mut *tx)
    .await?;
    if !already_applied {
        store::snapshot_board(
            &mut tx,
            &state.db,
            &board_id,
            &user.id,
            "restore",
            Some("Before restore"),
        )
        .await?;
    }
    let request = json!({
        "kind": "restore",
        "versionId": version_id,
        "elements": elements,
        "presentation": presentation,
    });
    let operation = store::replace_elements(
        &mut tx,
        &state.db,
        &board_id,
        current,
        &user.id,
        &input.client_operation_id,
        Some(input.base_revision),
        elements,
        &request,
    )
    .await?;
    sqlx::query("UPDATE boards SET presentation = $2 WHERE id = $1")
        .bind(&board_id)
        .bind(presentation)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    collaboration::broadcast_editor_operation(&state.collaboration, &board_id, &operation, true)
        .await;
    Ok(Json(json!({
        "revision": operation.revision,
        "elements": operation.batch.upserts,
    })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CommentInput {
    body: String,
    x: f64,
    y: f64,
    parent_id: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CommentReactionSummary {
    emoji: String,
    count: usize,
    reacted_by_me: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BoardComment {
    id: String,
    board_id: String,
    author_id: String,
    author_name: Option<String>,
    parent_id: Option<String>,
    body: String,
    x: f64,
    y: f64,
    resolved: bool,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    can_edit: bool,
    can_resolve: bool,
    can_delete: bool,
    reactions: Vec<CommentReactionSummary>,
}

type CommentRow = (
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    String,
    f64,
    f64,
    bool,
    DateTime<Utc>,
    DateTime<Utc>,
);

async fn list_comments(
    State(state): State<AppState>,
    cookies: Cookies,
    Path(board_id): Path<String>,
) -> AppResult<Json<Vec<BoardComment>>> {
    let user = session::require_current_user(&cookies, &state).await?;
    let access =
        authz::require_board_role(&state.db, &board_id, &user.id, BoardRole::Viewer).await?;
    let rows = sqlx::query_as::<_, CommentRow>(
        "SELECT c.id, c.board_id, c.author_id, u.name, c.parent_id, c.body,
                c.x, c.y, c.resolved, c.created_at, c.updated_at
         FROM board_comments c JOIN users u ON u.id = c.author_id
         WHERE c.board_id = $1 ORDER BY c.created_at ASC",
    )
    .bind(&board_id)
    .fetch_all(&state.db)
    .await?;
    let reaction_rows = sqlx::query_as::<_, (String, String, String)>(
        "SELECT r.comment_id, r.emoji, r.user_id FROM comment_reactions r
         JOIN board_comments c ON c.id = r.comment_id WHERE c.board_id = $1",
    )
    .bind(&board_id)
    .fetch_all(&state.db)
    .await?;
    let mut reaction_map: HashMap<String, HashMap<String, Vec<String>>> = HashMap::new();
    for (comment_id, emoji, user_id) in reaction_rows {
        reaction_map
            .entry(comment_id)
            .or_default()
            .entry(emoji)
            .or_default()
            .push(user_id);
    }
    Ok(Json(
        rows.into_iter()
            .map(|row| {
                let reactions = reaction_map.remove(&row.0).unwrap_or_default();
                comment_from_row(row, access.role, &user.id, reactions)
            })
            .collect(),
    ))
}

async fn create_comment(
    State(state): State<AppState>,
    cookies: Cookies,
    Path(board_id): Path<String>,
    AppJson(input): AppJson<CommentInput>,
) -> AppResult<Json<BoardComment>> {
    let user = session::require_current_user(&cookies, &state).await?;
    let access =
        authz::require_board_role(&state.db, &board_id, &user.id, BoardRole::Viewer).await?;
    let body = validate_comment_body(input.body)?;
    if !input.x.is_finite() || !input.y.is_finite() {
        return Err(AppError::BadRequest("Invalid comment position.".into()));
    }
    let mut tx = state.db.begin().await?;
    let mut recipient_candidates = vec![access.owner_id.clone()];
    if let Some(parent_id) = &input.parent_id {
        let parent = sqlx::query_as::<_, (Option<String>, String)>(
            "SELECT parent_id, board_id FROM board_comments WHERE id = $1",
        )
        .bind(parent_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| AppError::NotFound("Comment thread not found".into()))?;
        if parent.1 != board_id || parent.0.is_some() {
            return Err(AppError::NotFound("Comment thread not found".into()));
        }
        recipient_candidates = sqlx::query_scalar(
            "SELECT DISTINCT author_id FROM board_comments
             WHERE id=$1 OR parent_id=$1 ORDER BY author_id",
        )
        .bind(parent_id)
        .fetch_all(&mut *tx)
        .await?;
    }
    let board_name = sqlx::query_scalar::<_, String>("SELECT name FROM boards WHERE id=$1")
        .bind(&board_id)
        .fetch_one(&mut *tx)
        .await?;
    let id = cuid2::create_id();
    let row = sqlx::query_as::<_, CommentRow>(
        "WITH inserted AS (
            INSERT INTO board_comments (id, board_id, author_id, parent_id, body, x, y)
            VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING *
         )
         SELECT c.id, c.board_id, c.author_id, u.name, c.parent_id, c.body,
                c.x, c.y, c.resolved, c.created_at, c.updated_at
         FROM inserted c JOIN users u ON u.id = c.author_id",
    )
    .bind(&id)
    .bind(&board_id)
    .bind(&user.id)
    .bind(&input.parent_id)
    .bind(body)
    .bind(input.x)
    .bind(input.y)
    .fetch_one(&mut *tx)
    .await?;
    let recipients =
        notifications::eligible_board_recipients(&mut tx, &board_id, &recipient_candidates).await?;
    notifications::write(
        &mut tx,
        NewNotification {
            actor_id: user.id.clone(),
            actor_name: row.3.clone(),
            recipient_ids: recipients,
            notification_type: if input.parent_id.is_some() {
                NotificationType::CommentReply
            } else {
                NotificationType::BoardComment
            },
            event_key: format!("comment:{}", row.0),
            board_id: Some(board_id),
            workspace_id: None,
            comment_id: Some(row.0.clone()),
            resource_name: board_name,
            role: None,
        },
    )
    .await?;
    tx.commit().await?;
    Ok(Json(comment_from_row(
        row,
        access.role,
        &user.id,
        HashMap::new(),
    )))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct UpdateCommentInput {
    body: Option<String>,
    resolved: Option<bool>,
}

async fn update_comment(
    State(state): State<AppState>,
    cookies: Cookies,
    Path((board_id, comment_id)): Path<(String, String)>,
    AppJson(input): AppJson<UpdateCommentInput>,
) -> AppResult<Json<BoardComment>> {
    let user = session::require_current_user(&cookies, &state).await?;
    let access =
        authz::require_board_role(&state.db, &board_id, &user.id, BoardRole::Viewer).await?;
    if input.body.is_none() && input.resolved.is_none() {
        return Err(AppError::BadRequest("No comment changes supplied.".into()));
    }
    let existing = sqlx::query_as::<_, (String, bool)>(
        "SELECT author_id, resolved FROM board_comments WHERE id = $1 AND board_id = $2",
    )
    .bind(&comment_id)
    .bind(&board_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Comment not found".into()))?;
    if input.body.is_some() && existing.0 != user.id {
        return Err(AppError::Forbidden(
            "You can only edit your own comments.".into(),
        ));
    }
    if input.resolved.is_some() && existing.0 != user.id && access.role < BoardRole::Editor {
        return Err(AppError::Forbidden(
            "You do not have permission to resolve that.".into(),
        ));
    }
    let body = input.body.map(validate_comment_body).transpose()?;
    let row = sqlx::query_as::<_, CommentRow>(
        "UPDATE board_comments SET
            body = COALESCE($3, body),
            resolved = COALESCE($4, resolved),
            updated_at = now()
         WHERE id = $1 AND board_id = $2
         RETURNING id, board_id, author_id,
            (SELECT name FROM users WHERE id = author_id), parent_id, body,
            x, y, resolved, created_at, updated_at",
    )
    .bind(&comment_id)
    .bind(&board_id)
    .bind(body)
    .bind(input.resolved)
    .fetch_one(&state.db)
    .await?;
    Ok(Json(comment_from_row(
        row,
        access.role,
        &user.id,
        HashMap::new(),
    )))
}

async fn delete_comment(
    State(state): State<AppState>,
    cookies: Cookies,
    Path((board_id, comment_id)): Path<(String, String)>,
) -> AppResult<StatusCode> {
    let user = session::require_current_user(&cookies, &state).await?;
    let access =
        authz::require_board_role(&state.db, &board_id, &user.id, BoardRole::Viewer).await?;
    let author_id = sqlx::query_scalar::<_, String>(
        "SELECT author_id FROM board_comments WHERE id = $1 AND board_id = $2",
    )
    .bind(&comment_id)
    .bind(&board_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Comment not found".into()))?;
    if author_id != user.id && access.role != BoardRole::Owner {
        return Err(AppError::Forbidden(
            "You do not have permission to delete that.".into(),
        ));
    }
    sqlx::query("DELETE FROM board_comments WHERE id = $1")
        .bind(comment_id)
        .execute(&state.db)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReactionInput {
    emoji: String,
}

async fn toggle_reaction(
    State(state): State<AppState>,
    cookies: Cookies,
    Path((board_id, comment_id)): Path<(String, String)>,
    AppJson(input): AppJson<ReactionInput>,
) -> AppResult<Json<Vec<CommentReactionSummary>>> {
    let user = session::require_current_user(&cookies, &state).await?;
    authz::require_board_role(&state.db, &board_id, &user.id, BoardRole::Viewer).await?;
    if !COMMENT_EMOJIS.contains(&input.emoji.as_str()) {
        return Err(AppError::BadRequest("Invalid reaction.".into()));
    }
    let mut tx = state.db.begin().await?;
    let comment = sqlx::query_as::<_, (String, String, Option<String>)>(
        "SELECT c.author_id, b.name, actor.name
         FROM board_comments c
         JOIN boards b ON b.id = c.board_id
         JOIN users actor ON actor.id = $3
         WHERE c.id = $1 AND c.board_id = $2
         FOR UPDATE OF c",
    )
    .bind(&comment_id)
    .bind(&board_id)
    .bind(&user.id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| AppError::NotFound("Comment not found".into()))?;
    let reaction_id = sqlx::query_scalar::<_, String>(
        "INSERT INTO comment_reactions (id, comment_id, user_id, emoji)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (comment_id, user_id, emoji) DO NOTHING
         RETURNING id",
    )
    .bind(cuid2::create_id())
    .bind(&comment_id)
    .bind(&user.id)
    .bind(&input.emoji)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(reaction_id) = reaction_id {
        let recipients =
            notifications::eligible_board_recipients(&mut tx, &board_id, &[comment.0.clone()])
                .await?;
        notifications::write(
            &mut tx,
            NewNotification {
                actor_id: user.id.clone(),
                actor_name: comment.2,
                recipient_ids: recipients,
                notification_type: NotificationType::CommentReaction,
                event_key: format!("comment-reaction:{reaction_id}"),
                board_id: Some(board_id.clone()),
                workspace_id: None,
                comment_id: Some(comment_id.clone()),
                resource_name: comment.1,
                role: None,
            },
        )
        .await?;
    } else {
        sqlx::query(
            "DELETE FROM comment_reactions WHERE comment_id=$1 AND user_id=$2 AND emoji=$3",
        )
        .bind(&comment_id)
        .bind(&user.id)
        .bind(&input.emoji)
        .execute(&mut *tx)
        .await?;
    }
    let reactions = sqlx::query_as::<_, (String, String)>(
        "SELECT emoji, user_id FROM comment_reactions WHERE comment_id=$1",
    )
    .bind(&comment_id)
    .fetch_all(&mut *tx)
    .await?;
    let mut summary: HashMap<String, (usize, bool)> = HashMap::new();
    for (emoji, user_id) in reactions {
        let entry = summary.entry(emoji).or_default();
        entry.0 += 1;
        entry.1 |= user_id == user.id;
    }
    let summary = Json(
        summary
            .into_iter()
            .map(|(emoji, (count, reacted_by_me))| CommentReactionSummary {
                emoji,
                count,
                reacted_by_me,
            })
            .collect(),
    );
    tx.commit().await?;
    Ok(summary)
}

fn comment_from_row(
    row: CommentRow,
    role: BoardRole,
    user_id: &str,
    reactions: HashMap<String, Vec<String>>,
) -> BoardComment {
    let author_id = row.2;
    let can_edit = author_id == user_id;
    BoardComment {
        id: row.0,
        board_id: row.1,
        author_id: author_id.clone(),
        author_name: row.3,
        parent_id: row.4,
        body: row.5,
        x: row.6,
        y: row.7,
        resolved: row.8,
        created_at: row.9,
        updated_at: row.10,
        can_edit,
        can_resolve: can_edit || role >= BoardRole::Editor,
        can_delete: can_edit || role == BoardRole::Owner,
        reactions: reactions
            .into_iter()
            .map(|(emoji, users)| CommentReactionSummary {
                emoji,
                count: users.len(),
                reacted_by_me: users.iter().any(|id| id == user_id),
            })
            .collect(),
    }
}

fn validate_comment_body(body: String) -> AppResult<String> {
    let body = body.trim().to_owned();
    if body.is_empty() || body.chars().count() > 2_000 {
        return Err(AppError::BadRequest(
            "Comment must contain 1 to 2000 characters.".into(),
        ));
    }
    Ok(body)
}

fn role_name(role: BoardRole) -> &'static str {
    match role {
        BoardRole::Owner => "OWNER",
        BoardRole::Editor => "EDITOR",
        BoardRole::Viewer => "VIEWER",
    }
}

#[cfg(test)]
#[path = "editor_tests.rs"]
mod tests;
