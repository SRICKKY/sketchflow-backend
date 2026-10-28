use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::{FromRow, PgPool, Postgres, Transaction};
use tower_cookies::Cookies;
use validator::ValidateEmail;

use crate::{
    auth::session,
    authz::{self, BoardRole},
    billing::limits::{FREE_MAX_BOARD_MEMBERS, FREE_MAX_OWNED_BOARDS},
    billing::{self, BillingStatus},
    error::{AppError, AppResult},
    extract::AppJson,
    routes::notifications::{self, NewNotification, NotificationType},
    state::AppState,
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/dashboard", get(dashboard))
        .route("/api/boards", get(list_boards).post(create_board))
        .route(
            "/api/boards/{id}",
            get(get_board).patch(rename_board).delete(delete_board),
        )
        .route("/api/boards/{id}/duplicate", post(duplicate_board))
        .route("/api/boards/{id}/template", post(save_template))
        .route(
            "/api/boards/{id}/members",
            get(board_members).post(add_board_member),
        )
        .route(
            "/api/boards/{id}/members/{member_id}",
            axum::routing::patch(update_board_member).delete(remove_board_member),
        )
        .route("/api/templates", get(list_templates).post(import_template))
        .route(
            "/api/templates/{id}",
            axum::routing::delete(delete_template),
        )
        .route(
            "/api/workspaces",
            get(list_workspaces).post(create_workspace),
        )
        .route(
            "/api/workspaces/{id}",
            get(get_workspace)
                .patch(rename_workspace)
                .delete(delete_workspace),
        )
        .route(
            "/api/workspaces/{id}/members",
            get(workspace_members).post(add_workspace_member),
        )
        .route(
            "/api/workspaces/{id}/members/{member_id}",
            axum::routing::patch(update_workspace_member).delete(remove_workspace_member),
        )
        .route("/api/workspaces/{id}/leave", post(leave_workspace))
}

#[derive(Debug, Serialize, FromRow)]
#[serde(rename_all = "camelCase")]
pub struct BoardSummary {
    id: String,
    name: String,
    owner_id: String,
    owner_name: Option<String>,
    workspace_id: Option<String>,
    role: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    element_count: i64,
}

#[derive(Debug, Serialize, FromRow)]
pub struct WorkspaceSummary {
    id: String,
    name: String,
    slug: String,
    role: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Capabilities {
    is_pro: bool,
    owned_boards: i64,
    max_owned_boards: Option<i64>,
    can_create_board: bool,
    can_save_templates: bool,
}

#[derive(Serialize)]
struct DashboardResponse {
    capabilities: Capabilities,
    workspaces: Vec<WorkspaceSummary>,
}

#[derive(Serialize)]
struct WorkspaceDetail {
    #[serde(flatten)]
    summary: WorkspaceSummary,
    members: Vec<Member>,
}

#[derive(Debug, Serialize, FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Member {
    id: String,
    user_id: String,
    name: Option<String>,
    email: String,
    image: Option<String>,
    role: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Template {
    id: String,
    name: String,
    description: Option<String>,
    category: String,
    #[serde(default)]
    builtin: bool,
    #[serde(default, skip_serializing)]
    elements: Vec<Value>,
    #[serde(default)]
    element_count: usize,
}

#[derive(Serialize)]
struct Items<T> {
    items: Vec<T>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BoardPage {
    items: Vec<BoardSummary>,
    next_cursor: Option<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BoardCursor {
    updated_at: DateTime<Utc>,
    id: String,
}

impl BoardCursor {
    fn decode(value: &str) -> AppResult<Self> {
        let invalid = || AppError::BadRequest("Invalid board cursor.".into());
        if value.is_empty() || value.len() > 1024 {
            return Err(invalid());
        }
        let bytes = URL_SAFE_NO_PAD.decode(value).map_err(|_| invalid())?;
        let cursor: Self = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
        if cursor.id.is_empty()
            || cursor.id.len() > 128
            || cursor.id.chars().any(char::is_control)
            || cursor.updated_at.timestamp() < 0
            || cursor.updated_at.timestamp() > 253402300799
        {
            return Err(invalid());
        }
        Ok(cursor)
    }

    fn encode(board: &BoardSummary) -> String {
        URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&Self {
                updated_at: board.updated_at,
                id: board.id.clone(),
            })
            .expect("board cursor serializes"),
        )
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BoardInput {
    name: Option<String>,
    workspace_id: Option<String>,
    template_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NameInput {
    name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BoardQuery {
    scope: Option<String>,
    workspace_id: Option<String>,
    q: Option<String>,
    limit: Option<u32>,
    cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MemberInput {
    email: String,
    role: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RoleInput {
    role: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TemplateInput {
    name: String,
    description: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ImportTemplateInput {
    name: String,
    description: Option<String>,
    elements: Vec<Value>,
}

fn workspace_name(value: &str) -> AppResult<String> {
    let value = name(value, 40)?;
    if value.chars().count() < 2 {
        return Err(AppError::BadRequest(
            "Team name must contain 2 to 40 characters.".into(),
        ));
    }
    Ok(value)
}

fn template_description(value: Option<String>) -> AppResult<Option<String>> {
    let value = value.map(|d| d.trim().to_owned()).filter(|d| !d.is_empty());
    if value.as_ref().is_some_and(|d| d.chars().count() > 200) {
        return Err(AppError::BadRequest(
            "Description must contain at most 200 characters.".into(),
        ));
    }
    Ok(value)
}

fn validate_template_elements(elements: &[Value]) -> AppResult<()> {
    if elements.len() > 5_000 {
        return Err(AppError::BadRequest(
            "Templates support at most 5000 elements.".into(),
        ));
    }
    let mut ids = std::collections::HashSet::new();
    for chunk in elements.chunks(500) {
        super::collaboration::validate_operation(
            "template",
            super::collaboration::ElementBatch {
                upserts: chunk.to_vec(),
                deleted_ids: vec![],
            },
        )?;
        for element in chunk {
            if !ids.insert(element["id"].as_str().unwrap()) {
                return Err(AppError::BadRequest(
                    "Template contains duplicate element IDs.".into(),
                ));
            }
        }
    }
    Ok(())
}

fn id() -> String {
    cuid2::create_id()
}

fn name(value: &str, max: usize) -> AppResult<String> {
    let value = value.trim();
    if value.is_empty() || value.chars().count() > max {
        return Err(AppError::BadRequest(format!(
            "Name must contain 1 to {max} characters."
        )));
    }
    Ok(value.to_owned())
}

fn member_role(value: &str, workspace: bool) -> AppResult<&str> {
    if (workspace && matches!(value, "ADMIN" | "MEMBER"))
        || (!workspace && matches!(value, "EDITOR" | "VIEWER"))
    {
        Ok(value)
    } else {
        Err(AppError::BadRequest(
            "Invalid member role. OWNER cannot be assigned.".into(),
        ))
    }
}

async fn is_pro(db: &PgPool, user_id: &str) -> AppResult<bool> {
    Ok(billing::get_billing_state(db, user_id).await?.status == BillingStatus::Active)
}

async fn require_pro(db: &PgPool, user_id: &str) -> AppResult<()> {
    if !is_pro(db, user_id).await? {
        return Err(AppError::Forbidden(
            "Workspaces require an active Pro subscription.".into(),
        ));
    }
    Ok(())
}

async fn workspace_role(
    db: &PgPool,
    workspace_id: &str,
    user_id: &str,
    min: &str,
) -> AppResult<String> {
    require_pro(db, user_id).await?;
    let role = sqlx::query_scalar::<_, String>(
        "SELECT role::text FROM workspace_members WHERE workspace_id=$1 AND user_id=$2",
    )
    .bind(workspace_id)
    .bind(user_id)
    .fetch_optional(db)
    .await?
    .ok_or_else(|| AppError::NotFound("Workspace not found".into()))?;
    let rank = |r: &str| match r {
        "OWNER" => 3,
        "ADMIN" => 2,
        "MEMBER" => 1,
        _ => 0,
    };
    if rank(&role) < rank(min) {
        return Err(AppError::Forbidden(
            "You do not have permission to do that.".into(),
        ));
    }
    Ok(role)
}

async fn lock_resource(
    tx: &mut Transaction<'_, Postgres>,
    resource: &str,
    resource_id: &str,
) -> AppResult<()> {
    let sql = match resource {
        "boards" => "SELECT id FROM boards WHERE id=$1 FOR UPDATE",
        "workspaces" => "SELECT id FROM workspaces WHERE id=$1 FOR UPDATE",
        _ => unreachable!(),
    };
    sqlx::query_scalar::<_, String>(sql)
        .bind(resource_id)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or_else(|| AppError::NotFound("Resource not found".into()))?;
    Ok(())
}

async fn workspace_list(db: &PgPool, user_id: &str) -> AppResult<Vec<WorkspaceSummary>> {
    if !is_pro(db, user_id).await? {
        return Ok(vec![]);
    }
    Ok(sqlx::query_as(
        "SELECT w.id,w.name,w.slug,m.role::text AS role FROM workspaces w JOIN workspace_members m ON m.workspace_id=w.id WHERE m.user_id=$1 ORDER BY m.created_at,w.id"
    ).bind(user_id).fetch_all(db).await?)
}

async fn dashboard(
    State(state): State<AppState>,
    cookies: Cookies,
) -> AppResult<Json<DashboardResponse>> {
    let user = session::require_current_user(&cookies, &state).await?;
    let pro = is_pro(&state.db, &user.id).await?;
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM boards WHERE owner_id=$1 AND workspace_id IS NULL",
    )
    .bind(&user.id)
    .fetch_one(&state.db)
    .await?;
    Ok(Json(DashboardResponse {
        capabilities: Capabilities {
            is_pro: pro,
            owned_boards: count,
            max_owned_boards: if pro {
                None
            } else {
                Some(FREE_MAX_OWNED_BOARDS)
            },
            can_create_board: pro || count < FREE_MAX_OWNED_BOARDS,
            can_save_templates: pro,
        },
        workspaces: workspace_list(&state.db, &user.id).await?,
    }))
}

const BOARD_SELECT: &str = "SELECT b.id,b.name,b.owner_id,owner.name AS owner_name,b.workspace_id,b.created_at,b.updated_at,
    CASE WHEN b.owner_id=$1 THEN 'OWNER' ELSE COALESCE(m.role::text,CASE WHEN wm.role::text IN ('OWNER','ADMIN') THEN 'EDITOR' ELSE 'VIEWER' END) END AS role,
    (SELECT count(*) FROM board_elements e WHERE e.board_id=b.id) AS element_count
    FROM boards b JOIN users owner ON owner.id=b.owner_id
    LEFT JOIN board_members m ON m.board_id=b.id AND m.user_id=$1
    LEFT JOIN workspace_members wm ON wm.workspace_id=b.workspace_id AND wm.user_id=$1";

async fn board_summary(db: &PgPool, board_id: &str, user_id: &str) -> AppResult<BoardSummary> {
    authz::require_board_role(db, board_id, user_id, BoardRole::Viewer).await?;
    Ok(sqlx::query_as(&format!("{BOARD_SELECT} WHERE b.id=$2"))
        .bind(user_id)
        .bind(board_id)
        .fetch_one(db)
        .await?)
}

async fn list_boards(
    State(state): State<AppState>,
    cookies: Cookies,
    query: Result<Query<BoardQuery>, axum::extract::rejection::QueryRejection>,
) -> AppResult<Json<BoardPage>> {
    let user = session::require_current_user(&cookies, &state).await?;
    let Query(query) =
        query.map_err(|_| AppError::BadRequest("Invalid query parameters.".into()))?;
    let limit = query.limit.unwrap_or(12);
    if !(1..=100).contains(&limit) {
        return Err(AppError::BadRequest(
            "limit must be between 1 and 100.".into(),
        ));
    }
    let cursor = query
        .cursor
        .as_deref()
        .map(BoardCursor::decode)
        .transpose()?;
    let scope = query.scope.as_deref().unwrap_or("owned");
    if !matches!(scope, "owned" | "shared") {
        return Err(AppError::BadRequest(
            "scope must be owned or shared.".into(),
        ));
    }
    if query.q.as_ref().is_some_and(|q| q.chars().count() > 200) {
        return Err(AppError::BadRequest(
            "Search must contain at most 200 characters.".into(),
        ));
    }
    let workspace = query.workspace_id.as_deref().filter(|s| !s.is_empty());
    if let Some(workspace) = workspace {
        workspace_role(&state.db, workspace, &user.id, "MEMBER").await?;
    }
    let condition = if workspace.is_some() {
        "b.workspace_id=$2"
    } else if scope == "shared" {
        "b.workspace_id IS NULL AND b.owner_id<>$1 AND m.id IS NOT NULL AND $2::text IS NULL"
    } else {
        "b.workspace_id IS NULL AND b.owner_id=$1 AND $2::text IS NULL"
    };
    let mut items: Vec<BoardSummary> = sqlx::query_as(&format!(
        "{BOARD_SELECT} WHERE {condition} AND strpos(lower(b.name),lower($3))>0
        AND ($4::timestamptz IS NULL OR (b.updated_at,b.id)<($4,$5))
        ORDER BY b.updated_at DESC,b.id DESC LIMIT $6"
    ))
    .bind(&user.id)
    .bind(workspace)
    .bind(query.q.unwrap_or_default().trim())
    .bind(cursor.as_ref().map(|c| c.updated_at))
    .bind(cursor.as_ref().map(|c| c.id.as_str()))
    .bind(i64::from(limit) + 1)
    .fetch_all(&state.db)
    .await?;
    let next_cursor = if items.len() > limit as usize {
        items.truncate(limit as usize);
        items.last().map(BoardCursor::encode)
    } else {
        None
    };
    Ok(Json(BoardPage { items, next_cursor }))
}

async fn get_board(
    State(state): State<AppState>,
    cookies: Cookies,
    Path(board_id): Path<String>,
) -> AppResult<Json<BoardSummary>> {
    let user = session::require_current_user(&cookies, &state).await?;
    Ok(Json(board_summary(&state.db, &board_id, &user.id).await?))
}

fn builtin_templates() -> Vec<Template> {
    let mut templates: Vec<Template> =
        serde_json::from_str(include_str!("../data/builtin-templates.json"))
            .expect("valid bundled templates");
    for template in &mut templates {
        template.id = format!("builtin:{}", template.id);
        template.builtin = true;
        template.element_count = template.elements.len();
    }
    templates
}

async fn template(db: &PgPool, template_id: &str, user_id: &str) -> AppResult<Template> {
    if template_id.starts_with("builtin:") {
        return builtin_templates()
            .into_iter()
            .find(|t| t.id == template_id)
            .ok_or_else(|| AppError::NotFound("Template not found".into()));
    }
    let row: Option<(String,String,Option<String>,String,Value)> = sqlx::query_as(
        "SELECT id,name,description,category,elements FROM board_templates WHERE id=$1 AND created_by_id=$2"
    ).bind(template_id).bind(user_id).fetch_optional(db).await?;
    let (id, name, description, category, elements) =
        row.ok_or_else(|| AppError::NotFound("Template not found".into()))?;
    let elements = elements
        .as_array()
        .cloned()
        .ok_or_else(|| AppError::BadRequest("Invalid template data.".into()))?;
    Ok(Template {
        id,
        name,
        description,
        category,
        builtin: false,
        element_count: elements.len(),
        elements,
    })
}

fn remint_elements(elements: Vec<Value>) -> AppResult<Vec<Value>> {
    validate_template_elements(&elements)?;
    let mut ids = std::collections::HashMap::new();
    for element in &elements {
        let old_id = element
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| AppError::BadRequest("Invalid element data.".into()))?;
        ids.insert(old_id.to_owned(), id());
    }
    let mut elements = elements;
    for element in &mut elements {
        let object = element
            .as_object_mut()
            .ok_or_else(|| AppError::BadRequest("Invalid element data.".into()))?;
        let old_id = object.get("id").and_then(Value::as_str).unwrap().to_owned();
        object.insert("id".into(), json!(ids[&old_id]));
        for field in ["frameId", "groupId"] {
            if let Some(old) = object.get(field).and_then(Value::as_str) {
                let new_id = ids.entry(old.to_owned()).or_insert_with(id).clone();
                object.insert(field.into(), json!(new_id));
            }
        }
        for field in ["startBinding", "endBinding"] {
            if let Some(binding) = object.get_mut(field).and_then(Value::as_object_mut) {
                if let Some(old) = binding.get("elementId").and_then(Value::as_str) {
                    let new_id = ids.get(old).cloned();
                    binding.insert("elementId".into(), json!(new_id));
                }
            }
        }
        if object.get("type").and_then(Value::as_str).is_none() {
            return Err(AppError::BadRequest("Invalid element type.".into()));
        }
    }
    Ok(elements)
}

async fn insert_board(
    state: &AppState,
    user_id: &str,
    input: BoardInput,
    elements: Vec<Value>,
    default_name: &str,
) -> AppResult<BoardSummary> {
    let board_name = name(input.name.as_deref().unwrap_or(default_name), 80)?;
    let workspace = input.workspace_id.as_deref();
    let mut tx = state.db.begin().await?;
    if let Some(workspace) = workspace {
        lock_resource(&mut tx, "workspaces", workspace).await?;
        workspace_role(&state.db, workspace, user_id, "MEMBER").await?;
    }
    // Lock the owner before counting so concurrent creates cannot bypass the free cap.
    sqlx::query("SELECT id FROM users WHERE id=$1 FOR UPDATE")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    let pro = is_pro(&state.db, user_id).await?;
    if workspace.is_none() && !pro {
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM boards WHERE owner_id=$1 AND workspace_id IS NULL",
        )
        .bind(user_id)
        .fetch_one(&mut *tx)
        .await?;
        if count >= FREE_MAX_OWNED_BOARDS {
            return Err(AppError::Forbidden(
                "Free plan includes 5 boards. Upgrade to Pro for unlimited boards.".into(),
            ));
        }
    }
    let images = elements.iter().filter(|e| e["type"] == "image").count() as i64;
    if images > billing::limits::image_count_cap(pro) {
        return Err(AppError::Forbidden(
            "Template or board exceeds your plan's image limit.".into(),
        ));
    }
    let board_id = id();
    sqlx::query("INSERT INTO boards(id,name,owner_id,workspace_id) VALUES($1,$2,$3,$4)")
        .bind(&board_id)
        .bind(&board_name)
        .bind(user_id)
        .bind(workspace)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO board_members(id,board_id,user_id,role) VALUES($1,$2,$3,'OWNER')")
        .bind(id())
        .bind(&board_id)
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    for element in remint_elements(elements)? {
        sqlx::query(
            "INSERT INTO board_elements(id,board_id,element_id,type,data) VALUES($1,$2,$3,$4,$5)",
        )
        .bind(id())
        .bind(&board_id)
        .bind(element["id"].as_str().unwrap())
        .bind(element["type"].as_str().unwrap())
        .bind(&element)
        .execute(&mut *tx)
        .await?;
    }
    // Initial content is revision zero; subsequent edits use the collaboration writer.
    sqlx::query("INSERT INTO board_collaboration_state(board_id,revision) VALUES($1,0)")
        .bind(&board_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    board_summary(&state.db, &board_id, user_id).await
}

async fn create_board(
    State(state): State<AppState>,
    cookies: Cookies,
    AppJson(input): AppJson<BoardInput>,
) -> AppResult<(StatusCode, Json<BoardSummary>)> {
    let user = session::require_current_user(&cookies, &state).await?;
    let selected = match input.template_id.as_deref() {
        Some(template_id) => Some(template(&state.db, template_id, &user.id).await?),
        None => None,
    };
    let (elements, default_name) = selected
        .map(|t| (t.elements, t.name))
        .unwrap_or((vec![], "Untitled board".into()));
    Ok((
        StatusCode::CREATED,
        Json(insert_board(&state, &user.id, input, elements, &default_name).await?),
    ))
}

async fn duplicate_board(
    State(state): State<AppState>,
    cookies: Cookies,
    Path(board_id): Path<String>,
    AppJson(input): AppJson<BoardInput>,
) -> AppResult<(StatusCode, Json<BoardSummary>)> {
    let user = session::require_current_user(&cookies, &state).await?;
    if input.template_id.is_some() {
        return Err(AppError::BadRequest(
            "templateId is not valid when duplicating a board.".into(),
        ));
    }
    let source = board_summary(&state.db, &board_id, &user.id).await?;
    let elements = sqlx::query_scalar(
        "SELECT data FROM board_elements WHERE board_id=$1 ORDER BY created_at,id",
    )
    .bind(&board_id)
    .fetch_all(&state.db)
    .await?;
    let default_name = format!(
        "{} (copy)",
        source.name.chars().take(73).collect::<String>()
    );
    Ok((
        StatusCode::CREATED,
        Json(insert_board(&state, &user.id, input, elements, &default_name).await?),
    ))
}

async fn rename_board(
    State(state): State<AppState>,
    cookies: Cookies,
    Path(board_id): Path<String>,
    AppJson(input): AppJson<NameInput>,
) -> AppResult<Json<BoardSummary>> {
    let user = session::require_current_user(&cookies, &state).await?;
    let name = name(&input.name, 80)?;
    let mut tx = state.db.begin().await?;
    lock_resource(&mut tx, "boards", &board_id).await?;
    authz::require_board_role(&state.db, &board_id, &user.id, BoardRole::Owner).await?;
    sqlx::query("UPDATE boards SET name=$2,updated_at=now() WHERE id=$1")
        .bind(&board_id)
        .bind(name)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(board_summary(&state.db, &board_id, &user.id).await?))
}

async fn delete_board(
    State(state): State<AppState>,
    cookies: Cookies,
    Path(board_id): Path<String>,
) -> AppResult<Json<Value>> {
    let user = session::require_current_user(&cookies, &state).await?;
    let mut tx = state.db.begin().await?;
    lock_resource(&mut tx, "boards", &board_id).await?;
    authz::require_board_role(&state.db, &board_id, &user.id, BoardRole::Owner).await?;
    sqlx::query("DELETE FROM boards WHERE id=$1")
        .bind(board_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(json!({"success":true})))
}

async fn list_templates(
    State(state): State<AppState>,
    cookies: Cookies,
) -> AppResult<Json<Items<Template>>> {
    let user = session::require_current_user(&cookies, &state).await?;
    let mut items = builtin_templates();
    let rows: Vec<(String,String,Option<String>,String,i32)> = sqlx::query_as(
        "SELECT id,name,description,category,jsonb_array_length(elements) FROM board_templates WHERE created_by_id=$1 ORDER BY created_at DESC,id"
    ).bind(&user.id).fetch_all(&state.db).await?;
    items.extend(
        rows.into_iter()
            .map(|(id, name, description, category, count)| Template {
                id,
                name,
                description,
                category,
                builtin: false,
                elements: vec![],
                element_count: count as usize,
            }),
    );
    Ok(Json(Items { items }))
}

async fn save_template(
    State(state): State<AppState>,
    cookies: Cookies,
    Path(board_id): Path<String>,
    AppJson(input): AppJson<TemplateInput>,
) -> AppResult<(StatusCode, Json<Template>)> {
    let user = session::require_current_user(&cookies, &state).await?;
    if !is_pro(&state.db, &user.id).await? {
        return Err(AppError::Forbidden(
            "Saving templates requires an active Pro subscription.".into(),
        ));
    }
    let name = name(&input.name, 80)?;
    let description = template_description(input.description)?;
    let mut tx = state.db.begin().await?;
    lock_resource(&mut tx, "boards", &board_id).await?;
    authz::require_board_role(&state.db, &board_id, &user.id, BoardRole::Owner).await?;
    let elements: Vec<Value> = sqlx::query_scalar(
        "SELECT data FROM board_elements WHERE board_id=$1 ORDER BY created_at,id",
    )
    .bind(board_id)
    .fetch_all(&mut *tx)
    .await?;
    let template_id = id();
    sqlx::query("INSERT INTO board_templates(id,name,description,created_by_id,elements) VALUES($1,$2,$3,$4,$5)")
        .bind(&template_id).bind(&name).bind(&description).bind(&user.id).bind(json!(elements)).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok((
        StatusCode::CREATED,
        Json(Template {
            id: template_id,
            name,
            description,
            category: "Yours".into(),
            builtin: false,
            element_count: elements.len(),
            elements: vec![],
        }),
    ))
}

async fn delete_template(
    State(state): State<AppState>,
    cookies: Cookies,
    Path(template_id): Path<String>,
) -> AppResult<Json<Value>> {
    let user = session::require_current_user(&cookies, &state).await?;
    if template_id.starts_with("builtin:") {
        return Err(AppError::BadRequest(
            "Built-in templates cannot be deleted.".into(),
        ));
    }

    let result = sqlx::query("DELETE FROM board_templates WHERE id=$1 AND created_by_id=$2")
        .bind(template_id)
        .bind(user.id)
        .execute(&state.db)
        .await?;
    if result.rows_affected() == 0 {
        return Err(AppError::NotFound("Template not found".into()));
    }
    Ok(Json(json!({"success":true})))
}

async fn import_template(
    State(state): State<AppState>,
    cookies: Cookies,
    AppJson(input): AppJson<ImportTemplateInput>,
) -> AppResult<(StatusCode, Json<Template>)> {
    let user = session::require_current_user(&cookies, &state).await?;
    if !is_pro(&state.db, &user.id).await? {
        return Err(AppError::Forbidden(
            "Saving templates requires an active Pro subscription.".into(),
        ));
    }
    let name = name(&input.name, 80)?;
    let description = template_description(input.description)?;
    validate_template_elements(&input.elements)?;
    let images = input
        .elements
        .iter()
        .filter(|e| e["type"] == "image")
        .count() as i64;
    if images > billing::limits::image_count_cap(true) {
        return Err(AppError::Forbidden(
            "Template exceeds your plan's image limit.".into(),
        ));
    }
    let template_id = id();
    sqlx::query("INSERT INTO board_templates(id,name,description,created_by_id,elements) VALUES($1,$2,$3,$4,$5)")
            .bind(&template_id).bind(&name).bind(&description).bind(&user.id).bind(json!(input.elements)).execute(&state.db).await?;
    Ok((
        StatusCode::CREATED,
        Json(Template {
            id: template_id,
            name,
            description,
            category: "Yours".into(),
            builtin: false,
            element_count: input.elements.len(),
            elements: vec![],
        }),
    ))
}
async fn list_workspaces(
    State(state): State<AppState>,
    cookies: Cookies,
) -> AppResult<Json<Items<WorkspaceSummary>>> {
    let user = session::require_current_user(&cookies, &state).await?;
    Ok(Json(Items {
        items: workspace_list(&state.db, &user.id).await?,
    }))
}

async fn workspace_summary(
    db: &PgPool,
    workspace_id: &str,
    user_id: &str,
) -> AppResult<WorkspaceSummary> {
    workspace_role(db, workspace_id, user_id, "MEMBER").await?;
    Ok(sqlx::query_as("SELECT w.id,w.name,w.slug,m.role::text AS role FROM workspaces w JOIN workspace_members m ON m.workspace_id=w.id WHERE w.id=$1 AND m.user_id=$2")
        .bind(workspace_id).bind(user_id).fetch_one(db).await?)
}

async fn create_workspace(
    State(state): State<AppState>,
    cookies: Cookies,
    AppJson(input): AppJson<NameInput>,
) -> AppResult<(StatusCode, Json<WorkspaceSummary>)> {
    let user = session::require_current_user(&cookies, &state).await?;
    require_pro(&state.db, &user.id).await?;
    let name = workspace_name(&input.name)?;
    let workspace_id = id();
    let base: String = name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let slug = format!(
        "{}-{}",
        base.trim_matches('-').chars().take(32).collect::<String>(),
        workspace_id
    );
    let mut tx = state.db.begin().await?;
    sqlx::query("INSERT INTO workspaces(id,name,slug,created_by_id) VALUES($1,$2,$3,$4)")
        .bind(&workspace_id)
        .bind(&name)
        .bind(&slug)
        .bind(&user.id)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO workspace_members(id,workspace_id,user_id,role) VALUES($1,$2,$3,'OWNER')",
    )
    .bind(id())
    .bind(&workspace_id)
    .bind(&user.id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok((
        StatusCode::CREATED,
        Json(WorkspaceSummary {
            id: workspace_id,
            name,
            slug,
            role: "OWNER".into(),
        }),
    ))
}

async fn members(db: &PgPool, resource: &str, resource_id: &str) -> AppResult<Vec<Member>> {
    let sql = if resource == "boards" {
        "SELECT m.id,m.user_id,u.name,u.email,u.image,m.role::text AS role FROM board_members m JOIN users u ON u.id=m.user_id WHERE m.board_id=$1 ORDER BY m.created_at,m.id"
    } else {
        "SELECT m.id,m.user_id,u.name,u.email,u.image,m.role::text AS role FROM workspace_members m JOIN users u ON u.id=m.user_id WHERE m.workspace_id=$1 ORDER BY m.created_at,m.id"
    };
    Ok(sqlx::query_as(sql).bind(resource_id).fetch_all(db).await?)
}

async fn get_workspace(
    State(state): State<AppState>,
    cookies: Cookies,
    Path(workspace_id): Path<String>,
) -> AppResult<Json<WorkspaceDetail>> {
    let user = session::require_current_user(&cookies, &state).await?;
    let summary = workspace_summary(&state.db, &workspace_id, &user.id).await?;
    Ok(Json(WorkspaceDetail {
        summary,
        members: members(&state.db, "workspaces", &workspace_id).await?,
    }))
}

async fn rename_workspace(
    State(state): State<AppState>,
    cookies: Cookies,
    Path(workspace_id): Path<String>,
    AppJson(input): AppJson<NameInput>,
) -> AppResult<Json<WorkspaceSummary>> {
    let user = session::require_current_user(&cookies, &state).await?;
    let name = workspace_name(&input.name)?;
    let mut tx = state.db.begin().await?;
    lock_resource(&mut tx, "workspaces", &workspace_id).await?;
    workspace_role(&state.db, &workspace_id, &user.id, "ADMIN").await?;
    sqlx::query("UPDATE workspaces SET name=$2,updated_at=now() WHERE id=$1")
        .bind(&workspace_id)
        .bind(name)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(
        workspace_summary(&state.db, &workspace_id, &user.id).await?,
    ))
}

async fn delete_workspace(
    State(state): State<AppState>,
    cookies: Cookies,
    Path(workspace_id): Path<String>,
) -> AppResult<Json<Value>> {
    let user = session::require_current_user(&cookies, &state).await?;
    let mut tx = state.db.begin().await?;
    lock_resource(&mut tx, "workspaces", &workspace_id).await?;
    workspace_role(&state.db, &workspace_id, &user.id, "OWNER").await?;
    sqlx::query("DELETE FROM boards WHERE workspace_id=$1")
        .bind(&workspace_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM workspaces WHERE id=$1")
        .bind(&workspace_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(json!({"success":true})))
}

async fn workspace_members(
    State(state): State<AppState>,
    cookies: Cookies,
    Path(workspace_id): Path<String>,
) -> AppResult<Json<Items<Member>>> {
    let user = session::require_current_user(&cookies, &state).await?;
    workspace_role(&state.db, &workspace_id, &user.id, "MEMBER").await?;
    Ok(Json(Items {
        items: members(&state.db, "workspaces", &workspace_id).await?,
    }))
}

async fn board_members(
    State(state): State<AppState>,
    cookies: Cookies,
    Path(board_id): Path<String>,
) -> AppResult<Json<Items<Member>>> {
    let user = session::require_current_user(&cookies, &state).await?;
    authz::require_board_role(&state.db, &board_id, &user.id, BoardRole::Viewer).await?;
    Ok(Json(Items {
        items: members(&state.db, "boards", &board_id).await?,
    }))
}

async fn add_member(
    state: &AppState,
    actor_id: &str,
    resource: &str,
    resource_id: &str,
    input: MemberInput,
) -> AppResult<Member> {
    let workspace = resource == "workspaces";
    let role = member_role(&input.role, workspace)?;
    let email = input.email.trim().to_lowercase();
    if email.len() > 254 || !email.validate_email() {
        return Err(AppError::BadRequest("Enter a valid email.".into()));
    }
    let mut tx = state.db.begin().await?;
    lock_resource(&mut tx, resource, resource_id).await?;
    let owner_id = if workspace {
        workspace_role(&state.db, resource_id, actor_id, "ADMIN").await?;
        None
    } else {
        Some(
            authz::require_board_role(&state.db, resource_id, actor_id, BoardRole::Owner)
                .await?
                .owner_id,
        )
    };
    let invitee: String = sqlx::query_scalar(
        "SELECT id FROM users WHERE lower(email)=$1 ORDER BY created_at,id LIMIT 1",
    )
    .bind(email)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| AppError::NotFound("No SketchFlow account exists for that email.".into()))?;
    if owner_id.as_deref() == Some(invitee.as_str()) {
        return Err(AppError::Conflict(
            "That person is already the board owner.".into(),
        ));
    }
    if workspace && !is_pro(&state.db, &invitee).await? {
        return Err(AppError::Forbidden(
            "This user needs an active Pro subscription to join a workspace.".into(),
        ));
    }
    let existing = if workspace {
        "SELECT EXISTS(SELECT 1 FROM workspace_members WHERE workspace_id=$1 AND user_id=$2)"
    } else {
        "SELECT EXISTS(SELECT 1 FROM board_members WHERE board_id=$1 AND user_id=$2)"
    };
    if sqlx::query_scalar::<_, bool>(existing)
        .bind(resource_id)
        .bind(&invitee)
        .fetch_one(&mut *tx)
        .await?
    {
        return Err(AppError::Conflict(
            "That person is already a member.".into(),
        ));
    }
    if let Some(owner_id) = owner_id {
        if !is_pro(&state.db, &owner_id).await? {
            let count: i64 = sqlx::query_scalar(
                "SELECT count(*)+1 FROM board_members WHERE board_id=$1 AND user_id<>$2",
            )
            .bind(resource_id)
            .bind(&owner_id)
            .fetch_one(&mut *tx)
            .await?;
            if count >= FREE_MAX_BOARD_MEMBERS {
                return Err(AppError::Forbidden(
                    "Free plan includes 2 board members. Upgrade to Pro for unlimited members."
                        .into(),
                ));
            }
        }
    }
    let resource_name = if workspace {
        sqlx::query_scalar::<_, String>("SELECT name FROM workspaces WHERE id=$1")
            .bind(resource_id)
            .fetch_one(&mut *tx)
            .await?
    } else {
        sqlx::query_scalar::<_, String>("SELECT name FROM boards WHERE id=$1")
            .bind(resource_id)
            .fetch_one(&mut *tx)
            .await?
    };
    let actor_name = sqlx::query_scalar::<_, Option<String>>("SELECT name FROM users WHERE id=$1")
        .bind(actor_id)
        .fetch_one(&mut *tx)
        .await?;
    let member_id = id();
    let sql = if workspace {
        "INSERT INTO workspace_members(id,workspace_id,user_id,role) VALUES($1,$2,$3,$4::workspace_role)"
    } else {
        "INSERT INTO board_members(id,board_id,user_id,role) VALUES($1,$2,$3,$4::board_role)"
    };
    sqlx::query(sql)
        .bind(&member_id)
        .bind(resource_id)
        .bind(&invitee)
        .bind(role)
        .execute(&mut *tx)
        .await?;
    notifications::write(
        &mut tx,
        NewNotification {
            actor_id: actor_id.to_string(),
            actor_name,
            recipient_ids: vec![invitee],
            notification_type: if workspace {
                NotificationType::WorkspaceJoined
            } else {
                NotificationType::BoardShared
            },
            event_key: if workspace {
                format!("workspace-joined:{member_id}")
            } else {
                format!("board-shared:{member_id}")
            },
            board_id: (!workspace).then(|| resource_id.to_string()),
            workspace_id: workspace.then(|| resource_id.to_string()),
            comment_id: None,
            resource_name,
            role: Some(role.to_string()),
        },
    )
    .await?;
    tx.commit().await?;
    members(&state.db, resource, resource_id)
        .await?
        .into_iter()
        .find(|m| m.id == member_id)
        .ok_or_else(|| AppError::NotFound("Member not found".into()))
}

async fn change_member(
    state: &AppState,
    actor_id: &str,
    resource: &str,
    resource_id: &str,
    member_id: &str,
    new_role: Option<&str>,
) -> AppResult<Option<Member>> {
    let workspace = resource == "workspaces";
    if let Some(role) = new_role {
        member_role(role, workspace)?;
    }
    let mut tx = state.db.begin().await?;
    lock_resource(&mut tx, resource, resource_id).await?;
    if workspace {
        workspace_role(&state.db, resource_id, actor_id, "ADMIN").await?;
    } else {
        authz::require_board_role(&state.db, resource_id, actor_id, BoardRole::Owner).await?;
    }
    let member = members(&state.db, resource, resource_id)
        .await?
        .into_iter()
        .find(|m| m.id == member_id)
        .ok_or_else(|| AppError::NotFound("Member not found".into()))?;
    if member.role == "OWNER" {
        return Err(AppError::Forbidden(
            "The owner cannot be removed or have their role changed.".into(),
        ));
    }
    if let Some(role) = new_role {
        if workspace {
            require_pro(&state.db, &member.user_id).await?;
        }
        let sql = if workspace {
            "UPDATE workspace_members SET role=$3::workspace_role WHERE workspace_id=$1 AND id=$2"
        } else {
            "UPDATE board_members SET role=$3::board_role WHERE board_id=$1 AND id=$2"
        };
        sqlx::query(sql)
            .bind(resource_id)
            .bind(member_id)
            .bind(role)
            .execute(&mut *tx)
            .await?;
        if !workspace && member.role != role {
            let board_name = sqlx::query_scalar::<_, String>("SELECT name FROM boards WHERE id=$1")
                .bind(resource_id)
                .fetch_one(&mut *tx)
                .await?;
            let actor_name =
                sqlx::query_scalar::<_, Option<String>>("SELECT name FROM users WHERE id=$1")
                    .bind(actor_id)
                    .fetch_one(&mut *tx)
                    .await?;
            notifications::write(
                &mut tx,
                NewNotification {
                    actor_id: actor_id.to_string(),
                    actor_name,
                    recipient_ids: vec![member.user_id.clone()],
                    notification_type: NotificationType::BoardRoleChanged,
                    event_key: format!("board-role:{}", id()),
                    board_id: Some(resource_id.to_string()),
                    workspace_id: None,
                    comment_id: None,
                    resource_name: board_name,
                    role: Some(role.to_string()),
                },
            )
            .await?;
        }
    } else {
        let sql = if workspace {
            "DELETE FROM workspace_members WHERE workspace_id=$1 AND id=$2"
        } else {
            "DELETE FROM board_members WHERE board_id=$1 AND id=$2"
        };
        sqlx::query(sql)
            .bind(resource_id)
            .bind(member_id)
            .execute(&mut *tx)
            .await?;
        let notification_sql = if workspace {
            "DELETE FROM notifications WHERE workspace_id=$1 AND recipient_id=$2"
        } else {
            "DELETE FROM notifications WHERE board_id=$1 AND recipient_id=$2"
        };
        sqlx::query(notification_sql)
            .bind(resource_id)
            .bind(&member.user_id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(new_role.map(|role| Member {
        role: role.into(),
        ..member
    }))
}

async fn add_workspace_member(
    State(state): State<AppState>,
    cookies: Cookies,
    Path(resource_id): Path<String>,
    AppJson(input): AppJson<MemberInput>,
) -> AppResult<(StatusCode, Json<Member>)> {
    let user = session::require_current_user(&cookies, &state).await?;
    Ok((
        StatusCode::CREATED,
        Json(add_member(&state, &user.id, "workspaces", &resource_id, input).await?),
    ))
}

async fn add_board_member(
    State(state): State<AppState>,
    cookies: Cookies,
    Path(resource_id): Path<String>,
    AppJson(input): AppJson<MemberInput>,
) -> AppResult<(StatusCode, Json<Member>)> {
    let user = session::require_current_user(&cookies, &state).await?;
    Ok((
        StatusCode::CREATED,
        Json(add_member(&state, &user.id, "boards", &resource_id, input).await?),
    ))
}

async fn update_workspace_member(
    State(state): State<AppState>,
    cookies: Cookies,
    Path((resource_id, member_id)): Path<(String, String)>,
    AppJson(input): AppJson<RoleInput>,
) -> AppResult<Json<Member>> {
    let user = session::require_current_user(&cookies, &state).await?;
    Ok(Json(
        change_member(
            &state,
            &user.id,
            "workspaces",
            &resource_id,
            &member_id,
            Some(&input.role),
        )
        .await?
        .unwrap(),
    ))
}

async fn update_board_member(
    State(state): State<AppState>,
    cookies: Cookies,
    Path((resource_id, member_id)): Path<(String, String)>,
    AppJson(input): AppJson<RoleInput>,
) -> AppResult<Json<Member>> {
    let user = session::require_current_user(&cookies, &state).await?;
    Ok(Json(
        change_member(
            &state,
            &user.id,
            "boards",
            &resource_id,
            &member_id,
            Some(&input.role),
        )
        .await?
        .unwrap(),
    ))
}

async fn remove_workspace_member(
    State(state): State<AppState>,
    cookies: Cookies,
    Path((resource_id, member_id)): Path<(String, String)>,
) -> AppResult<Json<Value>> {
    let user = session::require_current_user(&cookies, &state).await?;
    change_member(
        &state,
        &user.id,
        "workspaces",
        &resource_id,
        &member_id,
        None,
    )
    .await?;
    Ok(Json(json!({"success":true})))
}

async fn remove_board_member(
    State(state): State<AppState>,
    cookies: Cookies,
    Path((resource_id, member_id)): Path<(String, String)>,
) -> AppResult<Json<Value>> {
    let user = session::require_current_user(&cookies, &state).await?;
    change_member(&state, &user.id, "boards", &resource_id, &member_id, None).await?;
    Ok(Json(json!({"success":true})))
}

async fn leave_workspace(
    State(state): State<AppState>,
    cookies: Cookies,
    Path(workspace_id): Path<String>,
) -> AppResult<Json<Value>> {
    let user = session::require_current_user(&cookies, &state).await?;
    let mut tx = state.db.begin().await?;
    lock_resource(&mut tx, "workspaces", &workspace_id).await?;
    let role = workspace_role(&state.db, &workspace_id, &user.id, "MEMBER").await?;
    if role == "OWNER" {
        return Err(AppError::Forbidden(
            "The owner must delete the workspace instead of leaving.".into(),
        ));
    }

    sqlx::query("DELETE FROM workspace_members WHERE workspace_id=$1 AND user_id=$2")
        .bind(&workspace_id)
        .bind(&user.id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM notifications WHERE workspace_id=$1 AND recipient_id=$2")
        .bind(&workspace_id)
        .bind(&user.id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(json!({"success":true})))
}

#[cfg(test)]
#[path = "dashboard_tests.rs"]
mod tests;
