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
use sha2::{Digest, Sha256};
use sqlx::{FromRow, PgPool, Postgres, Transaction};
use std::collections::{HashMap, HashSet};
use tower_cookies::Cookies;
use utoipa::ToSchema;

use crate::{
    auth::session,
    authz::{self, BoardRole},
    billing::{self, get_billing_state, BillingStatus},
    editor::schema,
    error::{AppError, AppResult},
    extract::AppJson,
    state::AppState,
};

const MAX_LIBRARY_ITEM_ELEMENTS: usize = 5_000;
const MAX_LIBRARY_PAGE_SIZE: i64 = 100;
const LIBRARY_WRITE_LIMIT_PER_MINUTE: i64 = 60;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/library/items", get(list_items).post(create_item))
        .route(
            "/api/library/items/{id}",
            axum::routing::patch(rename_item).delete(delete_item),
        )
        .route("/api/library/reorder", post(reorder_personal_items))
        .route("/api/library/items/{id}/copy", post(copy_item))
        .route("/api/library/items/{id}/move", post(move_item))
        .route(
            "/api/collections/{collection_id}/library",
            get(list_collection_items),
        )
        .route(
            "/api/collections/{collection_id}/library/items",
            post(create_collection_item),
        )
        .route(
            "/api/collections/{collection_id}/library/items/{id}",
            axum::routing::patch(rename_collection_item).delete(delete_collection_item),
        )
        .route(
            "/api/collections/{collection_id}/library/reorder",
            post(reorder_collection_items),
        )
        .route("/api/boards/{id}/libraries", get(board_libraries))
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateItemInput {
    name: String,
    source_board_id: String,
    elements: Vec<Value>,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RenameItemInput {
    name: String,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LibraryQuery {
    limit: Option<i64>,
    cursor: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReorderInput {
    item_ids: Option<Vec<String>>,
    item_id: Option<String>,
    position: Option<i64>,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct TransferInput {
    target: TransferTarget,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(untagged)]
pub enum TransferTarget {
    Personal(String),
    Collection(TransferCollection),
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TransferCollection {
    #[serde(alias = "collection_id")]
    collection_id: String,
}

#[derive(Debug, Serialize, FromRow, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct LibraryItem {
    id: String,
    library_id: String,
    name: String,
    elements: Value,
    element_count: i32,
    position: i64,
    content_hash: String,
    scope: String,
    collection_id: Option<String>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct LibraryItems {
    items: Vec<LibraryItem>,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CollectionLibraryPage {
    library: Option<LibraryDescriptor>,
    items: Vec<LibraryItem>,
    next_cursor: Option<String>,
    etag: Option<String>,
    readonly_reason: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BoardLibrariesResponse {
    libraries: Vec<LibraryDescriptor>,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct LibraryDescriptor {
    id: Option<String>,
    scope: String,
    collection_id: Option<String>,
    collection_name: Option<String>,
    etag: Option<String>,
    updated_at: Option<DateTime<Utc>>,
    readonly_reason: Option<String>,
    locked: bool,
}

#[derive(Debug, FromRow)]
struct LibraryRecord {
    id: String,
    updated_at: DateTime<Utc>,
}

#[derive(Debug, FromRow)]
struct LibraryItemRecord {
    id: String,
    library_id: String,
    name: String,
    elements: Value,
    element_count: i32,
    position: i64,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    scope: String,
    collection_id: Option<String>,
}

#[derive(Debug, FromRow)]
struct CollectionRecord {
    id: String,
    name: String,
}

#[derive(Debug, FromRow)]
struct ImageAsset {
    id: String,
    byte_size: i32,
}

#[derive(Debug, FromRow)]
struct LegacyLibraryItem {
    id: String,
    elements: Value,
}

#[derive(Debug, FromRow)]
struct DuplicateCandidate {
    id: String,
    library_id: String,
    name: String,
    content_hash: String,
    scope: String,
    collection_id: Option<String>,
    collection_name: Option<String>,
}

fn item_from_record(record: LibraryItemRecord) -> AppResult<LibraryItem> {
    let content_hash = content_hash(&record.elements)?;
    Ok(LibraryItem {
        id: record.id,
        library_id: record.library_id,
        name: record.name,
        elements: record.elements,
        element_count: record.element_count,
        position: record.position,
        content_hash,
        scope: record.scope,
        collection_id: record.collection_id,
        created_at: record.created_at,
        updated_at: record.updated_at,
    })
}

fn content_hash(elements: &Value) -> AppResult<String> {
    let canonical = canonicalize_elements(elements)?;
    let bytes = serde_json::to_vec(&canonical)
        .map_err(|error| AppError::Internal(anyhow::anyhow!(error)))?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

fn canonicalize_elements(elements: &Value) -> AppResult<Value> {
    let Some(elements) = elements.as_array() else {
        return Err(AppError::Internal(anyhow::anyhow!(
            "library item elements must be an array"
        )));
    };

    let mut element_ids = HashMap::new();
    for (index, element) in elements.iter().enumerate() {
        if let Some(id) = element.get("id").and_then(Value::as_str) {
            element_ids.insert(id.to_string(), format!("element:{index}"));
        }
    }

    let mut groups = HashMap::new();
    let mut frames = HashMap::new();
    let mut external_refs = HashMap::new();
    let mut canonical = Value::Array(elements.clone());
    for element in canonical.as_array_mut().unwrap().iter_mut() {
        if let Some(object) = element.as_object_mut() {
            if let Some(id) = object.get("id").and_then(Value::as_str) {
                object.insert("id".to_string(), Value::String(element_ids[id].clone()));
            }
            if let Some(Value::String(group_id)) = object.get("groupId") {
                let canonical_id = identity_token(&mut groups, group_id, "group");
                object.insert("groupId".to_string(), Value::String(canonical_id));
            }
            if let Some(Value::String(frame_id)) = object.get("frameId") {
                let canonical_id = element_ids
                    .get(frame_id)
                    .cloned()
                    .unwrap_or_else(|| identity_token(&mut frames, frame_id, "frame"));
                object.insert("frameId".to_string(), Value::String(canonical_id));
            }
            if let Some(Value::String(container_id)) = object.get("containerId") {
                let canonical_id = element_ids
                    .get(container_id)
                    .cloned()
                    .unwrap_or_else(|| identity_token(&mut external_refs, container_id, "ref"));
                object.insert("containerId".to_string(), Value::String(canonical_id));
            }
            for binding_key in ["startBinding", "endBinding"] {
                if let Some(binding) = object.get_mut(binding_key).and_then(Value::as_object_mut) {
                    if let Some(Value::String(target_id)) = binding.get("elementId") {
                        let canonical_id =
                            element_ids.get(target_id).cloned().unwrap_or_else(|| {
                                identity_token(&mut external_refs, target_id, "ref")
                            });
                        binding.insert("elementId".to_string(), Value::String(canonical_id));
                    }
                }
            }
        }
    }
    Ok(canonical)
}

fn identity_token(identities: &mut HashMap<String, String>, id: &str, prefix: &str) -> String {
    if let Some(token) = identities.get(id) {
        return token.clone();
    }
    let token = format!("{prefix}:{}", identities.len());
    identities.insert(id.to_string(), token.clone());
    token
}

async fn find_duplicate_owned_item(
    tx: &mut Transaction<'_, Postgres>,
    user_id: &str,
    hash: &str,
) -> AppResult<Option<DuplicateCandidate>> {
    let legacy_items = sqlx::query_as::<_, LegacyLibraryItem>(
        r#"
        SELECT item.id, item.elements
        FROM library_items item
        JOIN libraries library ON library.id=item.library_id
        LEFT JOIN private_collections collection ON collection.id=library.collection_id
        WHERE library.owner_id=$1
          AND item.content_hash_version=0
          AND (library.scope='user' OR collection.deleted_at IS NULL)
        FOR UPDATE OF item
        "#,
    )
    .bind(user_id)
    .fetch_all(&mut **tx)
    .await?;
    for item in legacy_items {
        let legacy_hash = content_hash(&item.elements)?;
        sqlx::query(
            "UPDATE library_items
             SET content_hash=$2, content_hash_version=1
             WHERE id=$1",
        )
        .bind(item.id)
        .bind(legacy_hash)
        .execute(&mut **tx)
        .await?;
    }

    Ok(sqlx::query_as::<_, DuplicateCandidate>(
        r#"
        SELECT item.id, item.library_id, item.name, item.content_hash,
               library.scope, library.collection_id, collection.name AS collection_name
        FROM library_items item
        JOIN libraries library ON library.id=item.library_id
        LEFT JOIN private_collections collection ON collection.id=library.collection_id
        WHERE library.owner_id=$1
          AND item.content_hash_version=1
          AND item.content_hash=$2
          AND (library.scope='user' OR collection.deleted_at IS NULL)
        ORDER BY (library.scope='user') DESC, item.created_at, item.id
        LIMIT 1
        "#,
    )
    .bind(user_id)
    .bind(hash)
    .fetch_optional(&mut **tx)
    .await?)
}

fn readonly_reason(status: BillingStatus) -> Option<&'static str> {
    match status {
        BillingStatus::Active => None,
        BillingStatus::Expired => Some("plan_lapsed"),
        BillingStatus::Free => Some("upgrade_required"),
    }
}

async fn require_owned_collection(
    db: &PgPool,
    collection_id: &str,
    user_id: &str,
) -> AppResult<CollectionRecord> {
    sqlx::query_as::<_, CollectionRecord>(
        "SELECT id, name FROM private_collections
         WHERE id=$1 AND owner_id=$2 AND deleted_at IS NULL",
    )
    .bind(collection_id)
    .bind(user_id)
    .fetch_optional(db)
    .await?
    .ok_or_else(|| AppError::NotFound("Collection not found.".into()))
}

async fn collection_read_access(
    db: &PgPool,
    collection_id: &str,
    user_id: &str,
) -> AppResult<(CollectionRecord, BillingStatus)> {
    let collection = require_owned_collection(db, collection_id, user_id).await?;
    let status = get_billing_state(db, user_id).await?.status;
    if status == BillingStatus::Free {
        return Err(AppError::Forbidden(
            "Collection libraries require a paid plan.".into(),
        ));
    }
    Ok((collection, status))
}

async fn collection_write_access(
    db: &PgPool,
    collection_id: &str,
    user_id: &str,
) -> AppResult<CollectionRecord> {
    let collection = require_owned_collection(db, collection_id, user_id).await?;
    if get_billing_state(db, user_id).await?.status != BillingStatus::Active {
        return Err(AppError::Forbidden(
            "An active paid plan is required to edit collection libraries.".into(),
        ));
    }
    Ok(collection)
}

async fn ensure_personal_library(
    tx: &mut Transaction<'_, Postgres>,
    owner_id: &str,
) -> AppResult<(LibraryRecord, bool)> {
    let library_id = nanoid::nanoid!(16);
    let inserted = sqlx::query(
        "INSERT INTO libraries(id, owner_id, scope)
         VALUES($1, $2, 'user') ON CONFLICT DO NOTHING RETURNING id",
    )
    .bind(library_id)
    .bind(owner_id)
    .execute(&mut **tx)
    .await?
    .rows_affected()
        > 0;
    let library = sqlx::query_as::<_, LibraryRecord>(
        "SELECT id, updated_at
         FROM libraries WHERE owner_id=$1 AND scope='user'",
    )
    .bind(owner_id)
    .fetch_one(&mut **tx)
    .await?;
    Ok((library, inserted))
}

async fn ensure_collection_library(
    tx: &mut Transaction<'_, Postgres>,
    collection_id: &str,
    owner_id: &str,
) -> AppResult<(LibraryRecord, bool)> {
    sqlx::query_scalar::<_, String>(
        "SELECT id FROM private_collections
         WHERE id=$1 AND owner_id=$2 AND deleted_at IS NULL FOR SHARE",
    )
    .bind(collection_id)
    .bind(owner_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| AppError::NotFound("Collection not found.".into()))?;
    let library_id = nanoid::nanoid!(16);
    let inserted = sqlx::query(
        "INSERT INTO libraries(id, owner_id, scope, collection_id)
         VALUES($1, $2, 'collection', $3) ON CONFLICT DO NOTHING RETURNING id",
    )
    .bind(library_id)
    .bind(owner_id)
    .bind(collection_id)
    .fetch_optional(&mut **tx)
    .await?
    .is_some();
    let library = sqlx::query_as::<_, LibraryRecord>(
        "SELECT id, updated_at
         FROM libraries WHERE collection_id=$1 AND scope='collection'",
    )
    .bind(collection_id)
    .fetch_one(&mut **tx)
    .await?;
    Ok((library, inserted))
}

async fn ensure_library(
    tx: &mut Transaction<'_, Postgres>,
    owner_id: &str,
    collection_id: Option<&str>,
) -> AppResult<(LibraryRecord, bool)> {
    match collection_id {
        Some(collection_id) => ensure_collection_library(tx, collection_id, owner_id).await,
        None => ensure_personal_library(tx, owner_id).await,
    }
}

async fn log_audit(
    tx: &mut Transaction<'_, Postgres>,
    actor_id: &str,
    event: &str,
    library_id: &str,
    target_library_id: Option<&str>,
    item_count: i64,
) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO library_audit_events
         (actor_id, event, library_id, target_library_id, item_count)
         VALUES($1, $2, $3, $4, $5)",
    )
    .bind(actor_id)
    .bind(event)
    .bind(library_id)
    .bind(target_library_id)
    .bind(item_count)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn rate_limit_write(db: &PgPool, actor_id: &str) -> AppResult<()> {
    let count: i32 = sqlx::query_scalar(
        r#"
        INSERT INTO library_write_rate_limits(actor_id, window_start, request_count)
        VALUES($1, now(), 1)
        ON CONFLICT(actor_id) DO UPDATE SET
            window_start = CASE
                WHEN library_write_rate_limits.window_start <= now() - interval '1 minute'
                    THEN now()
                ELSE library_write_rate_limits.window_start
            END,
            request_count = CASE
                WHEN library_write_rate_limits.window_start <= now() - interval '1 minute'
                    THEN 1
                ELSE library_write_rate_limits.request_count + 1
            END
        RETURNING request_count
        "#,
    )
    .bind(actor_id)
    .fetch_one(db)
    .await?;
    if i64::from(count) > LIBRARY_WRITE_LIMIT_PER_MINUTE {
        return Err(AppError::TooManyRequests(
            "Library write rate limit exceeded.".into(),
        ));
    }
    Ok(())
}

#[utoipa::path(
    get,
    path = "/api/library/items",
    tag = "library",
    responses(
        (status = 200, description = "The signed-in user's personal library items", body = LibraryItems),
        (status = 401, description = "Not signed in")
    )
)]
pub async fn list_items(
    State(state): State<AppState>,
    cookies: Cookies,
) -> AppResult<Json<LibraryItems>> {
    let user = session::require_current_user(&cookies, &state).await?;
    let records = sqlx::query_as::<_, LibraryItemRecord>(
        r#"
        SELECT item.id, item.library_id, item.name, item.elements, item.element_count,
               item.position, item.content_hash, item.created_at, item.updated_at,
               library.scope, library.collection_id
        FROM library_items AS item
        JOIN libraries AS library ON library.id = item.library_id
        WHERE library.owner_id = $1 AND library.scope='user'
        ORDER BY item.position, item.created_at DESC, item.id
        "#,
    )
    .bind(&user.id)
    .fetch_all(&state.db)
    .await?;
    let items = records
        .into_iter()
        .map(item_from_record)
        .collect::<AppResult<Vec<_>>>()?;
    Ok(Json(LibraryItems { items }))
}

#[utoipa::path(
    post,
    path = "/api/library/items",
    tag = "library",
    request_body = CreateItemInput,
    responses(
        (status = 201, description = "Library item created", body = LibraryItem),
        (status = 409, description = "Equivalent item exists in one of the user's libraries"),
        (status = 402, description = "Machine-readable per-item plan limit reached"),
        (status = 400, description = "Invalid item name, elements, or images"),
        (status = 401, description = "Not signed in"),
        (status = 403, description = "Source board is not editable"),
        (status = 404, description = "Source board not found")
    )
)]
pub async fn create_item(
    State(state): State<AppState>,
    cookies: Cookies,
    AppJson(input): AppJson<CreateItemInput>,
) -> AppResult<(StatusCode, Json<LibraryItem>)> {
    create_item_inner(state, cookies, input, None).await
}

#[utoipa::path(
    post,
    path = "/api/collections/{collection_id}/library/items",
    tag = "library",
    params(("collection_id" = String, Path, description = "Owned private collection ID")),
    request_body = CreateItemInput,
    responses(
        (status = 201, description = "Collection library item created", body = LibraryItem),
        (status = 409, description = "Equivalent item exists in one of the user's libraries"),
        (status = 402, description = "Machine-readable per-item plan limit reached"),
        (status = 401, description = "Not signed in"),
        (status = 403, description = "Active paid plan required"),
        (status = 404, description = "Collection or source board not found")
    )
)]
pub async fn create_collection_item(
    State(state): State<AppState>,
    cookies: Cookies,
    Path(collection_id): Path<String>,
    AppJson(input): AppJson<CreateItemInput>,
) -> AppResult<(StatusCode, Json<LibraryItem>)> {
    create_item_inner(state, cookies, input, Some(collection_id)).await
}

async fn create_item_inner(
    state: AppState,
    cookies: Cookies,
    input: CreateItemInput,
    collection_id: Option<String>,
) -> AppResult<(StatusCode, Json<LibraryItem>)> {
    let user = session::require_current_user(&cookies, &state).await?;
    if let Some(collection_id) = collection_id.as_deref() {
        collection_write_access(&state.db, collection_id, &user.id).await?;
    }
    let name = validate_name(&input.name)?;
    let source_board_id = input.source_board_id.trim();
    if source_board_id.is_empty() {
        return Err(AppError::BadRequest(
            "An editable source board is required.".to_string(),
        ));
    }
    authz::require_board_role(&state.db, source_board_id, &user.id, BoardRole::Editor).await?;
    if let Some(collection_id) = collection_id.as_deref() {
        let is_member: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM private_collection_boards
             WHERE owner_id=$1 AND collection_id=$2 AND board_id=$3)",
        )
        .bind(&user.id)
        .bind(collection_id)
        .bind(source_board_id)
        .fetch_one(&state.db)
        .await?;
        if !is_member {
            return Err(AppError::NotFound("Collection board not found.".into()));
        }
    }

    rate_limit_write(&state.db, &user.id).await?;
    let billing = get_billing_state(&state.db, &user.id).await?;
    let is_pro = billing.status == BillingStatus::Active;
    let elements = validate_elements(&input.elements, is_pro)?;
    let image_count = elements
        .iter()
        .filter(|element| element["type"] == "image")
        .count();
    let image_cap = billing::limits::image_count_cap(is_pro) as usize;
    if image_count > image_cap {
        return Err(AppError::LimitReached {
            code: "LIBRARY_LIMIT_REACHED",
            message: format!("Your plan allows up to {image_cap} images in a library item."),
            usage: image_count as i64,
            limit: image_cap as i64,
        });
    }
    let asset_ids = validate_image_assets(
        &state,
        source_board_id,
        &elements,
        billing::limits::image_byte_cap(is_pro),
    )
    .await?;
    let elements = json!(elements);
    let hash = content_hash(&elements)?;
    let item_id = nanoid::nanoid!(16);
    let mut tx = state.db.begin().await?;
    sqlx::query("SELECT id FROM users WHERE id=$1 FOR UPDATE")
        .bind(&user.id)
        .fetch_one(&mut *tx)
        .await?;
    if let Some(duplicate) = find_duplicate_owned_item(&mut tx, &user.id, &hash).await? {
        tx.commit().await?;
        return Err(AppError::LibraryItemDuplicate {
            existing_item: json!({
                "id": duplicate.id,
                "name": duplicate.name,
                "libraryId": duplicate.library_id.clone(),
                "contentHash": duplicate.content_hash
            }),
            source_metadata: json!({
                "scope": duplicate.scope,
                "libraryId": duplicate.library_id,
                "collectionId": duplicate.collection_id,
                "collectionName": duplicate.collection_name
            }),
        });
    }
    let (library, library_was_created) =
        ensure_library(&mut tx, &user.id, collection_id.as_deref()).await?;
    sqlx::query("SELECT id FROM libraries WHERE id=$1 FOR UPDATE")
        .bind(&library.id)
        .fetch_one(&mut *tx)
        .await?;
    sqlx::query("UPDATE library_items SET position=position+1 WHERE library_id=$1")
        .bind(&library.id)
        .execute(&mut *tx)
        .await?;
    let record = sqlx::query_as::<_, LibraryItemRecord>(
        r#"
        INSERT INTO library_items
            (id, library_id, name, elements, element_count, position, content_hash, content_hash_version)
        VALUES ($1, $2, $3, $4, $5, 0, $6, 1)
        RETURNING id, library_id, name, elements, element_count, position,
                  content_hash, created_at, updated_at,
                  (SELECT scope FROM libraries WHERE id=$2) AS scope,
                  (SELECT collection_id FROM libraries WHERE id=$2) AS collection_id
        "#,
    )
    .bind(&item_id)
    .bind(&library.id)
    .bind(name)
    .bind(&elements)
    .bind(elements.as_array().unwrap().len() as i32)
    .bind(&hash)
    .fetch_one(&mut *tx)
    .await?;
    for asset_id in asset_ids {
        sqlx::query("INSERT INTO library_item_assets (library_item_id, asset_id) VALUES ($1, $2)")
            .bind(&item_id)
            .bind(asset_id)
            .execute(&mut *tx)
            .await?;
    }
    touch_library(&mut tx, &library.id).await?;
    if library_was_created {
        log_audit(&mut tx, &user.id, "library_created", &library.id, None, 0).await?;
    }
    log_audit(&mut tx, &user.id, "item_added", &library.id, None, 1).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(item_from_record(record)?)))
}

async fn touch_library(tx: &mut Transaction<'_, Postgres>, library_id: &str) -> AppResult<()> {
    sqlx::query("UPDATE libraries SET updated_at=now() WHERE id=$1")
        .bind(library_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

#[utoipa::path(
    get,
    path = "/api/collections/{collection_id}/library",
    tag = "library",
    params(
        ("collection_id" = String, Path, description = "Owned private collection ID"),
        ("limit" = Option<i64>, Query, description = "Page size, 1 to 100"),
        ("cursor" = Option<String>, Query, description = "Opaque page cursor")
    ),
    responses(
        (status = 200, description = "Paged collection library", body = CollectionLibraryPage),
        (status = 401, description = "Not signed in"),
        (status = 403, description = "Library unavailable to this plan"),
        (status = 404, description = "Collection not found")
    )
)]
pub async fn list_collection_items(
    State(state): State<AppState>,
    cookies: Cookies,
    Path(collection_id): Path<String>,
    query: Result<Query<LibraryQuery>, axum::extract::rejection::QueryRejection>,
) -> AppResult<Json<CollectionLibraryPage>> {
    let user = session::require_current_user(&cookies, &state).await?;
    let (collection, billing_status) =
        collection_read_access(&state.db, &collection_id, &user.id).await?;
    let Query(query) =
        query.map_err(|_| AppError::BadRequest("Invalid pagination parameters.".into()))?;
    let limit = query.limit.unwrap_or(50);
    let offset = decode_page_cursor(query.cursor.as_deref())?;
    if !(1..=MAX_LIBRARY_PAGE_SIZE).contains(&limit) {
        return Err(AppError::BadRequest(
            "limit must be between 1 and 100.".into(),
        ));
    }
    let library = sqlx::query_as::<_, LibraryRecord>(
        "SELECT id, updated_at
         FROM libraries WHERE collection_id=$1 AND scope='collection'",
    )
    .bind(&collection_id)
    .fetch_optional(&state.db)
    .await?;
    let Some(library) = library else {
        return Ok(Json(CollectionLibraryPage {
            library: None,
            items: vec![],
            next_cursor: None,
            etag: None,
            readonly_reason: readonly_reason(billing_status).map(str::to_string),
        }));
    };
    let records = sqlx::query_as::<_, LibraryItemRecord>(
        r#"
        SELECT item.id, item.library_id, item.name, item.elements, item.element_count,
               item.position, item.content_hash, item.created_at, item.updated_at,
               library.scope, library.collection_id
        FROM library_items item
        JOIN libraries library ON library.id=item.library_id
        WHERE library.id=$1
        ORDER BY item.position, item.created_at DESC, item.id
        LIMIT $2 OFFSET $3
        "#,
    )
    .bind(&library.id)
    .bind(limit + 1)
    .bind(offset)
    .fetch_all(&state.db)
    .await?;
    let has_more = records.len() > limit as usize;
    let items = records
        .into_iter()
        .take(limit as usize)
        .map(item_from_record)
        .collect::<AppResult<Vec<_>>>()?;
    let etag = library_etag(&library);
    let metadata = descriptor(
        "collection",
        Some(collection),
        Some(library),
        readonly_reason(billing_status),
    );
    Ok(Json(CollectionLibraryPage {
        library: Some(metadata),
        next_cursor: has_more.then(|| encode_page_cursor(offset + limit)),
        etag: Some(etag),
        readonly_reason: readonly_reason(billing_status).map(str::to_string),
        items,
    }))
}

fn decode_page_cursor(cursor: Option<&str>) -> AppResult<i64> {
    let Some(cursor) = cursor else {
        return Ok(0);
    };
    if cursor.len() > 128 {
        return Err(AppError::BadRequest("Invalid page cursor.".into()));
    }
    let decoded = URL_SAFE_NO_PAD
        .decode(cursor)
        .map_err(|_| AppError::BadRequest("Invalid page cursor.".into()))?;
    let offset = std::str::from_utf8(&decoded)
        .ok()
        .and_then(|value| value.parse::<i64>().ok())
        .filter(|offset| *offset >= 0)
        .ok_or_else(|| AppError::BadRequest("Invalid page cursor.".into()))?;
    Ok(offset)
}

fn encode_page_cursor(offset: i64) -> String {
    URL_SAFE_NO_PAD.encode(offset.to_string())
}

fn library_etag(library: &LibraryRecord) -> String {
    format!(
        "W/\"{}-{}\"",
        library.id,
        library.updated_at.timestamp_micros()
    )
}

#[utoipa::path(
    patch,
    path = "/api/library/items/{id}",
    tag = "library",
    params(("id" = String, Path, description = "Personal library item ID")),
    request_body = RenameItemInput,
    responses(
        (status = 200, description = "Renamed library item", body = LibraryItem),
        (status = 400, description = "Invalid name"),
        (status = 401, description = "Not signed in"),
        (status = 404, description = "Library item not found")
    )
)]
pub async fn rename_item(
    State(state): State<AppState>,
    cookies: Cookies,
    Path(item_id): Path<String>,
    AppJson(input): AppJson<RenameItemInput>,
) -> AppResult<Json<LibraryItem>> {
    rename_item_inner(state, cookies, item_id, None, input).await
}

#[utoipa::path(
    patch,
    path = "/api/collections/{collection_id}/library/items/{id}",
    tag = "library",
    params(
        ("collection_id" = String, Path, description = "Owned private collection ID"),
        ("id" = String, Path, description = "Library item ID")
    ),
    request_body = RenameItemInput,
    responses(
        (status = 200, description = "Renamed collection library item", body = LibraryItem),
        (status = 401, description = "Not signed in"),
        (status = 403, description = "Active paid plan required"),
        (status = 404, description = "Collection library item not found")
    )
)]
pub async fn rename_collection_item(
    State(state): State<AppState>,
    cookies: Cookies,
    Path((collection_id, item_id)): Path<(String, String)>,
    AppJson(input): AppJson<RenameItemInput>,
) -> AppResult<Json<LibraryItem>> {
    rename_item_inner(state, cookies, item_id, Some(collection_id), input).await
}

async fn rename_item_inner(
    state: AppState,
    cookies: Cookies,
    item_id: String,
    collection_id: Option<String>,
    input: RenameItemInput,
) -> AppResult<Json<LibraryItem>> {
    let user = session::require_current_user(&cookies, &state).await?;
    if let Some(collection_id) = collection_id.as_deref() {
        collection_write_access(&state.db, collection_id, &user.id).await?;
    }
    rate_limit_write(&state.db, &user.id).await?;
    let name = validate_name(&input.name)?;
    let record = sqlx::query_as::<_, LibraryItemRecord>(
        r#"
        UPDATE library_items item
        SET name=$2, updated_at=now()
        FROM libraries library
        WHERE item.id=$1 AND item.library_id=library.id
          AND library.owner_id=$3
          AND (($4::text IS NULL AND library.scope='user')
               OR (library.scope='collection' AND library.collection_id=$4))
          AND ($4::text IS NULL OR EXISTS(
              SELECT 1 FROM private_collections c
              WHERE c.id=library.collection_id AND c.owner_id=$3 AND c.deleted_at IS NULL
          ))
        RETURNING item.id, item.library_id, item.name, item.elements,
                  item.element_count, item.position, item.content_hash,
                  item.created_at, item.updated_at, library.scope, library.collection_id
        "#,
    )
    .bind(item_id)
    .bind(name)
    .bind(&user.id)
    .bind(collection_id.as_deref())
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Library item not found.".into()))?;
    touch_library_by_id(&state.db, &record.library_id).await?;
    Ok(Json(item_from_record(record)?))
}

async fn touch_library_by_id(db: &PgPool, library_id: &str) -> AppResult<()> {
    sqlx::query("UPDATE libraries SET updated_at=now() WHERE id=$1")
        .bind(library_id)
        .execute(db)
        .await?;
    Ok(())
}

#[utoipa::path(
    delete,
    path = "/api/library/items/{id}",
    tag = "library",
    params(("id" = String, Path, description = "Personal library item ID")),
    responses(
        (status = 204, description = "Library item deleted"),
        (status = 401, description = "Not signed in"),
        (status = 404, description = "Library item not found")
    )
)]
pub async fn delete_item(
    State(state): State<AppState>,
    cookies: Cookies,
    Path(item_id): Path<String>,
) -> AppResult<StatusCode> {
    delete_item_inner(state, cookies, item_id, None).await
}

#[utoipa::path(
    delete,
    path = "/api/collections/{collection_id}/library/items/{id}",
    tag = "library",
    params(
        ("collection_id" = String, Path, description = "Owned private collection ID"),
        ("id" = String, Path, description = "Library item ID")
    ),
    responses(
        (status = 204, description = "Collection library item deleted"),
        (status = 401, description = "Not signed in"),
        (status = 403, description = "Active paid plan required"),
        (status = 404, description = "Collection library item not found")
    )
)]
pub async fn delete_collection_item(
    State(state): State<AppState>,
    cookies: Cookies,
    Path((collection_id, item_id)): Path<(String, String)>,
) -> AppResult<StatusCode> {
    delete_item_inner(state, cookies, item_id, Some(collection_id)).await
}

async fn delete_item_inner(
    state: AppState,
    cookies: Cookies,
    item_id: String,
    collection_id: Option<String>,
) -> AppResult<StatusCode> {
    let user = session::require_current_user(&cookies, &state).await?;
    if let Some(collection_id) = collection_id.as_deref() {
        collection_write_access(&state.db, collection_id, &user.id).await?;
    }
    rate_limit_write(&state.db, &user.id).await?;
    let mut tx = state.db.begin().await?;
    let deleted = sqlx::query_as::<_, (String, String)>(
        r#"
        DELETE FROM library_items item
        USING libraries library
        WHERE item.id=$1 AND item.library_id=library.id
          AND library.owner_id=$2
          AND (($3::text IS NULL AND library.scope='user')
               OR (library.scope='collection' AND library.collection_id=$3))
          AND ($3::text IS NULL OR EXISTS(
              SELECT 1 FROM private_collections c
              WHERE c.id=library.collection_id AND c.owner_id=$2 AND c.deleted_at IS NULL
          ))
        RETURNING item.library_id, library.scope
        "#,
    )
    .bind(item_id)
    .bind(&user.id)
    .bind(collection_id.as_deref())
    .fetch_optional(&mut *tx)
    .await?;
    let (library_id, _) =
        deleted.ok_or_else(|| AppError::NotFound("Library item not found.".into()))?;
    touch_library(&mut tx, &library_id).await?;
    log_audit(&mut tx, &user.id, "item_deleted", &library_id, None, 1).await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    post,
    path = "/api/library/reorder",
    tag = "library",
    request_body = ReorderInput,
    responses((status = 200, description = "Personal library item order updated"))
)]
pub async fn reorder_personal_items(
    State(state): State<AppState>,
    cookies: Cookies,
    AppJson(input): AppJson<ReorderInput>,
) -> AppResult<Json<Value>> {
    reorder_items(state, cookies, None, input).await
}

#[utoipa::path(
    post,
    path = "/api/collections/{collection_id}/library/reorder",
    tag = "library",
    params(("collection_id" = String, Path, description = "Owned private collection ID")),
    request_body = ReorderInput,
    responses((status = 200, description = "Collection library item order updated"))
)]
pub async fn reorder_collection_items(
    State(state): State<AppState>,
    cookies: Cookies,
    Path(collection_id): Path<String>,
    AppJson(input): AppJson<ReorderInput>,
) -> AppResult<Json<Value>> {
    reorder_items(state, cookies, Some(collection_id), input).await
}

async fn reorder_items(
    state: AppState,
    cookies: Cookies,
    collection_id: Option<String>,
    input: ReorderInput,
) -> AppResult<Json<Value>> {
    let user = session::require_current_user(&cookies, &state).await?;
    if let Some(collection_id) = collection_id.as_deref() {
        collection_write_access(&state.db, collection_id, &user.id).await?;
    }
    let order = match (input.item_ids, input.item_id, input.position) {
        (Some(item_ids), None, None) if !item_ids.is_empty() => {
            let unique: HashSet<_> = item_ids.iter().collect();
            if unique.len() != item_ids.len() {
                return Err(AppError::BadRequest(
                    "itemIds cannot contain duplicates.".into(),
                ));
            }
            Some(item_ids)
        }
        (None, Some(item_id), Some(position)) if position >= 0 => {
            move_to_position(state, collection_id, item_id, position, user.id).await?;
            return Ok(Json(json!({"success": true})));
        }
        _ => {
            return Err(AppError::BadRequest(
                "Provide either a complete itemIds order or itemId and position.".into(),
            ))
        }
    };
    rate_limit_write(&state.db, &user.id).await?;
    let mut tx = state.db.begin().await?;
    let (library, _) = ensure_library(&mut tx, &user.id, collection_id.as_deref()).await?;
    sqlx::query("SELECT id FROM libraries WHERE id=$1 FOR UPDATE")
        .bind(&library.id)
        .fetch_one(&mut *tx)
        .await?;
    let item_ids = order.expect("order input was validated above");
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM library_items WHERE library_id=$1 AND id=ANY($2)")
            .bind(&library.id)
            .bind(&item_ids)
            .fetch_one(&mut *tx)
            .await?;
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM library_items WHERE library_id=$1")
        .bind(&library.id)
        .fetch_one(&mut *tx)
        .await?;
    if count != item_ids.len() as i64 || total != item_ids.len() as i64 {
        return Err(AppError::BadRequest(
            "itemIds must list every item in this library exactly once.".into(),
        ));
    }
    for (position, item_id) in item_ids.iter().enumerate() {
        sqlx::query(
            "UPDATE library_items SET position=$3, updated_at=now()
             WHERE id=$1 AND library_id=$2",
        )
        .bind(item_id)
        .bind(&library.id)
        .bind(position as i64)
        .execute(&mut *tx)
        .await?;
    }
    touch_library(&mut tx, &library.id).await?;
    tx.commit().await?;
    Ok(Json(json!({"success": true})))
}

async fn move_to_position(
    state: AppState,
    collection_id: Option<String>,
    item_id: String,
    position: i64,
    user_id: String,
) -> AppResult<()> {
    rate_limit_write(&state.db, &user_id).await?;
    let mut tx = state.db.begin().await?;
    let (library, _) = ensure_library(&mut tx, &user_id, collection_id.as_deref()).await?;
    sqlx::query("SELECT id FROM libraries WHERE id=$1 FOR UPDATE")
        .bind(&library.id)
        .fetch_one(&mut *tx)
        .await?;
    let mut item_ids = sqlx::query_scalar::<_, String>(
        "SELECT id FROM library_items WHERE library_id=$1 ORDER BY position, created_at DESC, id",
    )
    .bind(&library.id)
    .fetch_all(&mut *tx)
    .await?;
    let Some(old_position) = item_ids.iter().position(|id| id == &item_id) else {
        return Err(AppError::NotFound("Library item not found.".into()));
    };
    if position >= item_ids.len() as i64 {
        return Err(AppError::BadRequest(
            "position must be within the current library order.".into(),
        ));
    }
    let item = item_ids.remove(old_position);
    item_ids.insert(position as usize, item);
    for (new_position, item_id) in item_ids.iter().enumerate() {
        sqlx::query(
            "UPDATE library_items SET position=$3, updated_at=now()
             WHERE library_id=$1 AND id=$2",
        )
        .bind(&library.id)
        .bind(item_id)
        .bind(new_position as i64)
        .execute(&mut *tx)
        .await?;
    }
    touch_library(&mut tx, &library.id).await?;
    tx.commit().await?;
    Ok(())
}

#[utoipa::path(
    post,
    path = "/api/library/items/{id}/copy",
    tag = "library",
    params(("id" = String, Path, description = "Owned library item ID")),
    request_body = TransferInput,
    responses(
        (status = 201, description = "Library item copied", body = LibraryItem),
        (status = 404, description = "Library item or target collection not found")
    )
)]
pub async fn copy_item(
    State(state): State<AppState>,
    cookies: Cookies,
    Path(item_id): Path<String>,
    AppJson(input): AppJson<TransferInput>,
) -> AppResult<(StatusCode, Json<LibraryItem>)> {
    transfer_item(state, cookies, item_id, input, false).await
}

#[utoipa::path(
    post,
    path = "/api/library/items/{id}/move",
    tag = "library",
    params(("id" = String, Path, description = "Owned library item ID")),
    request_body = TransferInput,
    responses(
        (status = 200, description = "Library item moved", body = LibraryItem),
        (status = 404, description = "Library item or target collection not found")
    )
)]
pub async fn move_item(
    State(state): State<AppState>,
    cookies: Cookies,
    Path(item_id): Path<String>,
    AppJson(input): AppJson<TransferInput>,
) -> AppResult<Json<LibraryItem>> {
    transfer_item(state, cookies, item_id, input, true)
        .await
        .map(|(_, item)| Json(item.0))
}

async fn transfer_item(
    state: AppState,
    cookies: Cookies,
    item_id: String,
    input: TransferInput,
    move_item: bool,
) -> AppResult<(StatusCode, Json<LibraryItem>)> {
    let user = session::require_current_user(&cookies, &state).await?;
    let source = load_owned_item(&state.db, &user.id, &item_id).await?;
    let source_collection_id = source.collection_id.clone();
    let billing_status = get_billing_state(&state.db, &user.id).await?.status;
    if source.scope == "collection" && billing_status == BillingStatus::Free {
        return Err(AppError::Forbidden(
            "Collection libraries require a paid plan.".into(),
        ));
    }
    let target_collection_id = match input.target {
        TransferTarget::Personal(target) if target == "personal" => None,
        TransferTarget::Personal(_) => {
            return Err(AppError::BadRequest("Unknown library target.".into()))
        }
        TransferTarget::Collection(target) => {
            collection_write_access(&state.db, &target.collection_id, &user.id).await?;
            Some(target.collection_id)
        }
    };
    if move_item
        && (source.scope == "collection" || target_collection_id.is_some())
        && billing_status != BillingStatus::Active
    {
        return Err(AppError::Forbidden(
            "An active paid plan is required to move collection library items.".into(),
        ));
    }
    if source_collection_id.as_deref() == target_collection_id.as_deref() {
        if move_item {
            return Err(AppError::Conflict(
                "The item is already in the target library.".into(),
            ));
        }
    }
    rate_limit_write(&state.db, &user.id).await?;
    let source = item_from_record(source)?;
    let source_library_id = load_item_library_id(&state.db, &item_id).await?;
    let mut tx = state.db.begin().await?;
    let (target_library, target_library_was_created) =
        ensure_library(&mut tx, &user.id, target_collection_id.as_deref()).await?;
    sqlx::query("SELECT id FROM libraries WHERE id=$1 FOR UPDATE")
        .bind(&target_library.id)
        .fetch_one(&mut *tx)
        .await?;
    sqlx::query("UPDATE library_items SET position=position+1 WHERE library_id=$1")
        .bind(&target_library.id)
        .execute(&mut *tx)
        .await?;

    let result_record = if move_item {
        sqlx::query_as::<_, LibraryItemRecord>(
            r#"
            UPDATE library_items item
            SET library_id=$2, position=0, updated_at=now()
            WHERE item.id=$1 AND item.library_id=$3
            RETURNING item.id, item.library_id, item.name, item.elements,
                      item.element_count, item.position, item.content_hash,
                      item.created_at, item.updated_at,
                      (SELECT scope FROM libraries WHERE id=$2) AS scope,
                      (SELECT collection_id FROM libraries WHERE id=$2) AS collection_id
            "#,
        )
        .bind(&item_id)
        .bind(&target_library.id)
        .bind(&source_library_id)
        .fetch_one(&mut *tx)
        .await?
    } else {
        let copied_id = nanoid::nanoid!(16);
        let elements = serde_json::to_value(&source.elements)
            .map_err(|error| AppError::Internal(anyhow::anyhow!(error)))?;
        let record = sqlx::query_as::<_, LibraryItemRecord>(
            r#"
            INSERT INTO library_items
                (id, library_id, name, elements, element_count, position, content_hash, content_hash_version)
            VALUES($1, $2, $3, $4, $5, 0, $6, 1)
            RETURNING id, library_id, name, elements, element_count, position,
                      content_hash, created_at, updated_at,
                      (SELECT scope FROM libraries WHERE id=$2) AS scope,
                      (SELECT collection_id FROM libraries WHERE id=$2) AS collection_id
            "#,
        )
        .bind(&copied_id)
        .bind(&target_library.id)
        .bind(&source.name)
        .bind(&elements)
        .bind(source.element_count)
        .bind(&source.content_hash)
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO library_item_assets(library_item_id, asset_id)
             SELECT $1, asset_id FROM library_item_assets WHERE library_item_id=$2",
        )
        .bind(&copied_id)
        .bind(&item_id)
        .execute(&mut *tx)
        .await?;
        record
    };
    touch_library(&mut tx, &target_library.id).await?;
    if target_library_was_created {
        log_audit(
            &mut tx,
            &user.id,
            "library_created",
            &target_library.id,
            None,
            0,
        )
        .await?;
    }
    if move_item {
        touch_library(&mut tx, &source_library_id).await?;
        log_audit(
            &mut tx,
            &user.id,
            "item_moved",
            &source_library_id,
            Some(&target_library.id),
            1,
        )
        .await?;
    } else {
        log_audit(
            &mut tx,
            &user.id,
            "item_copied",
            &source_library_id,
            Some(&target_library.id),
            1,
        )
        .await?;
    }
    tx.commit().await?;
    let status = if move_item {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    Ok((status, Json(item_from_record(result_record)?)))
}

async fn load_owned_item(
    db: &PgPool,
    owner_id: &str,
    item_id: &str,
) -> AppResult<LibraryItemRecord> {
    sqlx::query_as::<_, LibraryItemRecord>(
        r#"
        SELECT item.id, item.library_id, item.name, item.elements, item.element_count,
               item.position, item.content_hash, item.created_at, item.updated_at,
               library.scope, library.collection_id
        FROM library_items item
        JOIN libraries library ON library.id=item.library_id
        WHERE item.id=$1 AND library.owner_id=$2
          AND (library.scope='user' OR EXISTS(
              SELECT 1 FROM private_collections c
              WHERE c.id=library.collection_id AND c.owner_id=$2 AND c.deleted_at IS NULL
          ))
        "#,
    )
    .bind(item_id)
    .bind(owner_id)
    .fetch_optional(db)
    .await?
    .ok_or_else(|| AppError::NotFound("Library item not found.".into()))
}

async fn load_item_library_id(db: &PgPool, item_id: &str) -> AppResult<String> {
    Ok(
        sqlx::query_scalar("SELECT library_id FROM library_items WHERE id=$1")
            .bind(item_id)
            .fetch_one(db)
            .await?,
    )
}

pub(crate) async fn duplicate_library_to_collection(
    tx: &mut Transaction<'_, Postgres>,
    actor_id: &str,
    source_collection_id: &str,
    target_collection_id: &str,
) -> AppResult<()> {
    let source = sqlx::query_as::<_, LibraryRecord>(
        "SELECT id, updated_at
         FROM libraries WHERE collection_id=$1 AND scope='collection'",
    )
    .bind(source_collection_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(source) = source else {
        return Ok(());
    };
    let owner_id: String =
        sqlx::query_scalar("SELECT owner_id FROM private_collections WHERE id=$1")
            .bind(target_collection_id)
            .fetch_one(&mut **tx)
            .await?;
    let (target, created) = ensure_collection_library(tx, target_collection_id, &owner_id).await?;
    let source_items = sqlx::query_as::<_, LibraryItemRecord>(
        r#"
        SELECT item.id, item.library_id, item.name, item.elements, item.element_count,
               item.position, item.content_hash, item.created_at, item.updated_at,
               library.scope, library.collection_id
        FROM library_items item JOIN libraries library ON library.id=item.library_id
        WHERE item.library_id=$1 ORDER BY item.position, item.id
        "#,
    )
    .bind(&source.id)
    .fetch_all(&mut **tx)
    .await?;
    for item in &source_items {
        let item_id = nanoid::nanoid!(16);
        let hash = content_hash(&item.elements)?;
        sqlx::query(
            "INSERT INTO library_items
             (id, library_id, name, elements, element_count, position, content_hash, content_hash_version)
             VALUES($1,$2,$3,$4,$5,$6,$7,1)",
        )
        .bind(&item_id)
        .bind(&target.id)
        .bind(&item.name)
        .bind(&item.elements)
        .bind(item.element_count)
        .bind(item.position)
        .bind(hash)
        .execute(&mut **tx)
        .await?;
        sqlx::query(
            "INSERT INTO library_item_assets(library_item_id, asset_id)
             SELECT $1, asset_id FROM library_item_assets WHERE library_item_id=$2",
        )
        .bind(item_id)
        .bind(&item.id)
        .execute(&mut **tx)
        .await?;
    }
    touch_library(tx, &target.id).await?;
    if created {
        log_audit(tx, actor_id, "library_created", &target.id, None, 0).await?;
    }
    if !source_items.is_empty() {
        log_audit(
            tx,
            actor_id,
            "item_copied",
            &source.id,
            Some(&target.id),
            source_items.len() as i64,
        )
        .await?;
    }
    Ok(())
}

#[utoipa::path(
    get,
    path = "/api/boards/{id}/libraries",
    tag = "library",
    params(("id" = String, Path, description = "Board ID")),
    responses(
        (status = 200, description = "Personal and owned collection-library metadata", body = BoardLibrariesResponse),
        (status = 401, description = "Not signed in"),
        (status = 404, description = "Board not found")
    )
)]
pub async fn board_libraries(
    State(state): State<AppState>,
    cookies: Cookies,
    Path(board_id): Path<String>,
) -> AppResult<Json<BoardLibrariesResponse>> {
    let user = session::require_current_user(&cookies, &state).await?;
    authz::require_board_role(&state.db, &board_id, &user.id, BoardRole::Viewer).await?;
    let billing_status = get_billing_state(&state.db, &user.id).await?.status;
    let personal = sqlx::query_as::<_, LibraryRecord>(
        "SELECT id, updated_at
         FROM libraries WHERE owner_id=$1 AND scope='user'",
    )
    .bind(&user.id)
    .fetch_optional(&state.db)
    .await?;
    let mut libraries = vec![descriptor("user", None, personal, None)];
    let collections = sqlx::query_as::<_, (String, String, Option<String>, Option<DateTime<Utc>>)>(
        r#"
        SELECT collection.id, collection.name, library.id, library.updated_at
        FROM private_collection_boards membership
        JOIN private_collections collection
          ON collection.id=membership.collection_id
         AND collection.owner_id=membership.owner_id
        LEFT JOIN libraries library
          ON library.collection_id=collection.id AND library.scope='collection'
        WHERE membership.board_id=$1 AND collection.owner_id=$2
          AND collection.deleted_at IS NULL
        ORDER BY collection.created_at, collection.id
        "#,
    )
    .bind(&board_id)
    .bind(&user.id)
    .fetch_all(&state.db)
    .await?;
    for (collection_id, collection_name, library_id, updated_at) in collections {
        let library = match (library_id, updated_at) {
            (Some(id), Some(updated_at)) => Some(LibraryRecord { id, updated_at }),
            _ => None,
        };
        let mut entry = descriptor(
            "collection",
            Some(CollectionRecord {
                id: collection_id,
                name: collection_name,
            }),
            library,
            readonly_reason(billing_status),
        );
        if billing_status == BillingStatus::Free {
            entry.etag = None;
            entry.updated_at = None;
        }
        libraries.push(entry);
    }
    Ok(Json(BoardLibrariesResponse { libraries }))
}

fn descriptor(
    scope: &str,
    collection: Option<CollectionRecord>,
    library: Option<LibraryRecord>,
    readonly_reason: Option<&str>,
) -> LibraryDescriptor {
    let (collection_id, collection_name) = collection
        .map(|collection| (Some(collection.id), Some(collection.name)))
        .unwrap_or((None, None));
    let etag = library.as_ref().map(library_etag);
    let updated_at = library.as_ref().map(|library| library.updated_at);
    LibraryDescriptor {
        id: library.map(|library| library.id),
        scope: scope.to_string(),
        collection_id,
        collection_name,
        etag,
        updated_at,
        readonly_reason: readonly_reason.map(str::to_string),
        locked: readonly_reason == Some("upgrade_required"),
    }
}

fn validate_name(value: &str) -> AppResult<String> {
    let value = value.trim();
    let length = value.chars().count();
    if !(1..=80).contains(&length) || value.chars().any(char::is_control) {
        return Err(AppError::BadRequest(
            "Library item names must be 1 to 80 characters.".to_string(),
        ));
    }
    Ok(value.to_string())
}

fn validate_elements(elements: &[Value], is_pro: bool) -> AppResult<Vec<Value>> {
    if elements.is_empty() {
        return Err(AppError::BadRequest(
            "Library items must contain at least one element.".into(),
        ));
    }
    if elements.len() > MAX_LIBRARY_ITEM_ELEMENTS {
        return Err(AppError::LimitReached {
            code: "LIBRARY_LIMIT_REACHED",
            message: "Library items must contain between 1 and 5000 elements.".into(),
            usage: elements.len() as i64,
            limit: MAX_LIBRARY_ITEM_ELEMENTS as i64,
        });
    }
    let svg_limit = schema::svg_element_limit(is_pro);
    if let Some(usage) = elements.iter().find_map(|element| {
        let svg = element
            .get("svg")
            .and_then(Value::as_str)
            .filter(|_| element.get("type").and_then(Value::as_str) == Some("svg"))?;
        let length = svg.encode_utf16().count();
        (length > svg_limit).then_some(length)
    }) {
        return Err(AppError::LimitReached {
            code: "LIBRARY_LIMIT_REACHED",
            message: "An SVG exceeds the per-item size limit for your plan.".into(),
            usage: usage as i64,
            limit: svg_limit as i64,
        });
    }
    let elements = schema::elements_with_svg_limit(elements, "Invalid library item", svg_limit)?;
    let mut ids = HashSet::with_capacity(elements.len());
    for element in &elements {
        let id = element["id"].as_str().ok_or_else(|| {
            AppError::BadRequest("Library elements must have valid IDs.".to_string())
        })?;
        if !ids.insert(id) {
            return Err(AppError::BadRequest(
                "Library items cannot contain duplicate element IDs.".to_string(),
            ));
        }
    }
    Ok(elements)
}

async fn validate_image_assets(
    state: &AppState,
    board_id: &str,
    elements: &[Value],
    max_bytes: i64,
) -> AppResult<Vec<String>> {
    let asset_ids: Vec<String> = elements
        .iter()
        .filter(|element| element["type"] == "image")
        .filter_map(|element| element["assetId"].as_str().map(str::to_string))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    if asset_ids.is_empty() {
        return Ok(asset_ids);
    }

    let assets = sqlx::query_as::<_, ImageAsset>(
        r#"
        SELECT asset.id, asset.byte_size
        FROM assets AS asset
        WHERE asset.id = ANY($1)
          AND asset.status = 'complete'
          AND (
              asset.board_id = $2
              OR EXISTS (
                  SELECT 1
                  FROM board_elements AS element
                  WHERE element.board_id = $2
                    AND element.type = 'image'
                    AND element.data->>'assetId' = asset.id
              )
          )
        "#,
    )
    .bind(&asset_ids)
    .bind(board_id)
    .fetch_all(&state.db)
    .await?;
    let found_ids: HashSet<_> = assets.iter().map(|asset| asset.id.as_str()).collect();
    if found_ids.len() != asset_ids.len()
        || asset_ids.iter().any(|id| !found_ids.contains(id.as_str()))
    {
        return Err(AppError::BadRequest(
            "Library images must be valid uploads from the editable source board.".to_string(),
        ));
    }
    if let Some(asset) = assets
        .iter()
        .find(|asset| i64::from(asset.byte_size) > max_bytes)
    {
        return Err(AppError::LimitReached {
            code: "LIBRARY_LIMIT_REACHED",
            message: "A library image exceeds the per-image size limit for your plan.".into(),
            usage: i64::from(asset.byte_size),
            limit: max_bytes,
        });
    }
    Ok(asset_ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rectangle(id: &str) -> Value {
        json!({
            "id": id,
            "type": "rectangle",
            "x": 0.0,
            "y": 0.0,
            "width": 100.0,
            "height": 80.0,
            "rotation": 0.0,
            "strokeColor": "#1e1e1e",
            "backgroundColor": "transparent",
            "strokeWidth": 2.0,
            "opacity": 1.0,
            "fillStyle": "solid",
            "strokeStyle": "solid",
            "roughness": 0.0,
            "seed": 1.0,
            "groupId": null,
            "zIndex": 0.0,
            "locked": false
        })
    }

    #[test]
    fn validates_library_item_names() {
        assert_eq!(validate_name("  Box  ").unwrap(), "Box");
        assert!(validate_name("  ").is_err());
        assert!(validate_name(&"x".repeat(81)).is_err());
        assert!(validate_name("bad\nname").is_err());
    }

    #[test]
    fn validates_library_element_count_and_unique_ids() {
        assert!(validate_elements(&[], false).is_err());
        assert!(validate_elements(&[rectangle("same"), rectangle("same")], false).is_err());
        assert_eq!(
            validate_elements(&[rectangle("one")], false).unwrap().len(),
            1
        );
        let too_many = vec![rectangle("one"); MAX_LIBRARY_ITEM_ELEMENTS + 1];
        assert!(validate_elements(&too_many, false).is_err());
        let oversized_svg = json!({
            "id":"large-svg",
            "type":"svg",
            "svg":"x".repeat(schema::svg_element_limit(false) + 1)
        });
        assert!(matches!(
            validate_elements(&[oversized_svg], false),
            Err(AppError::LimitReached { .. })
        ));
    }

    #[test]
    fn transfer_targets_accept_personal_and_collection_shapes() {
        assert!(serde_json::from_value::<TransferInput>(json!({"target":"personal"})).is_ok());
        assert!(serde_json::from_value::<TransferInput>(
            json!({"target":{"collection_id":"collection-id"}})
        )
        .is_ok());
        assert!(serde_json::from_value::<TransferInput>(
            json!({"target":{"collectionId":"collection-id"}})
        )
        .is_ok());
        assert!(serde_json::from_value::<TransferInput>(
            json!({"target":{"collection_id":"collection-id","owner_id":"attacker"}})
        )
        .is_err());
    }

    #[test]
    fn content_hash_is_stable_and_plan_state_has_readonly_reasons() {
        let elements = json!([rectangle("shape")]);
        assert_eq!(
            content_hash(&elements).unwrap(),
            content_hash(&elements).unwrap()
        );
        assert_eq!(readonly_reason(BillingStatus::Expired), Some("plan_lapsed"));
        assert_eq!(
            readonly_reason(BillingStatus::Free),
            Some("upgrade_required")
        );
        assert_eq!(readonly_reason(BillingStatus::Active), None);
    }

    #[test]
    fn canonical_content_hash_ignores_identity_ids_but_preserves_content() {
        let original = json!([
            {
                "id":"old-shape",
                "type":"rectangle",
                "x":0.0,"y":0.0,"width":100.0,"height":80.0,
                "groupId":"old-group",
                "frameId":"old-frame",
                "startBinding":{"elementId":"old-target","focus":0.5,"gap":1.0}
            },
            {
                "id":"old-target",
                "type":"ellipse",
                "x":10.0,"y":20.0,"width":30.0,"height":40.0,
                "groupId":"old-group"
            },
            {"id":"old-frame","type":"frame","x":0.0,"y":0.0,"width":200.0,"height":200.0}
        ]);
        let mut reidentified = original.clone();
        reidentified[0]["id"] = json!("new-shape");
        reidentified[0]["groupId"] = json!("new-group");
        reidentified[0]["frameId"] = json!("new-frame");
        reidentified[0]["startBinding"]["elementId"] = json!("new-target");
        reidentified[1]["id"] = json!("new-target");
        reidentified[1]["groupId"] = json!("new-group");
        reidentified[2]["id"] = json!("new-frame");

        assert_eq!(
            content_hash(&original).unwrap(),
            content_hash(&reidentified).unwrap()
        );
        reidentified[0]["width"] = json!(101.0);
        assert_ne!(
            content_hash(&original).unwrap(),
            content_hash(&reidentified).unwrap()
        );
        let image_a = json!([{
            "id":"image-a",
            "type":"image",
            "assetId":"asset-a",
            "x":0.0,"y":0.0,"width":20.0,"height":20.0
        }]);
        let mut image_b = image_a.clone();
        image_b[0]["id"] = json!("image-b");
        image_b[0]["assetId"] = json!("asset-b");
        assert_ne!(
            content_hash(&image_a).unwrap(),
            content_hash(&image_b).unwrap()
        );
    }
}
