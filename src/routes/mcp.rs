pub mod oauth;

use std::{sync::Arc, time::Duration};

use axum::{
    body::Body,
    extract::State,
    http::{header, request::Parts, Request, StatusCode},
    response::{IntoResponse, Response},
    routing::any,
    Router,
};
use chrono::{DateTime, Utc};
use rmcp::{
    handler::server::{tool::Extension as McpExtension, wrapper::Parameters},
    model::{CallToolResult, ContentBlock},
    tool, tool_router,
    transport::streamable_http_server::{
        session::never::NeverSessionManager, StreamableHttpServerConfig, StreamableHttpService,
    },
    ErrorData,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use tower::Service;

use crate::{
    authz::{self, BoardRole},
    editor::{schema, store},
    error::{AppError, AppResult},
    routes::collaboration::{self, ElementBatch},
    state::AppState,
};

pub const MCP_READ_SCOPE: &str = "boards:read";
pub const MCP_WRITE_SCOPE: &str = "boards:write";
const MAX_DRAW_ELEMENTS: usize = 100;
const MAX_BOARD_PAGE_SIZE: usize = 250;
const MAX_COORDINATE: f64 = 1_000_000.0;

#[derive(Clone, Debug)]
pub struct McpPrincipal {
    user_id: String,
    client_id: String,
    grant_id: String,
    scopes: Vec<String>,
}

impl McpPrincipal {
    fn has_scope(&self, scope: &str) -> bool {
        self.scopes.iter().any(|granted| granted == scope)
    }
}

#[derive(Clone)]
struct McpHttpState {
    app: AppState,
    service: StreamableHttpService<McpServer, NeverSessionManager>,
    resource_metadata_url: String,
}

#[derive(Clone)]
struct McpServer {
    app: AppState,
}

pub fn router(app: AppState) -> Router {
    let public_base = app.config.public_base_url.trim_end_matches('/');
    let public_url = reqwest::Url::parse(public_base)
        .expect("PUBLIC_BASE_URL must be a valid URL for the MCP server");
    let mut allowed_host = public_url
        .host_str()
        .expect("PUBLIC_BASE_URL must include a host")
        .to_owned();
    if let Some(port) = public_url.port() {
        allowed_host.push(':');
        allowed_host.push_str(&port.to_string());
    }

    let service_app = app.clone();
    let http_config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_max_request_body_bytes(2 * 1024 * 1024)
        .with_allowed_hosts([allowed_host])
        .with_allowed_origins(app.config.cors_allowed_origins.clone())
        .enforce_origin_validation()
        .with_sse_keep_alive(Some(Duration::from_secs(15)));
    let service = StreamableHttpService::new(
        move || {
            Ok(McpServer {
                app: service_app.clone(),
            })
        },
        Arc::new(NeverSessionManager::default()),
        http_config,
    );
    let resource_metadata_url =
        format!("{public_base}/.well-known/oauth-protected-resource/api/mcp");

    Router::new()
        .route("/api/mcp", any(mcp_endpoint))
        .with_state(Arc::new(McpHttpState {
            app,
            service,
            resource_metadata_url,
        }))
}

async fn mcp_endpoint(
    State(state): State<Arc<McpHttpState>>,
    mut request: Request<Body>,
) -> Response {
    let bearer = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|header| header.to_str().ok())
        .and_then(|value| {
            let (scheme, token) = value.split_once(' ')?;
            (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty()).then_some(token)
        });
    let Some(bearer) = bearer else {
        return unauthorized(&state.resource_metadata_url);
    };
    let principal = match authenticate_access_token(&state.app, bearer).await {
        Ok(Some(principal)) => principal,
        Ok(None) => return unauthorized(&state.resource_metadata_url),
        Err(error) => {
            tracing::error!(
                subsystem = "mcp",
                event = "access_token_lookup_failed",
                error = %error,
                "Could not authenticate MCP request"
            );
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    request.extensions_mut().insert(principal);
    let response = match state.service.clone().call(request).await {
        Ok(response) => response,
        Err(never) => match never {},
    };
    let (parts, body) = response.into_parts();
    Response::from_parts(parts, Body::new(body))
}

fn unauthorized(resource_metadata_url: &str) -> Response {
    let mut response = StatusCode::UNAUTHORIZED.into_response();
    let challenge =
        format!("Bearer resource_metadata=\"{resource_metadata_url}\", scope=\"{MCP_READ_SCOPE}\"");
    if let Ok(value) = challenge.parse() {
        response
            .headers_mut()
            .insert(header::WWW_AUTHENTICATE, value);
    }
    response
}

async fn authenticate_access_token(app: &AppState, token: &str) -> AppResult<Option<McpPrincipal>> {
    let token_hash = hex::encode(Sha256::digest(token.as_bytes()));
    sqlx::query_as::<_, (String, String, String, Vec<String>)>(
        "SELECT g.user_id, g.client_id, g.id, g.scopes
         FROM mcp_oauth_access_tokens t
         JOIN mcp_oauth_grants g ON g.id = t.grant_id
         WHERE t.token_hash = $1 AND t.expires_at > now()
           AND t.revoked_at IS NULL AND g.revoked_at IS NULL",
    )
    .bind(token_hash)
    .fetch_optional(&app.db)
    .await
    .map(|principal| {
        principal.map(|(user_id, client_id, grant_id, scopes)| McpPrincipal {
            user_id,
            client_id,
            grant_id,
            scopes,
        })
    })
    .map_err(AppError::from)
}

fn principal_from_parts(parts: &Parts) -> Result<McpPrincipal, ErrorData> {
    parts
        .extensions
        .get::<McpPrincipal>()
        .cloned()
        .ok_or_else(|| ErrorData::internal_error("MCP principal is missing.", None))
}

fn tool_error(error: impl std::fmt::Display) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(error.to_string())])
}

fn structured_result(value: Value) -> CallToolResult {
    CallToolResult::structured(value)
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ListBoardsInput {
    #[schemars(description = "Zero-based offset into the accessible board list.")]
    offset: Option<i64>,
    #[schemars(description = "Maximum boards to return (1-100).")]
    limit: Option<i64>,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
struct BoardListEntry {
    id: String,
    name: String,
    role: String,
    revision: i64,
    updated_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct GetBoardInput {
    board_id: String,
    #[schemars(description = "Zero-based offset into the board elements.")]
    offset: Option<usize>,
    #[schemars(description = "Maximum elements to return (1-250).")]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum ShapeKind {
    Rectangle,
    Ellipse,
    Diamond,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ElementStyle {
    stroke_color: Option<String>,
    background_color: Option<String>,
    stroke_width: Option<f64>,
    opacity: Option<f64>,
    fill_style: Option<String>,
    stroke_style: Option<String>,
    roundness: Option<String>,
    roughness: Option<f64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ShapeInput {
    shape: ShapeKind,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    style: Option<ElementStyle>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DrawShapesInput {
    board_id: String,
    base_revision: i64,
    client_operation_id: String,
    shapes: Vec<ShapeInput>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PointInput {
    x: f64,
    y: f64,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DrawFreehandInput {
    board_id: String,
    base_revision: i64,
    client_operation_id: String,
    points: Vec<PointInput>,
    style: Option<ElementStyle>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DrawConnectorInput {
    board_id: String,
    base_revision: i64,
    client_operation_id: String,
    kind: ConnectorKind,
    start: PointInput,
    end: PointInput,
    style: Option<ElementStyle>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum ConnectorKind {
    Line,
    Arrow,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AddTextInput {
    board_id: String,
    base_revision: i64,
    client_operation_id: String,
    x: f64,
    y: f64,
    text: String,
    width: Option<f64>,
    style: Option<ElementStyle>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct UpdateElementInput {
    id: String,
    x: Option<f64>,
    y: Option<f64>,
    width: Option<f64>,
    height: Option<f64>,
    rotation: Option<f64>,
    stroke_color: Option<String>,
    background_color: Option<String>,
    opacity: Option<f64>,
    text: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct UpdateElementsInput {
    board_id: String,
    base_revision: i64,
    client_operation_id: String,
    updates: Vec<UpdateElementInput>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DeleteElementsInput {
    board_id: String,
    base_revision: i64,
    client_operation_id: String,
    element_ids: Vec<String>,
}

#[tool_router(server_handler)]
impl McpServer {
    #[tool(
        name = "list_boards",
        description = "List boards the authenticated user can view. Use the returned board ID with get_board or drawing tools."
    )]
    async fn list_boards(
        &self,
        Parameters(input): Parameters<ListBoardsInput>,
        McpExtension(parts): McpExtension<Parts>,
    ) -> Result<CallToolResult, ErrorData> {
        let principal = principal_from_parts(&parts)?;
        if !principal.has_scope(MCP_READ_SCOPE) {
            return Ok(tool_error(
                "This authorization does not include boards:read.",
            ));
        }
        let offset = input.offset.unwrap_or(0).clamp(0, 100_000);
        let limit = input.limit.unwrap_or(50).clamp(1, 100);
        match list_accessible_boards(&self.app, &principal, offset, limit).await {
            Ok(boards) => {
                audit_read(&principal, "list_boards", None);
                Ok(structured_result(json!({ "boards": boards })))
            }
            Err(error) => Ok(tool_error(error)),
        }
    }

    #[tool(
        name = "get_board",
        description = "Read a board snapshot and revision. Elements are paginated; pass base_revision from this result when writing."
    )]
    async fn get_board(
        &self,
        Parameters(input): Parameters<GetBoardInput>,
        McpExtension(parts): McpExtension<Parts>,
    ) -> Result<CallToolResult, ErrorData> {
        let principal = principal_from_parts(&parts)?;
        if !principal.has_scope(MCP_READ_SCOPE) {
            return Ok(tool_error(
                "This authorization does not include boards:read.",
            ));
        }
        match get_board_snapshot(&self.app, &principal, input).await {
            Ok(snapshot) => {
                audit_read(&principal, "get_board", snapshot["boardId"].as_str());
                Ok(structured_result(snapshot))
            }
            Err(error) => Ok(tool_error(error)),
        }
    }

    #[tool(
        name = "draw_shapes",
        description = "Create rectangle, ellipse, or diamond elements on a board. Coordinates are canvas world coordinates."
    )]
    async fn draw_shapes(
        &self,
        Parameters(input): Parameters<DrawShapesInput>,
        McpExtension(parts): McpExtension<Parts>,
    ) -> Result<CallToolResult, ErrorData> {
        let principal = principal_from_parts(&parts)?;
        if !principal.has_scope(MCP_WRITE_SCOPE) {
            return Ok(tool_error(
                "Drawing requires the separate boards:write authorization.",
            ));
        }
        match draw_shapes(&self.app, &principal, input).await {
            Ok(result) => Ok(structured_result(result)),
            Err(error) => Ok(tool_error(error)),
        }
    }

    #[tool(
        name = "draw_freehand",
        description = "Create a freehand stroke from absolute canvas world-coordinate points."
    )]
    async fn draw_freehand(
        &self,
        Parameters(input): Parameters<DrawFreehandInput>,
        McpExtension(parts): McpExtension<Parts>,
    ) -> Result<CallToolResult, ErrorData> {
        let principal = principal_from_parts(&parts)?;
        if !principal.has_scope(MCP_WRITE_SCOPE) {
            return Ok(tool_error(
                "Drawing requires the separate boards:write authorization.",
            ));
        }
        match draw_freehand(&self.app, &principal, input).await {
            Ok(result) => Ok(structured_result(result)),
            Err(error) => Ok(tool_error(error)),
        }
    }

    #[tool(
        name = "draw_connector",
        description = "Create a line or arrow between two absolute canvas world-coordinate points."
    )]
    async fn draw_connector(
        &self,
        Parameters(input): Parameters<DrawConnectorInput>,
        McpExtension(parts): McpExtension<Parts>,
    ) -> Result<CallToolResult, ErrorData> {
        let principal = principal_from_parts(&parts)?;
        if !principal.has_scope(MCP_WRITE_SCOPE) {
            return Ok(tool_error(
                "Drawing requires the separate boards:write authorization.",
            ));
        }
        match draw_connector(&self.app, &principal, input).await {
            Ok(result) => Ok(structured_result(result)),
            Err(error) => Ok(tool_error(error)),
        }
    }

    #[tool(
        name = "add_text",
        description = "Add a text element at a canvas world-coordinate position."
    )]
    async fn add_text(
        &self,
        Parameters(input): Parameters<AddTextInput>,
        McpExtension(parts): McpExtension<Parts>,
    ) -> Result<CallToolResult, ErrorData> {
        let principal = principal_from_parts(&parts)?;
        if !principal.has_scope(MCP_WRITE_SCOPE) {
            return Ok(tool_error(
                "Drawing requires the separate boards:write authorization.",
            ));
        }
        match add_text(&self.app, &principal, input).await {
            Ok(result) => Ok(structured_result(result)),
            Err(error) => Ok(tool_error(error)),
        }
    }

    #[tool(
        name = "update_elements",
        description = "Move, resize, rotate, recolor, or edit text on existing elements. Only allowlisted fields can be changed."
    )]
    async fn update_elements(
        &self,
        Parameters(input): Parameters<UpdateElementsInput>,
        McpExtension(parts): McpExtension<Parts>,
    ) -> Result<CallToolResult, ErrorData> {
        let principal = principal_from_parts(&parts)?;
        if !principal.has_scope(MCP_WRITE_SCOPE) {
            return Ok(tool_error(
                "Editing requires the separate boards:write authorization.",
            ));
        }
        match update_elements(&self.app, &principal, input).await {
            Ok(result) => Ok(structured_result(result)),
            Err(error) => Ok(tool_error(error)),
        }
    }

    #[tool(
        name = "delete_elements",
        description = "Delete one or more unlocked elements from a board."
    )]
    async fn delete_elements(
        &self,
        Parameters(input): Parameters<DeleteElementsInput>,
        McpExtension(parts): McpExtension<Parts>,
    ) -> Result<CallToolResult, ErrorData> {
        let principal = principal_from_parts(&parts)?;
        if !principal.has_scope(MCP_WRITE_SCOPE) {
            return Ok(tool_error(
                "Deleting requires the separate boards:write authorization.",
            ));
        }
        match delete_elements(&self.app, &principal, input).await {
            Ok(result) => Ok(structured_result(result)),
            Err(error) => Ok(tool_error(error)),
        }
    }
}

async fn list_accessible_boards(
    app: &AppState,
    principal: &McpPrincipal,
    offset: i64,
    limit: i64,
) -> AppResult<Vec<BoardListEntry>> {
    let workspace_access = store::user_is_pro(&app.db, &principal.user_id).await?;
    let board_ids = sqlx::query_scalar::<_, String>(
        "SELECT b.id FROM boards b
         WHERE b.deleted_at IS NULL AND (
           b.owner_id = $1
           OR EXISTS (
             SELECT 1 FROM board_members bm
             WHERE bm.board_id = b.id AND bm.user_id = $1
           )
           OR ($2 AND EXISTS (
             SELECT 1 FROM workspace_members wm
             WHERE wm.workspace_id = b.workspace_id AND wm.user_id = $1
           ))
         )
         ORDER BY b.updated_at DESC, b.id DESC
         OFFSET $3 LIMIT $4",
    )
    .bind(&principal.user_id)
    .bind(workspace_access)
    .bind(offset)
    .bind(limit)
    .fetch_all(&app.db)
    .await?;

    let mut result = Vec::with_capacity(board_ids.len());
    for board_id in board_ids {
        let Some(access) = authz::get_board_access(&app.db, &board_id, &principal.user_id).await?
        else {
            continue;
        };
        let entry = sqlx::query_as::<_, BoardListEntry>(
            "SELECT b.id, b.name, $2::text AS role,
                    COALESCE(s.revision, 0) AS revision, b.updated_at
             FROM boards b
             LEFT JOIN board_collaboration_state s ON s.board_id = b.id
             WHERE b.id = $1 AND b.deleted_at IS NULL",
        )
        .bind(&board_id)
        .bind(role_name(access.role))
        .fetch_optional(&app.db)
        .await?;
        if let Some(entry) = entry {
            result.push(entry);
        }
    }
    Ok(result)
}

async fn get_board_snapshot(
    app: &AppState,
    principal: &McpPrincipal,
    input: GetBoardInput,
) -> AppResult<Value> {
    authz::require_board_role(
        &app.db,
        &input.board_id,
        &principal.user_id,
        BoardRole::Viewer,
    )
    .await?;
    let snapshot = collaboration::load_snapshot(app, &input.board_id).await?;
    let offset = input.offset.unwrap_or(0).min(snapshot.elements.len());
    let limit = input.limit.unwrap_or(100).clamp(1, MAX_BOARD_PAGE_SIZE);
    let end = offset.saturating_add(limit).min(snapshot.elements.len());
    let elements = &snapshot.elements[offset..end];
    let board_name = sqlx::query_scalar::<_, String>(
        "SELECT name FROM boards WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(&input.board_id)
    .fetch_optional(&app.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Board not found.".into()))?;
    Ok(json!({
        "boardId": snapshot.board_id,
        "name": board_name,
        "revision": snapshot.revision,
        "totalElements": snapshot.elements.len(),
        "offset": offset,
        "elements": elements,
        "nextOffset": (end < snapshot.elements.len()).then_some(end),
    }))
}

async fn draw_shapes(
    app: &AppState,
    principal: &McpPrincipal,
    input: DrawShapesInput,
) -> AppResult<Value> {
    require_mcp_write_access(app, principal, &input.board_id).await?;
    if input.shapes.is_empty() || input.shapes.len() > MAX_DRAW_ELEMENTS {
        return Err(AppError::BadRequest(format!(
            "Provide between 1 and {MAX_DRAW_ELEMENTS} shapes."
        )));
    }
    let existing = collaboration::load_snapshot(app, &input.board_id).await?;
    let mut z_index = next_z_index(&existing.elements);
    let mut elements = Vec::with_capacity(input.shapes.len());
    for (index, shape) in input.shapes.into_iter().enumerate() {
        validate_geometry(shape.x, shape.y, shape.width, shape.height)?;
        if shape.width <= 0.0 || shape.height <= 0.0 {
            return Err(AppError::BadRequest(
                "Shape width and height must be positive.".into(),
            ));
        }
        let element_type = match shape.shape {
            ShapeKind::Rectangle => "rectangle",
            ShapeKind::Ellipse => "ellipse",
            ShapeKind::Diamond => "diamond",
        };
        let mut element = base_element(
            element_type,
            shape.x,
            shape.y,
            shape.width,
            shape.height,
            z_index,
        );
        assign_mcp_identity(&mut element, &input.client_operation_id, index);
        if let Some(z_index) = preserved_z_index(&existing.elements, &element) {
            element.insert("zIndex".into(), json!(z_index));
        }
        apply_style(&mut element, shape.style);
        elements.push(Value::Object(element));
        z_index += 1;
    }
    apply_mcp_batch(
        app,
        principal,
        &input.board_id,
        "draw_shapes",
        &input.client_operation_id,
        input.base_revision,
        elements,
        Vec::new(),
    )
    .await
}

async fn draw_freehand(
    app: &AppState,
    principal: &McpPrincipal,
    input: DrawFreehandInput,
) -> AppResult<Value> {
    require_mcp_write_access(app, principal, &input.board_id).await?;
    if input.points.len() < 2 || input.points.len() > 500 {
        return Err(AppError::BadRequest(
            "A freehand stroke requires between 2 and 500 points.".into(),
        ));
    }
    for point in &input.points {
        validate_point(point)?;
    }
    let min_x = input
        .points
        .iter()
        .map(|point| point.x)
        .fold(f64::INFINITY, f64::min);
    let min_y = input
        .points
        .iter()
        .map(|point| point.y)
        .fold(f64::INFINITY, f64::min);
    let max_x = input
        .points
        .iter()
        .map(|point| point.x)
        .fold(f64::NEG_INFINITY, f64::max);
    let max_y = input
        .points
        .iter()
        .map(|point| point.y)
        .fold(f64::NEG_INFINITY, f64::max);
    let points: Vec<Value> = input
        .points
        .iter()
        .map(|point| json!({"x": point.x - min_x, "y": point.y - min_y}))
        .collect();
    let width = (max_x - min_x).max(1.0);
    let height = (max_y - min_y).max(1.0);
    validate_geometry(min_x, min_y, width, height)?;
    let snapshot = collaboration::load_snapshot(app, &input.board_id).await?;
    let mut element = base_element(
        "freehand",
        min_x,
        min_y,
        width,
        height,
        next_z_index(&snapshot.elements),
    );
    assign_mcp_identity(&mut element, &input.client_operation_id, 0);
    if let Some(z_index) = preserved_z_index(&snapshot.elements, &element) {
        element.insert("zIndex".into(), json!(z_index));
    }
    element.insert("points".into(), Value::Array(points));
    apply_style(&mut element, input.style);
    apply_mcp_batch(
        app,
        principal,
        &input.board_id,
        "draw_freehand",
        &input.client_operation_id,
        input.base_revision,
        vec![Value::Object(element)],
        Vec::new(),
    )
    .await
}

async fn draw_connector(
    app: &AppState,
    principal: &McpPrincipal,
    input: DrawConnectorInput,
) -> AppResult<Value> {
    require_mcp_write_access(app, principal, &input.board_id).await?;
    validate_point(&input.start)?;
    validate_point(&input.end)?;
    let snapshot = collaboration::load_snapshot(app, &input.board_id).await?;
    let kind = match input.kind {
        ConnectorKind::Line => "line",
        ConnectorKind::Arrow => "arrow",
    };
    let x = input.start.x.min(input.end.x);
    let y = input.start.y.min(input.end.y);
    let width = (input.end.x - input.start.x).abs().max(1.0);
    let height = (input.end.y - input.start.y).abs().max(1.0);
    validate_geometry(x, y, width, height)?;
    let mut element = base_element(kind, x, y, width, height, next_z_index(&snapshot.elements));
    assign_mcp_identity(&mut element, &input.client_operation_id, 0);
    if let Some(z_index) = preserved_z_index(&snapshot.elements, &element) {
        element.insert("zIndex".into(), json!(z_index));
    }
    let start_point = json!({"x": input.start.x - x, "y": input.start.y - y});
    let end_point = json!({"x": input.end.x - x, "y": input.end.y - y});
    element.insert("points".into(), json!([start_point, end_point]));
    if kind == "arrow" {
        element.insert("startArrowhead".into(), json!("none"));
        element.insert("endArrowhead".into(), json!("arrow"));
        element.insert("startBinding".into(), Value::Null);
        element.insert("endBinding".into(), Value::Null);
        element.insert("arrowType".into(), json!("elbow"));
        element.insert("routing".into(), json!("elbow"));
    }
    apply_style(&mut element, input.style);
    apply_mcp_batch(
        app,
        principal,
        &input.board_id,
        "draw_connector",
        &input.client_operation_id,
        input.base_revision,
        vec![Value::Object(element)],
        Vec::new(),
    )
    .await
}

async fn add_text(
    app: &AppState,
    principal: &McpPrincipal,
    input: AddTextInput,
) -> AppResult<Value> {
    require_mcp_write_access(app, principal, &input.board_id).await?;
    validate_coordinate(input.x)?;
    validate_coordinate(input.y)?;
    if input.text.trim().is_empty() || input.text.chars().count() > 20_000 {
        return Err(AppError::BadRequest(
            "Text must contain 1 to 20000 characters.".into(),
        ));
    }
    let width = input.width.unwrap_or(160.0);
    if !width.is_finite() || !(24.0..=100_000.0).contains(&width) {
        return Err(AppError::BadRequest(
            "Text width must be between 24 and 100000.".into(),
        ));
    }
    let snapshot = collaboration::load_snapshot(app, &input.board_id).await?;
    let mut element = base_element(
        "text",
        input.x,
        input.y,
        width,
        26.0,
        next_z_index(&snapshot.elements),
    );
    assign_mcp_identity(&mut element, &input.client_operation_id, 0);
    if let Some(z_index) = preserved_z_index(&snapshot.elements, &element) {
        element.insert("zIndex".into(), json!(z_index));
    }
    element.insert("text".into(), json!(input.text));
    element.insert("fontSize".into(), json!(20));
    element.insert("fontFamily".into(), json!("sans"));
    element.insert("fontWeight".into(), json!("normal"));
    element.insert("fontStyle".into(), json!("normal"));
    element.insert("textAlign".into(), json!("left"));
    element.insert("verticalAlign".into(), json!("top"));
    element.insert("autoResize".into(), json!(false));
    apply_style(&mut element, input.style);
    apply_mcp_batch(
        app,
        principal,
        &input.board_id,
        "add_text",
        &input.client_operation_id,
        input.base_revision,
        vec![Value::Object(element)],
        Vec::new(),
    )
    .await
}

async fn update_elements(
    app: &AppState,
    principal: &McpPrincipal,
    input: UpdateElementsInput,
) -> AppResult<Value> {
    require_mcp_write_access(app, principal, &input.board_id).await?;
    if input.updates.is_empty() || input.updates.len() > MAX_DRAW_ELEMENTS {
        return Err(AppError::BadRequest(format!(
            "Provide between 1 and {MAX_DRAW_ELEMENTS} element updates."
        )));
    }
    let snapshot = collaboration::load_snapshot(app, &input.board_id).await?;
    let mut by_id: std::collections::HashMap<&str, &Value> = snapshot
        .elements
        .iter()
        .filter_map(|element| Some((element.get("id")?.as_str()?, element)))
        .collect();
    let mut updates = Vec::with_capacity(input.updates.len());
    let mut seen = std::collections::HashSet::new();
    for update in input.updates {
        if !seen.insert(update.id.clone()) {
            return Err(AppError::BadRequest("Duplicate element ID.".into()));
        }
        let original = by_id
            .remove(update.id.as_str())
            .ok_or_else(|| AppError::NotFound("Element not found on this board.".into()))?;
        if original.get("locked").and_then(Value::as_bool) == Some(true) {
            return Err(AppError::Forbidden(
                "Locked elements cannot be changed by an agent.".into(),
            ));
        }
        if let Some(x) = update.x {
            validate_coordinate(x)?;
        }
        if let Some(y) = update.y {
            validate_coordinate(y)?;
        }
        for dimension in [update.width, update.height].into_iter().flatten() {
            if !dimension.is_finite() || !(1.0..=MAX_COORDINATE).contains(&dimension) {
                return Err(AppError::BadRequest(
                    "Element dimensions must be between 1 and 1000000.".into(),
                ));
            }
        }
        if let Some(rotation) = update.rotation {
            validate_coordinate(rotation)?;
        }
        let mut element = original
            .as_object()
            .cloned()
            .ok_or_else(|| AppError::BadRequest("Stored element is invalid.".into()))?;
        if update.x.is_none()
            && update.y.is_none()
            && update.width.is_none()
            && update.height.is_none()
            && update.rotation.is_none()
            && update.stroke_color.is_none()
            && update.background_color.is_none()
            && update.opacity.is_none()
            && update.text.is_none()
        {
            return Err(AppError::BadRequest(
                "Each update must change at least one supported field.".into(),
            ));
        }
        set_optional(&mut element, "x", update.x);
        set_optional(&mut element, "y", update.y);
        set_optional(&mut element, "width", update.width);
        set_optional(&mut element, "height", update.height);
        set_optional(&mut element, "rotation", update.rotation);
        set_optional_string(&mut element, "strokeColor", update.stroke_color);
        set_optional_string(&mut element, "backgroundColor", update.background_color);
        set_optional(&mut element, "opacity", update.opacity);
        if let Some(text) = update.text {
            if !matches!(
                element.get("type").and_then(Value::as_str),
                Some("text" | "note")
            ) {
                return Err(AppError::BadRequest(
                    "Only text and note elements can have their text changed.".into(),
                ));
            }
            if text.chars().count() > 20_000 {
                return Err(AppError::BadRequest(
                    "Text must contain at most 20000 characters.".into(),
                ));
            }
            element.insert("text".into(), json!(text));
        }
        updates.push(Value::Object(element));
    }
    apply_mcp_batch(
        app,
        principal,
        &input.board_id,
        "update_elements",
        &input.client_operation_id,
        input.base_revision,
        updates,
        Vec::new(),
    )
    .await
}

async fn delete_elements(
    app: &AppState,
    principal: &McpPrincipal,
    input: DeleteElementsInput,
) -> AppResult<Value> {
    require_mcp_write_access(app, principal, &input.board_id).await?;
    if input.element_ids.is_empty() || input.element_ids.len() > MAX_DRAW_ELEMENTS {
        return Err(AppError::BadRequest(format!(
            "Provide between 1 and {MAX_DRAW_ELEMENTS} element IDs."
        )));
    }
    let snapshot = collaboration::load_snapshot(app, &input.board_id).await?;
    let existing: std::collections::HashMap<&str, &Value> = snapshot
        .elements
        .iter()
        .filter_map(|element| Some((element.get("id")?.as_str()?, element)))
        .collect();
    let mut seen = std::collections::HashSet::new();
    for id in &input.element_ids {
        if !seen.insert(id) {
            return Err(AppError::BadRequest("Duplicate element ID.".into()));
        }
        let element = existing
            .get(id.as_str())
            .ok_or_else(|| AppError::NotFound("Element not found on this board.".into()))?;
        if element.get("locked").and_then(Value::as_bool) == Some(true) {
            return Err(AppError::Forbidden(
                "Locked elements cannot be deleted by an agent.".into(),
            ));
        }
    }
    apply_mcp_batch(
        app,
        principal,
        &input.board_id,
        "delete_elements",
        &input.client_operation_id,
        input.base_revision,
        Vec::new(),
        input.element_ids,
    )
    .await
}

async fn apply_mcp_batch(
    app: &AppState,
    principal: &McpPrincipal,
    board_id: &str,
    tool_name: &'static str,
    client_operation_id: &str,
    base_revision: i64,
    upserts: Vec<Value>,
    deleted_ids: Vec<String>,
) -> AppResult<Value> {
    if !principal.has_scope(MCP_WRITE_SCOPE) {
        return Err(AppError::Forbidden(
            "This authorization does not include boards:write.".into(),
        ));
    }
    authz::require_board_role(&app.db, board_id, &principal.user_id, BoardRole::Editor).await?;
    store::validate_client_operation_id(client_operation_id)?;
    let svg_limit = if schema::contains_svg(&upserts) {
        schema::svg_element_limit(store::user_is_pro(&app.db, &principal.user_id).await?)
    } else {
        schema::SVG_ELEMENT_PRO_MAX
    };
    let upserts = schema::elements_with_svg_limit(&upserts, "Invalid canvas data", svg_limit)?;
    let batch = collaboration::validate_operation(
        client_operation_id,
        ElementBatch {
            upserts,
            deleted_ids,
        },
    )?;
    let operation = store::apply_patch(
        &app.db,
        board_id,
        &principal.user_id,
        client_operation_id,
        Some(base_revision),
        batch,
    )
    .await?;
    collaboration::broadcast_editor_operation(&app.collaboration, board_id, &operation, false)
        .await;
    if !operation.replayed {
        store::maybe_create_auto_version(&app.db, board_id, &principal.user_id).await;
    }
    tracing::info!(
        subsystem = "mcp",
        event = "board_operation_applied",
        user_id = %principal.user_id,
        client_id = %principal.client_id,
        grant_id = %principal.grant_id,
        board_id,
        tool = tool_name,
        operation_id = %operation.client_operation_id,
        revision = operation.revision,
        replayed = operation.replayed,
        "MCP board operation applied"
    );
    Ok(json!({
        "boardId": board_id,
        "operationId": operation.client_operation_id,
        "revision": operation.revision,
        "upsertedIds": operation.batch.upserts.iter().filter_map(|element| {
            element.get("id").and_then(Value::as_str)
        }).collect::<Vec<_>>(),
        "deletedIds": operation.batch.deleted_ids,
        "replayed": operation.replayed,
    }))
}

async fn require_mcp_write_access(
    app: &AppState,
    principal: &McpPrincipal,
    board_id: &str,
) -> AppResult<()> {
    if !principal.has_scope(MCP_WRITE_SCOPE) {
        return Err(AppError::Forbidden(
            "This authorization does not include boards:write.".into(),
        ));
    }
    authz::require_board_role(&app.db, board_id, &principal.user_id, BoardRole::Editor).await?;
    Ok(())
}

fn base_element(
    element_type: &str,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    z_index: i64,
) -> Map<String, Value> {
    let element = json!({
        "id": cuid2::create_id(),
        "type": element_type,
        "x": x,
        "y": y,
        "width": width,
        "height": height,
        "rotation": 0,
        "strokeColor": "#1c1917",
        "backgroundColor": "transparent",
        "strokeWidth": 2,
        "opacity": 1,
        "fillStyle": "solid",
        "strokeStyle": "solid",
        "roundness": "soft",
        "roughness": 0,
        "seed": rand::random::<u32>() as i64,
        "groupId": null,
        "frameId": null,
        "zIndex": z_index,
        "locked": false,
    })
    .as_object()
    .expect("base element is a JSON object")
    .clone();
    element
}

fn assign_mcp_identity(element: &mut Map<String, Value>, client_operation_id: &str, index: usize) {
    let mut hasher = Sha256::new();
    hasher.update(client_operation_id.as_bytes());
    hasher.update((index as u64).to_be_bytes());
    let digest = hasher.finalize();
    element.insert(
        "id".into(),
        json!(format!("mcp_{}", hex::encode(&digest[..16]))),
    );
    element.insert(
        "seed".into(),
        json!(u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]]) as i64),
    );
}

fn apply_style(element: &mut Map<String, Value>, style: Option<ElementStyle>) {
    let Some(style) = style else {
        return;
    };
    set_optional_string(element, "strokeColor", style.stroke_color);
    set_optional_string(element, "backgroundColor", style.background_color);
    set_optional(element, "strokeWidth", style.stroke_width);
    set_optional(element, "opacity", style.opacity);
    set_optional_string(element, "fillStyle", style.fill_style);
    set_optional_string(element, "strokeStyle", style.stroke_style);
    set_optional_string(element, "roundness", style.roundness);
    set_optional(element, "roughness", style.roughness);
}

fn set_optional(element: &mut Map<String, Value>, key: &str, value: Option<f64>) {
    if let Some(value) = value {
        element.insert(key.into(), json!(value));
    }
}

fn set_optional_string(element: &mut Map<String, Value>, key: &str, value: Option<String>) {
    if let Some(value) = value {
        element.insert(key.into(), json!(value));
    }
}

fn next_z_index(elements: &[Value]) -> i64 {
    elements
        .iter()
        .filter_map(|element| element.get("zIndex").and_then(Value::as_i64))
        .max()
        .unwrap_or(0)
        .saturating_add(1)
}

fn preserved_z_index(elements: &[Value], new_element: &Map<String, Value>) -> Option<i64> {
    let id = new_element.get("id")?.as_str()?;
    elements
        .iter()
        .find(|element| element.get("id").and_then(Value::as_str) == Some(id))
        .and_then(|element| element.get("zIndex").and_then(Value::as_i64))
}

fn audit_read(principal: &McpPrincipal, tool: &str, board_id: Option<&str>) {
    tracing::info!(
        subsystem = "mcp",
        event = "board_read",
        user_id = %principal.user_id,
        client_id = %principal.client_id,
        grant_id = %principal.grant_id,
        tool,
        board_id,
        "MCP board read"
    );
}

fn validate_coordinate(value: f64) -> AppResult<()> {
    if !value.is_finite() || value.abs() > MAX_COORDINATE {
        return Err(AppError::BadRequest(
            "Canvas coordinates must be finite and within +/-1000000.".into(),
        ));
    }
    Ok(())
}

fn validate_geometry(x: f64, y: f64, width: f64, height: f64) -> AppResult<()> {
    validate_coordinate(x)?;
    validate_coordinate(y)?;
    if !width.is_finite()
        || !height.is_finite()
        || width.abs() > MAX_COORDINATE
        || height.abs() > MAX_COORDINATE
    {
        return Err(AppError::BadRequest(
            "Shape dimensions must be finite and within +/-1000000.".into(),
        ));
    }
    Ok(())
}

fn validate_point(point: &PointInput) -> AppResult<()> {
    validate_coordinate(point.x)?;
    validate_coordinate(point.y)
}

fn role_name(role: BoardRole) -> String {
    match role {
        BoardRole::Viewer => "viewer",
        BoardRole::Editor => "editor",
        BoardRole::Owner => "owner",
    }
    .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_shape_has_default_style_and_identity_fields() {
        let element = base_element("rectangle", 10.0, 20.0, 80.0, 40.0, 2);
        assert_eq!(
            element.get("type").and_then(Value::as_str),
            Some("rectangle")
        );
        assert_eq!(
            element.get("strokeColor").and_then(Value::as_str),
            Some("#1c1917")
        );
        assert_eq!(element.get("locked").and_then(Value::as_bool), Some(false));
        assert_eq!(element.get("zIndex").and_then(Value::as_i64), Some(2));
    }

    #[test]
    fn generated_shapes_pass_the_shared_canvas_schema() {
        let elements: Vec<Value> = ["rectangle", "ellipse", "diamond"]
            .into_iter()
            .map(|kind| Value::Object(base_element(kind, 10.0, 20.0, 80.0, 40.0, 1)))
            .collect();
        assert!(schema::elements_with_svg_limit(
            &elements,
            "Invalid canvas data",
            schema::SVG_ELEMENT_PRO_MAX
        )
        .is_ok());
    }

    #[test]
    fn generated_strokes_connectors_and_text_pass_the_shared_schema() {
        let mut line = base_element("line", 10.0, 20.0, 80.0, 40.0, 1);
        line.insert(
            "points".into(),
            json!([{"x": 0.0, "y": 0.0}, {"x": 80.0, "y": 40.0}]),
        );
        let mut arrow = base_element("arrow", 10.0, 20.0, 80.0, 40.0, 2);
        arrow.insert(
            "points".into(),
            json!([{"x": 0.0, "y": 0.0}, {"x": 80.0, "y": 40.0}]),
        );
        arrow.insert("startArrowhead".into(), json!("none"));
        arrow.insert("endArrowhead".into(), json!("arrow"));
        arrow.insert("startBinding".into(), Value::Null);
        arrow.insert("endBinding".into(), Value::Null);
        arrow.insert("arrowType".into(), json!("elbow"));
        arrow.insert("routing".into(), json!("elbow"));
        let mut freehand = base_element("freehand", 10.0, 20.0, 80.0, 40.0, 3);
        freehand.insert(
            "points".into(),
            json!([{"x": 0.0, "y": 0.0}, {"x": 80.0, "y": 40.0}]),
        );
        let mut text = base_element("text", 10.0, 20.0, 160.0, 26.0, 4);
        text.extend(
            json!({
                "text": "Agent text",
                "fontSize": 20,
                "fontFamily": "sans",
                "fontWeight": "normal",
                "fontStyle": "normal",
                "textAlign": "left",
                "verticalAlign": "top",
                "autoResize": false,
            })
            .as_object()
            .expect("test text fields are an object")
            .clone(),
        );
        let elements: Vec<Value> = [line, arrow, freehand, text]
            .into_iter()
            .map(Value::Object)
            .collect();
        assert!(schema::elements_with_svg_limit(
            &elements,
            "Invalid canvas data",
            schema::SVG_ELEMENT_PRO_MAX
        )
        .is_ok());
    }

    #[test]
    fn update_tool_rejects_fields_outside_its_allowlist() {
        let update = serde_json::from_value::<UpdateElementInput>(json!({
            "id": "element-1",
            "locked": false,
        }));
        assert!(update.is_err());
    }

    #[test]
    fn generated_element_identity_is_stable_for_operation_retries() {
        let mut first = base_element("rectangle", 10.0, 20.0, 80.0, 40.0, 1);
        let mut retry = base_element("rectangle", 10.0, 20.0, 80.0, 40.0, 1);
        assign_mcp_identity(&mut first, "operation-1", 0);
        assign_mcp_identity(&mut retry, "operation-1", 0);
        assert_eq!(first["id"], retry["id"]);
        assert_eq!(first["seed"], retry["seed"]);
        assign_mcp_identity(&mut retry, "operation-2", 0);
        assert_ne!(first["id"], retry["id"]);
    }

    #[test]
    fn z_index_is_preserved_when_a_create_operation_is_retried() {
        let mut element = base_element("rectangle", 10.0, 20.0, 80.0, 40.0, 2);
        assign_mcp_identity(&mut element, "operation-1", 0);
        let persisted = Value::Object(element.clone());
        assert_eq!(preserved_z_index(&[persisted], &element), Some(2));
        assert_eq!(preserved_z_index(&[], &element), None);
    }

    #[test]
    fn coordinate_validation_rejects_nonfinite_and_extreme_positions() {
        assert!(validate_coordinate(0.0).is_ok());
        assert!(validate_coordinate(f64::NAN).is_err());
        assert!(validate_coordinate(MAX_COORDINATE + 1.0).is_err());
    }

    #[test]
    fn principal_scopes_are_checked_exactly() {
        let principal = McpPrincipal {
            user_id: "user-1".into(),
            client_id: "client-1".into(),
            grant_id: "grant-1".into(),
            scopes: vec![MCP_READ_SCOPE.into()],
        };
        assert!(principal.has_scope(MCP_READ_SCOPE));
        assert!(!principal.has_scope(MCP_WRITE_SCOPE));
    }

    #[test]
    fn tool_operation_ids_use_the_revisioned_writer_constraints() {
        assert!(store::validate_client_operation_id("agent-op_123").is_ok());
        assert!(store::validate_client_operation_id("bad id").is_err());
    }
}
