use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{header, HeaderValue, Request},
    middleware::{self, Next},
    response::Response,
    routing::{get, patch, post},
    Json, Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::{FromRow, Postgres, QueryBuilder, Transaction};
use tower_cookies::Cookies;

use crate::{
    auth::session,
    error::{AppError, AppResult},
    extract::AppJson,
    state::AppState,
};

const DEFAULT_PAGE_SIZE: i64 = 20;
const MAX_PAGE_SIZE: i64 = 50;

const ACTIVE_PLAN: &str = r#"
    EXISTS (
        SELECT 1
        FROM users notification_user
        WHERE notification_user.id = n.recipient_id
          AND (
            (notification_user.plan = 'pro' AND notification_user.plan_expires_at > now())
            OR EXISTS (
                SELECT 1 FROM subscriptions notification_subscription
                WHERE notification_subscription.user_id = n.recipient_id
                  AND notification_subscription.status = 'active'
                  AND notification_subscription.expires_at > now()
            )
          )
    )
"#;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/notifications", get(list_notifications))
        .route(
            "/api/notifications/unread-count",
            get(unread_notification_count),
        )
        .route(
            "/api/notifications/{notification_id}",
            patch(mark_notification_read),
        )
        .route(
            "/api/notifications/read-all",
            post(mark_all_notifications_read),
        )
        .layer(middleware::from_fn(private_notification_response))
}

async fn private_notification_response(request: Request<Body>, next: Next) -> Response {
    let mut response = next.run(request).await;
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    response
        .headers_mut()
        .insert(header::VARY, HeaderValue::from_static("Cookie"));
    response
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum NotificationType {
    BoardShared,
    BoardRoleChanged,
    WorkspaceJoined,
    BoardComment,
    CommentReply,
}

impl NotificationType {
    fn as_str(self) -> &'static str {
        match self {
            Self::BoardShared => "BOARD_SHARED",
            Self::BoardRoleChanged => "BOARD_ROLE_CHANGED",
            Self::WorkspaceJoined => "WORKSPACE_JOINED",
            Self::BoardComment => "BOARD_COMMENT",
            Self::CommentReply => "COMMENT_REPLY",
        }
    }

    fn parse(value: &str) -> AppResult<Self> {
        match value {
            "BOARD_SHARED" => Ok(Self::BoardShared),
            "BOARD_ROLE_CHANGED" => Ok(Self::BoardRoleChanged),
            "WORKSPACE_JOINED" => Ok(Self::WorkspaceJoined),
            "BOARD_COMMENT" => Ok(Self::BoardComment),
            "COMMENT_REPLY" => Ok(Self::CommentReply),
            _ => Err(AppError::Internal(anyhow::anyhow!(
                "unknown persisted notification type"
            ))),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NotificationQuery {
    filter: Option<String>,
    limit: Option<i64>,
    cursor: Option<String>,
}

impl NotificationQuery {
    fn validate(self) -> AppResult<ValidatedQuery> {
        let filter = self.filter.unwrap_or_else(|| "all".to_string());
        if filter != "all" && filter != "unread" {
            return Err(AppError::BadRequest(
                "Notification filter must be 'all' or 'unread'.".into(),
            ));
        }
        let limit = self.limit.unwrap_or(DEFAULT_PAGE_SIZE);
        if !(1..=MAX_PAGE_SIZE).contains(&limit) {
            return Err(AppError::BadRequest(
                "Notification limit must be between 1 and 50.".into(),
            ));
        }
        if self
            .cursor
            .as_ref()
            .is_some_and(|cursor| cursor.len() > 512)
        {
            return Err(AppError::BadRequest("Invalid notification cursor.".into()));
        }
        Ok(ValidatedQuery {
            unread_only: filter == "unread",
            limit,
            cursor: self.cursor.map(|value| decode_cursor(&value)).transpose()?,
        })
    }
}

struct ValidatedQuery {
    unread_only: bool,
    limit: i64,
    cursor: Option<NotificationCursor>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct NotificationCursor {
    id: String,
    created_at: DateTime<Utc>,
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

fn decode_cursor(value: &str) -> AppResult<NotificationCursor> {
    let invalid = || AppError::BadRequest("Invalid notification cursor.".into());
    if value.is_empty() || value.len() > 512 {
        return Err(invalid());
    }
    let bytes = URL_SAFE_NO_PAD.decode(value).map_err(|_| invalid())?;
    let cursor: NotificationCursor = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
    if !valid_id(&cursor.id) {
        return Err(invalid());
    }
    Ok(cursor)
}

fn encode_cursor(row: &NotificationRow) -> String {
    URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(&NotificationCursor {
            id: row.id.clone(),
            created_at: row.created_at,
        })
        .expect("notification cursor serializes"),
    )
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct NotificationItem {
    id: String,
    #[schema(value_type = String)]
    r#type: NotificationType,
    message: String,
    href: String,
    created_at: DateTime<Utc>,
    read_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct NotificationPage {
    items: Vec<NotificationItem>,
    next_cursor: Option<String>,
    unread_count: i64,
    read_all_before: Option<String>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UnreadCount {
    unread_count: i64,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct MarkReadResponse {
    id: String,
    read_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[derive(utoipa::ToSchema)]
pub struct MarkAllReadInput {
    read_all_before: String,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct MarkAllReadResponse {
    updated_count: u64,
    unread_count: i64,
}

#[derive(Debug, FromRow)]
struct NotificationRow {
    id: String,
    type_name: String,
    payload_version: i32,
    payload: Value,
    actor_id: Option<String>,
    board_id: Option<String>,
    workspace_id: Option<String>,
    comment_id: Option<String>,
    created_at: DateTime<Utc>,
    read_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct NotificationPayload {
    actor_name: String,
    resource_name: String,
    role: Option<String>,
}

impl NotificationRow {
    fn serialize(self) -> AppResult<NotificationItem> {
        let invalid =
            || AppError::Internal(anyhow::anyhow!("invalid persisted notification payload"));
        if self.payload_version != 1 {
            return Err(invalid());
        }
        let payload: NotificationPayload =
            serde_json::from_value(self.payload).map_err(|_| invalid())?;
        if payload.actor_name.trim().is_empty()
            || payload.actor_name.chars().count() > 200
            || payload.resource_name.trim().is_empty()
            || payload.resource_name.chars().count() > 300
        {
            return Err(invalid());
        }
        let notification_type = NotificationType::parse(&self.type_name)?;
        let role = payload.role.as_deref();
        if (notification_type == NotificationType::BoardRoleChanged
            && !matches!(role, Some("EDITOR" | "VIEWER")))
            || role.is_some_and(|role| !matches!(role, "EDITOR" | "VIEWER" | "ADMIN" | "MEMBER"))
        {
            return Err(invalid());
        }
        let actor = if self.actor_id.is_some() {
            payload.actor_name.as_str()
        } else {
            "A former member"
        };
        let resource = &payload.resource_name;
        let message = match notification_type {
            NotificationType::BoardShared => format!("{actor} shared {resource} with you."),
            NotificationType::BoardRoleChanged => format!(
                "{actor} changed your role on {resource} to {}.",
                role.expect("role validated").to_lowercase()
            ),
            NotificationType::WorkspaceJoined => format!("{actor} added you to {resource}."),
            NotificationType::BoardComment => format!("{actor} commented on {resource}."),
            NotificationType::CommentReply => {
                format!("{actor} replied to a thread on {resource}.")
            }
        };
        let href = if let Some(board_id) = self.board_id {
            let mut href = format!("/board/{}", encode_path_segment(&board_id));
            if let Some(comment_id) = self.comment_id {
                href.push_str("?comment=");
                href.push_str(&encode_path_segment(&comment_id));
            }
            href
        } else if let Some(workspace_id) = self.workspace_id {
            format!("/dashboard?space={}", encode_path_segment(&workspace_id))
        } else {
            return Err(invalid());
        };
        Ok(NotificationItem {
            id: self.id,
            r#type: notification_type,
            message,
            href,
            created_at: self.created_at,
            read_at: self.read_at,
        })
    }
}

fn encode_path_segment(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(byte as char);
        } else {
            use std::fmt::Write;
            write!(encoded, "%{byte:02X}").expect("writing to a string cannot fail");
        }
    }
    encoded
}

fn push_eligible_filter(query: &mut QueryBuilder<'_, Postgres>, user_id: &str) {
    query
        .push("n.recipient_id = ")
        .push_bind(user_id.to_string());
    query.push(
        r#"
        AND (
            (
                n.board_id IS NOT NULL
                AND EXISTS (
                    SELECT 1 FROM boards b
                    WHERE b.id = n.board_id
                      AND (
                        b.owner_id = n.recipient_id
                        OR EXISTS (
                            SELECT 1 FROM board_members bm
                            WHERE bm.board_id = b.id
                              AND bm.user_id = n.recipient_id
                        )
                        OR (
                            b.workspace_id IS NOT NULL
                            AND "#,
    );
    query.push(ACTIVE_PLAN);
    query.push(
        r#"
                            AND EXISTS (
                                SELECT 1 FROM workspace_members wm
                                WHERE wm.workspace_id = b.workspace_id
                                  AND wm.user_id = n.recipient_id
                            )
                        )
                      )
                )
            )
            OR (
                n.board_id IS NULL
                AND n.workspace_id IS NOT NULL
                AND "#,
    );
    query.push(ACTIVE_PLAN);
    query.push(
        r#"
                AND EXISTS (
                    SELECT 1 FROM workspace_members wm
                    WHERE wm.workspace_id = n.workspace_id
                      AND wm.user_id = n.recipient_id
                )
            )
        )
        "#,
    );
}

#[utoipa::path(
    get,
    path = "/api/notifications",
    tag = "notifications",
    params(
        ("filter" = Option<String>, Query, description = "all or unread; defaults to all"),
        ("limit" = Option<i64>, Query, description = "Page size from 1 to 50; defaults to 20"),
        ("cursor" = Option<String>, Query, description = "Opaque cursor returned by the previous page")
    ),
    responses(
        (status = 200, description = "Notification page", body = NotificationPage),
        (status = 400, description = "Invalid query"),
        (status = 401, description = "Authentication required")
    )
)]
pub async fn list_notifications(
    State(state): State<AppState>,
    cookies: Cookies,
    query: Result<Query<NotificationQuery>, axum::extract::rejection::QueryRejection>,
) -> AppResult<Json<NotificationPage>> {
    let user = session::require_current_user(&cookies, &state).await?;
    let Query(query) =
        query.map_err(|_| AppError::BadRequest("Invalid notification query.".into()))?;
    let input = query.validate()?;
    let mut tx = state.db.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await?;

    let mut rows_query = QueryBuilder::<Postgres>::new(
        "SELECT n.id, n.notification_type::text AS type_name, n.payload_version, n.payload, n.actor_id, \
         n.board_id, n.workspace_id, n.comment_id, n.created_at, n.read_at \
         FROM notifications n WHERE ",
    );
    push_eligible_filter(&mut rows_query, &user.id);
    if input.unread_only {
        rows_query.push(" AND n.read_at IS NULL");
    }
    if let Some(cursor) = &input.cursor {
        rows_query
            .push(" AND (n.created_at, n.id) < (")
            .push_bind(cursor.created_at)
            .push(", ")
            .push_bind(&cursor.id)
            .push(")");
    }
    rows_query
        .push(" ORDER BY n.created_at DESC, n.id DESC LIMIT ")
        .push_bind(input.limit + 1);
    let rows = rows_query
        .build_query_as::<NotificationRow>()
        .fetch_all(&mut *tx)
        .await?;
    let has_more = rows.len() as i64 > input.limit;
    let mut rows = rows;
    rows.truncate(input.limit as usize);
    let next_cursor = if has_more {
        rows.last().map(encode_cursor)
    } else {
        None
    };
    let items = rows
        .into_iter()
        .map(NotificationRow::serialize)
        .collect::<AppResult<Vec<_>>>()?;

    let mut count_query =
        QueryBuilder::<Postgres>::new("SELECT count(*) FROM notifications n WHERE ");
    push_eligible_filter(&mut count_query, &user.id);
    count_query.push(" AND n.read_at IS NULL");
    let unread_count = count_query
        .build_query_scalar::<i64>()
        .fetch_one(&mut *tx)
        .await?;

    let mut boundary_query =
        QueryBuilder::<Postgres>::new("SELECT n.id FROM notifications n WHERE ");
    push_eligible_filter(&mut boundary_query, &user.id);
    boundary_query.push(" ORDER BY n.sequence DESC LIMIT 1");
    let read_all_before = boundary_query
        .build_query_scalar::<String>()
        .fetch_optional(&mut *tx)
        .await?;

    tx.commit().await?;
    Ok(Json(NotificationPage {
        items,
        next_cursor,
        unread_count,
        read_all_before,
    }))
}

#[utoipa::path(
    get,
    path = "/api/notifications/unread-count",
    tag = "notifications",
    responses(
        (status = 200, description = "Unread notification count", body = UnreadCount),
        (status = 401, description = "Authentication required")
    )
)]
pub async fn unread_notification_count(
    State(state): State<AppState>,
    cookies: Cookies,
) -> AppResult<Json<UnreadCount>> {
    let user = session::require_current_user(&cookies, &state).await?;
    let mut query = QueryBuilder::<Postgres>::new("SELECT count(*) FROM notifications n WHERE ");
    push_eligible_filter(&mut query, &user.id);
    query.push(" AND n.read_at IS NULL");
    let unread_count = query
        .build_query_scalar::<i64>()
        .fetch_one(&state.db)
        .await?;
    Ok(Json(UnreadCount { unread_count }))
}

#[utoipa::path(
    patch,
    path = "/api/notifications/{notification_id}",
    tag = "notifications",
    params(("notification_id" = String, Path, description = "Notification ID")),
    responses(
        (status = 200, description = "Notification marked read", body = MarkReadResponse),
        (status = 400, description = "Invalid notification ID"),
        (status = 404, description = "Notification not found"),
        (status = 401, description = "Authentication required")
    )
)]
pub async fn mark_notification_read(
    State(state): State<AppState>,
    cookies: Cookies,
    Path(notification_id): Path<String>,
) -> AppResult<Json<MarkReadResponse>> {
    let user = session::require_current_user(&cookies, &state).await?;
    if !valid_id(&notification_id) {
        return Err(AppError::BadRequest("Invalid notification ID.".into()));
    }
    let mut tx = state.db.begin().await?;
    let mut find_query =
        QueryBuilder::<Postgres>::new("SELECT n.read_at FROM notifications n WHERE n.id = ");
    find_query.push_bind(&notification_id).push(" AND ");
    push_eligible_filter(&mut find_query, &user.id);
    find_query.push(" FOR UPDATE");
    let current_read_at = find_query
        .build_query_scalar::<Option<DateTime<Utc>>>()
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| AppError::NotFound("Notification not found".into()))?;

    let read_at = if let Some(read_at) = current_read_at {
        read_at
    } else {
        let mut update_query = QueryBuilder::<Postgres>::new(
            "UPDATE notifications n SET read_at = now() WHERE n.id = ",
        );
        update_query
            .push_bind(&notification_id)
            .push(" AND n.read_at IS NULL AND ");
        push_eligible_filter(&mut update_query, &user.id);
        update_query.push(" RETURNING n.read_at");
        update_query
            .build_query_scalar::<DateTime<Utc>>()
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| {
                AppError::Conflict("Notification access changed. Refresh and retry.".into())
            })?
    };
    tx.commit().await?;
    Ok(Json(MarkReadResponse {
        id: notification_id,
        read_at,
    }))
}

#[utoipa::path(
    post,
    path = "/api/notifications/read-all",
    tag = "notifications",
    request_body = MarkAllReadInput,
    responses(
        (status = 200, description = "Notifications marked read", body = MarkAllReadResponse),
        (status = 400, description = "Invalid boundary"),
        (status = 409, description = "Boundary expired"),
        (status = 401, description = "Authentication required")
    )
)]
pub async fn mark_all_notifications_read(
    State(state): State<AppState>,
    cookies: Cookies,
    AppJson(input): AppJson<MarkAllReadInput>,
) -> AppResult<Json<MarkAllReadResponse>> {
    let user = session::require_current_user(&cookies, &state).await?;
    if !valid_id(&input.read_all_before) {
        return Err(AppError::BadRequest(
            "Invalid notification boundary.".into(),
        ));
    }
    let mut tx = state.db.begin().await?;
    let locked_user =
        sqlx::query_scalar::<_, String>("SELECT id FROM users WHERE id = $1 FOR NO KEY UPDATE")
            .bind(&user.id)
            .fetch_optional(&mut *tx)
            .await?;
    if locked_user.is_none() {
        return Err(AppError::Unauthorized("Sign in to continue.".into()));
    }
    let boundary = sqlx::query_scalar::<_, i64>(
        "SELECT sequence FROM notifications WHERE id = $1 AND recipient_id = $2",
    )
    .bind(&input.read_all_before)
    .bind(&user.id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| {
        AppError::Conflict("Notification boundary expired. Refresh and retry.".into())
    })?;

    let mut update_query = QueryBuilder::<Postgres>::new(
        "UPDATE notifications n SET read_at = now() WHERE n.sequence <= ",
    );
    update_query
        .push_bind(boundary)
        .push(" AND n.read_at IS NULL AND ");
    push_eligible_filter(&mut update_query, &user.id);
    update_query.push(" RETURNING n.id");
    let updated_count = update_query.build().fetch_all(&mut *tx).await?.len() as u64;

    let mut count_query =
        QueryBuilder::<Postgres>::new("SELECT count(*) FROM notifications n WHERE ");
    push_eligible_filter(&mut count_query, &user.id);
    count_query.push(" AND n.read_at IS NULL");
    let unread_count = count_query
        .build_query_scalar::<i64>()
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(MarkAllReadResponse {
        updated_count,
        unread_count,
    }))
}

#[derive(Debug, Clone)]
pub(crate) struct NewNotification {
    pub actor_id: String,
    pub actor_name: Option<String>,
    pub recipient_ids: Vec<String>,
    pub notification_type: NotificationType,
    pub event_key: String,
    pub board_id: Option<String>,
    pub workspace_id: Option<String>,
    pub comment_id: Option<String>,
    pub resource_name: String,
    pub role: Option<String>,
}

pub(crate) async fn write(
    tx: &mut Transaction<'_, Postgres>,
    input: NewNotification,
) -> AppResult<()> {
    let mut recipients = input
        .recipient_ids
        .into_iter()
        .filter(|recipient_id| recipient_id != &input.actor_id)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    if recipients.is_empty() {
        return Ok(());
    }

    sqlx::query_scalar::<_, String>(
        "SELECT id FROM users WHERE id = ANY($1) ORDER BY id FOR NO KEY UPDATE",
    )
    .bind(&recipients)
    .fetch_all(&mut **tx)
    .await?;

    let actor_name = input
        .actor_name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or("A member");
    let mut payload = json!({
        "actorName": actor_name.chars().take(200).collect::<String>(),
        "resourceName": input.resource_name.chars().take(300).collect::<String>(),
    });
    if let Some(role) = &input.role {
        payload["role"] = json!(role);
    }
    for recipient_id in recipients.drain(..) {
        sqlx::query(
            r#"
            INSERT INTO notifications (
                id, recipient_id, actor_id, notification_type, event_key, board_id,
                workspace_id, comment_id, payload_version, payload
            )
            VALUES ($1, $2, $3, $4::notification_type, $5, $6, $7, $8, 1, $9)
            ON CONFLICT (recipient_id, event_key) DO NOTHING
            "#,
        )
        .bind(cuid2::create_id())
        .bind(recipient_id)
        .bind(&input.actor_id)
        .bind(input.notification_type.as_str())
        .bind(&input.event_key)
        .bind(&input.board_id)
        .bind(&input.workspace_id)
        .bind(&input.comment_id)
        .bind(&payload)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

pub(crate) async fn eligible_board_recipients(
    tx: &mut Transaction<'_, Postgres>,
    board_id: &str,
    candidates: &[String],
) -> AppResult<Vec<String>> {
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    Ok(sqlx::query_scalar(
        r#"
        SELECT DISTINCT candidate.id
        FROM users candidate
        WHERE candidate.id = ANY($2)
          AND EXISTS (
              SELECT 1
              FROM boards b
              WHERE b.id = $1
                AND (
                    b.owner_id = candidate.id
                    OR EXISTS (
                        SELECT 1 FROM board_members bm
                        WHERE bm.board_id = b.id AND bm.user_id = candidate.id
                    )
                    OR (
                        b.workspace_id IS NOT NULL
                        AND (
                            (candidate.plan = 'pro' AND candidate.plan_expires_at > now())
                            OR EXISTS (
                                SELECT 1 FROM subscriptions s
                                WHERE s.user_id = candidate.id
                                  AND s.status = 'active'
                                  AND s.expires_at > now()
                            )
                        )
                        AND EXISTS (
                            SELECT 1 FROM workspace_members wm
                            WHERE wm.workspace_id = b.workspace_id
                              AND wm.user_id = candidate.id
                        )
                    )
                )
          )
        ORDER BY candidate.id
        "#,
    )
    .bind(board_id)
    .bind(candidates)
    .fetch_all(&mut **tx)
    .await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{to_bytes, Body},
        http::{Request, StatusCode},
    };
    use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
    use std::{str::FromStr, sync::Arc};
    use tower::ServiceExt;
    use tower_cookies::CookieManagerLayer;

    fn row(type_name: &str, payload: Value) -> NotificationRow {
        NotificationRow {
            id: "notification-1".into(),
            type_name: type_name.into(),
            payload_version: 1,
            payload,
            actor_id: Some("actor-1".into()),
            board_id: Some("board 1".into()),
            workspace_id: None,
            comment_id: Some("comment?1".into()),
            created_at: DateTime::parse_from_rfc3339("2026-10-28T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            read_at: None,
        }
    }

    #[test]
    fn validates_notification_query_defaults_and_limits() {
        let query = NotificationQuery {
            filter: None,
            limit: None,
            cursor: None,
        }
        .validate()
        .unwrap();
        assert_eq!(query.limit, DEFAULT_PAGE_SIZE);
        assert!(!query.unread_only);
        assert!(query.cursor.is_none());

        assert!(
            NotificationQuery {
                filter: Some("unread".into()),
                limit: Some(50),
                cursor: None,
            }
            .validate()
            .unwrap()
            .unread_only
        );
        for limit in [0, 51] {
            assert!(NotificationQuery {
                filter: None,
                limit: Some(limit),
                cursor: None,
            }
            .validate()
            .is_err());
        }
        assert!(NotificationQuery {
            filter: Some("recent".into()),
            limit: None,
            cursor: None,
        }
        .validate()
        .is_err());
    }

    #[test]
    fn validates_and_encodes_notification_cursors() {
        let cursor = NotificationCursor {
            id: "notification-1".into(),
            created_at: DateTime::parse_from_rfc3339("2026-10-28T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        };
        let encoded = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&cursor).unwrap());
        assert_eq!(decode_cursor(&encoded).unwrap().id, cursor.id);
        for value in ["", "invalid", &"x".repeat(513)] {
            assert!(decode_cursor(value).is_err());
        }
        let malformed =
            URL_SAFE_NO_PAD.encode(br#"{"id":"bad id","createdAt":"2026-10-28T00:00:00Z"}"#);
        assert!(decode_cursor(&malformed).is_err());
    }

    #[test]
    fn serializes_messages_and_safe_deep_links() {
        let item = row(
            "BOARD_COMMENT",
            json!({"actorName":"Ada","resourceName":"Ideas"}),
        )
        .serialize()
        .unwrap();
        assert_eq!(item.message, "Ada commented on Ideas.");
        assert_eq!(item.href, "/board/board%201?comment=comment%3F1");
        assert_eq!(item.r#type, NotificationType::BoardComment);
    }

    #[test]
    fn malformed_persisted_payload_fails_explicitly() {
        assert!(row(
            "BOARD_ROLE_CHANGED",
            json!({
                "actorName": "Ada",
                "resourceName": "Ideas"
            })
        )
        .serialize()
        .is_err());
    }

    async fn request(
        app: &Router,
        token: Option<&str>,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut builder = Request::builder().method(method).uri(path);
        if let Some(token) = token {
            builder = builder.header("cookie", format!("sketchflow_session={token}"));
        }
        let request_body = match body {
            Some(body) => {
                builder = builder.header("content-type", "application/json");
                Body::from(body.to_string())
            }
            None => Body::empty(),
        };
        let response = app
            .clone()
            .oneshot(builder.body(request_body).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let body = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| panic!("notification API returned non-JSON for {path}"));
        (status, body)
    }

    #[tokio::test]
    #[ignore = "requires explicit loopback NOTIFICATION_TEST_DATABASE_URL ending in _test"]
    async fn notification_http_flows_authorization_pagination_and_read_boundaries() {
        let url = std::env::var("NOTIFICATION_TEST_DATABASE_URL")
            .expect("set NOTIFICATION_TEST_DATABASE_URL");
        let options = PgConnectOptions::from_str(&url).unwrap();
        assert!(matches!(
            options.get_host(),
            "127.0.0.1" | "localhost" | "::1"
        ));
        assert!(options.get_database().unwrap().ends_with("_test"));
        let admin = PgPoolOptions::new()
            .max_connections(2)
            .connect_with(options.clone())
            .await
            .unwrap();
        let schema = format!("notification_test_{}", uuid::Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE SCHEMA {schema}"))
            .execute(&admin)
            .await
            .unwrap();
        let db = PgPoolOptions::new()
            .max_connections(10)
            .connect_with(options.options([("search_path", schema.as_str())]))
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&db).await.unwrap();
        for (id, name) in [
            ("owner", "Board owner"),
            ("member", "Board member"),
            ("invitee", "Board invitee"),
        ] {
            sqlx::query(
                "INSERT INTO users(id,name,email,plan,plan_expires_at)
                 VALUES($1,$2,$3,$4,CASE WHEN $4='pro' THEN now()+interval '30 days' END)",
            )
            .bind(id)
            .bind(name)
            .bind(format!("{id}@test.invalid"))
            .bind(if id == "owner" || id == "invitee" {
                "pro"
            } else {
                "free"
            })
            .execute(&db)
            .await
            .unwrap();
        }
        sqlx::query("INSERT INTO boards(id,name,owner_id) VALUES('board-1','Planning','owner')")
            .execute(&db)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO workspaces(id,name,slug,created_by_id)
             VALUES('workspace-1','Team','team','owner')",
        )
        .execute(&db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO workspace_members(id,workspace_id,user_id,role)
             VALUES('workspace-owner','workspace-1','owner','OWNER')",
        )
        .execute(&db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO board_members(id,board_id,user_id,role) VALUES('member-row','board-1','member','VIEWER')",
        )
        .execute(&db)
        .await
        .unwrap();

        let mut tx = db.begin().await.unwrap();
        for event in 1..=3 {
            write(
                &mut tx,
                NewNotification {
                    actor_id: "owner".into(),
                    actor_name: Some("Board owner".into()),
                    recipient_ids: vec!["member".into()],
                    notification_type: NotificationType::BoardShared,
                    event_key: format!("board-shared:{event}"),
                    board_id: Some("board-1".into()),
                    workspace_id: None,
                    comment_id: None,
                    resource_name: "Planning".into(),
                    role: Some("VIEWER".into()),
                },
            )
            .await
            .unwrap();
        }
        tx.commit().await.unwrap();

        let config = crate::config::AppConfig::from_env();
        let secret = config.jwt_secret.clone();
        let state = AppState {
            db: db.clone(),
            config: Arc::new(config),
            collaboration: crate::routes::collaboration::CollaborationHub::default(),
        };
        let app = crate::routes::dashboard::router()
            .merge(crate::routes::editor::router())
            .merge(router())
            .layer(CookieManagerLayer::new())
            .with_state(state);
        let token = crate::auth::jwt::issue_token("member", &secret, 1).unwrap();
        let owner_token = crate::auth::jwt::issue_token("owner", &secret, 1).unwrap();
        let invitee_token = crate::auth::jwt::issue_token("invitee", &secret, 1).unwrap();

        let (status, _) = request(&app, None, "GET", "/api/notifications", None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, _) = request(
            &app,
            Some(&owner_token),
            "POST",
            "/api/boards/board-1/members",
            Some(json!({"email":"invitee@test.invalid","role":"EDITOR"})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let (status, _) = request(
            &app,
            Some(&owner_token),
            "POST",
            "/api/workspaces/workspace-1/members",
            Some(json!({"email":"invitee@test.invalid","role":"MEMBER"})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let (status, invitee_page) = request(
            &app,
            Some(&invitee_token),
            "GET",
            "/api/notifications",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let invitee_types = invitee_page["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["type"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert!(invitee_types.contains(&"BOARD_SHARED"));
        assert!(invitee_types.contains(&"WORKSPACE_JOINED"));

        let (status, _) = request(
            &app,
            Some(&owner_token),
            "PATCH",
            "/api/boards/board-1/members/member-row",
            Some(json!({"role":"EDITOR"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (status, root_comment) = request(
            &app,
            Some(&token),
            "POST",
            "/api/boards/board-1/comments",
            Some(json!({"body":"First comment","x":10,"y":20})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, owner_page) =
            request(&app, Some(&owner_token), "GET", "/api/notifications", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(owner_page["items"][0]["type"], "BOARD_COMMENT");

        let (status, _) = request(
            &app,
            Some(&invitee_token),
            "POST",
            "/api/boards/board-1/comments",
            Some(json!({
                "body":"Reply",
                "x":10,
                "y":20,
                "parentId":root_comment["id"]
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, member_page) =
            request(&app, Some(&token), "GET", "/api/notifications", None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(member_page["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["type"] == "COMMENT_REPLY"));

        let (status, first_page) = request(
            &app,
            Some(&token),
            "GET",
            "/api/notifications?limit=2",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(first_page["items"].as_array().unwrap().len(), 2);
        assert_eq!(first_page["unreadCount"], 5);
        let cursor = first_page["nextCursor"].as_str().unwrap();
        let (status, second_page) = request(
            &app,
            Some(&token),
            "GET",
            &format!("/api/notifications?limit=2&cursor={cursor}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(second_page["items"].as_array().unwrap().len(), 2);
        assert!(second_page["nextCursor"].is_null());
        assert_ne!(first_page["items"][0]["id"], second_page["items"][0]["id"]);

        let boundary = first_page["readAllBefore"].as_str().unwrap();
        let mut tx = db.begin().await.unwrap();
        write(
            &mut tx,
            NewNotification {
                actor_id: "owner".into(),
                actor_name: Some("Board owner".into()),
                recipient_ids: vec!["member".into()],
                notification_type: NotificationType::BoardRoleChanged,
                event_key: "board-role:later".into(),
                board_id: Some("board-1".into()),
                workspace_id: None,
                comment_id: None,
                resource_name: "Planning".into(),
                role: Some("VIEWER".into()),
            },
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();

        let (status, marked) = request(
            &app,
            Some(&token),
            "POST",
            "/api/notifications/read-all",
            Some(json!({"readAllBefore": boundary})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(marked["updatedCount"], 5);
        assert_eq!(marked["unreadCount"], 1);
        let (status, unread) = request(
            &app,
            Some(&token),
            "GET",
            "/api/notifications/unread-count",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(unread["unreadCount"], 1);

        let (status, unread_page) = request(
            &app,
            Some(&token),
            "GET",
            "/api/notifications?filter=unread",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let notification_id = unread_page["items"][0]["id"].as_str().unwrap();
        let (status, _) = request(
            &app,
            Some(&owner_token),
            "PATCH",
            &format!("/api/notifications/{notification_id}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, read) = request(
            &app,
            Some(&token),
            "PATCH",
            &format!("/api/notifications/{notification_id}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(read["readAt"].is_string());

        sqlx::query("DELETE FROM board_members WHERE id='member-row'")
            .execute(&db)
            .await
            .unwrap();
        let (status, hidden) = request(&app, Some(&token), "GET", "/api/notifications", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(hidden["items"].as_array().unwrap().len(), 0);

        db.close().await;
        sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
            .execute(&admin)
            .await
            .unwrap();
    }
}
