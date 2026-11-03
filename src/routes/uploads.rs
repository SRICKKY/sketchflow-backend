use axum::{
    extract::{Path, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use std::time::Duration;
use tower_cookies::Cookies;

use crate::{
    assets,
    auth::session,
    authz::{self, BoardRole},
    billing::{get_billing_state, limits, BillingStatus},
    error::{AppError, AppResult},
    state::AppState,
};

const ALLOWED_MIME_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/webp", "image/gif"];
const PRESIGNED_UPLOAD_TTL: Duration = Duration::from_secs(10 * 60);
const MAX_IMAGE_DIMENSION: i32 = 32_768;
const MAX_PENDING_UPLOADS_PER_USER: i64 = 5;

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PresignUploadRequest {
    pub board_id: String,
    pub mime_type: String,
    pub byte_size: i64,
    pub natural_width: i32,
    pub natural_height: i32,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PresignUploadResponse {
    pub asset_id: String,
    pub upload_url: String,
    pub required_headers: std::collections::HashMap<String, String>,
    pub expires_in_seconds: u64,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UploadResponse {
    pub asset_id: String,
    pub mime_type: String,
    pub natural_width: i32,
    pub natural_height: i32,
    pub url: String,
}

#[derive(Debug, FromRow)]
struct UploadRow {
    owner_id: Option<String>,
    board_id: Option<String>,
    mime_type: String,
    width: i32,
    height: i32,
    byte_size: i32,
    storage_path: String,
    status: String,
    unexpired: bool,
}

#[derive(Debug, FromRow)]
struct ImageCountRow {
    count: i64,
}

#[utoipa::path(
    post,
    path = "/api/uploads/presign",
    tag = "uploads",
    request_body = PresignUploadRequest,
    responses(
        (status = 200, description = "Presigned S3 upload URL", body = PresignUploadResponse),
        (status = 400, description = "Unsupported image type, invalid dimensions, or too large"),
        (status = 401, description = "Not signed in"),
        (status = 403, description = "Insufficient board role or image count limit"),
        (status = 404, description = "Board not found"),
        (status = 413, description = "Image is too large"),
        (status = 429, description = "Too many uploads in progress"),
        (status = 503, description = "S3 storage is not configured")
    )
)]
pub async fn presign_upload(
    State(state): State<AppState>,
    cookies: Cookies,
    Json(request): Json<PresignUploadRequest>,
) -> AppResult<Json<PresignUploadResponse>> {
    let user = session::require_current_user(&cookies, &state).await?;
    let board_id = request.board_id.trim();
    if board_id.is_empty() {
        return Err(AppError::BadRequest("Board is required.".to_string()));
    }
    if !ALLOWED_MIME_TYPES.contains(&request.mime_type.as_str()) {
        return Err(AppError::BadRequest("Unsupported image type.".to_string()));
    }
    if request.natural_width <= 0
        || request.natural_height <= 0
        || request.natural_width > MAX_IMAGE_DIMENSION
        || request.natural_height > MAX_IMAGE_DIMENSION
    {
        return Err(AppError::BadRequest(
            "Invalid image dimensions.".to_string(),
        ));
    }
    if request.byte_size <= 0 {
        return Err(AppError::BadRequest("Image is empty.".to_string()));
    }

    let access =
        authz::require_board_role(&state.db, board_id, &user.id, BoardRole::Editor).await?;
    let billing = get_billing_state(&state.db, &access.owner_id).await?;
    let is_pro = billing.status == BillingStatus::Active;
    let plan_max = limits::image_byte_cap(is_pro);
    let max_bytes = plan_max.min(limits::PRO_MAX_IMAGE_BYTES);
    if request.byte_size > max_bytes {
        let message = if request.byte_size > plan_max {
            limits::image_size_message(is_pro)
        } else {
            "Image is too large"
        };
        return Err(AppError::PayloadTooLarge(message.to_string()));
    }

    let pending_uploads = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM assets WHERE owner_id = $1 AND status = 'pending' AND expires_at > now()",
    )
    .bind(&user.id)
    .fetch_one(&state.db)
    .await?;
    if pending_uploads >= MAX_PENDING_UPLOADS_PER_USER {
        return Err(AppError::TooManyRequests(
            "Too many image uploads are already in progress. Please wait and try again."
                .to_string(),
        ));
    }

    let current_images = sqlx::query_as::<_, ImageCountRow>(
        "SELECT count(*) AS count FROM board_elements WHERE board_id = $1 AND type = 'image'",
    )
    .bind(board_id)
    .fetch_one(&state.db)
    .await?
    .count;
    let count_cap = limits::image_count_cap(is_pro);
    if current_images + 1 > count_cap {
        let message = if is_pro {
            format!("Pro plan allows {count_cap} images per board.")
        } else {
            limits::IMAGE_COUNT_MESSAGE.to_string()
        };
        return Err(AppError::BadRequest(message));
    }

    let asset_id = nanoid::nanoid!(16);
    let storage_path =
        assets::s3_upload_storage_path(&state.config, &asset_id, &request.mime_type)?;
    sqlx::query(
        r#"
        INSERT INTO assets (
            id, mime_type, width, height, byte_size, storage_path,
            owner_id, board_id, status, expires_at
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 'pending', now() + interval '11 minutes')
        "#,
    )
    .bind(&asset_id)
    .bind(&request.mime_type)
    .bind(request.natural_width)
    .bind(request.natural_height)
    .bind(request.byte_size as i32)
    .bind(&storage_path)
    .bind(&user.id)
    .bind(board_id)
    .execute(&state.db)
    .await?;

    let presigned = match assets::presign_upload(
        &state.config,
        &asset_id,
        &request.mime_type,
        PRESIGNED_UPLOAD_TTL,
    )
    .await
    {
        Ok(presigned) => presigned,
        Err(error) => {
            sqlx::query(
                "UPDATE assets SET status = 'failed', expires_at = now(), cleanup_at = now() WHERE id = $1",
            )
            .bind(&asset_id)
            .execute(&state.db)
            .await?;
            return Err(error);
        }
    };

    Ok(Json(PresignUploadResponse {
        asset_id,
        upload_url: presigned.upload_url,
        required_headers: presigned.required_headers,
        expires_in_seconds: PRESIGNED_UPLOAD_TTL.as_secs(),
    }))
}

#[utoipa::path(
    post,
    path = "/api/uploads/{asset_id}/complete",
    tag = "uploads",
    params(("asset_id" = String, Path, description = "Pending image asset ID")),
    responses(
        (status = 200, description = "Verified uploaded asset", body = UploadResponse),
        (status = 400, description = "Uploaded object is missing or invalid"),
        (status = 409, description = "Upload is no longer active or has expired"),
        (status = 401, description = "Not signed in"),
        (status = 403, description = "Insufficient board role"),
        (status = 404, description = "Upload not found")
    )
)]
pub async fn complete_upload(
    State(state): State<AppState>,
    cookies: Cookies,
    Path(asset_id): Path<String>,
) -> AppResult<Json<UploadResponse>> {
    let user = session::require_current_user(&cookies, &state).await?;
    let row = load_upload(&state, &asset_id).await?;
    authorize_upload(&state, &user.id, &row).await?;

    if row.status == "complete" {
        return Ok(Json(upload_response(&asset_id, &row)));
    }
    if row.status != "pending" {
        return Err(AppError::Conflict(
            "This upload is no longer active.".to_string(),
        ));
    }
    if !row.unexpired {
        sqlx::query("UPDATE assets SET status = 'expired' WHERE id = $1 AND status = 'pending'")
            .bind(&asset_id)
            .execute(&state.db)
            .await?;
        return Err(AppError::Conflict(
            "The upload has expired. Please upload the image again.".to_string(),
        ));
    }

    let valid = assets::verify_uploaded_object(
        &state.config,
        &row.storage_path,
        &row.mime_type,
        i64::from(row.byte_size),
    )
    .await?;
    if !valid {
        sqlx::query("UPDATE assets SET status = 'failed' WHERE id = $1 AND status = 'pending'")
            .bind(&asset_id)
            .execute(&state.db)
            .await?;
        return Err(AppError::BadRequest(
            "The uploaded image could not be verified.".to_string(),
        ));
    }

    let updated = sqlx::query(
        r#"
        UPDATE assets
        SET status = 'complete', completed_at = now(), expires_at = NULL
        WHERE id = $1 AND owner_id = $2 AND status = 'pending' AND expires_at > now()
        "#,
    )
    .bind(&asset_id)
    .bind(&user.id)
    .execute(&state.db)
    .await?
    .rows_affected();
    if updated == 0 {
        let latest = load_upload(&state, &asset_id).await?;
        if latest.status == "complete" {
            return Ok(Json(upload_response(&asset_id, &latest)));
        }
        return Err(AppError::Conflict(
            "The upload expired before it could be completed.".to_string(),
        ));
    }

    Ok(Json(upload_response(&asset_id, &row)))
}

#[utoipa::path(
    post,
    path = "/api/uploads/{asset_id}/abort",
    tag = "uploads",
    params(("asset_id" = String, Path, description = "Pending image asset ID")),
    responses(
        (status = 204, description = "Upload aborted"),
        (status = 401, description = "Not signed in"),
        (status = 403, description = "Insufficient board role"),
        (status = 404, description = "Upload not found")
    )
)]
pub async fn abort_upload(
    State(state): State<AppState>,
    cookies: Cookies,
    Path(asset_id): Path<String>,
) -> AppResult<StatusCode> {
    let user = session::require_current_user(&cookies, &state).await?;
    let row = load_upload(&state, &asset_id).await?;
    authorize_upload(&state, &user.id, &row).await?;

    if row.status == "pending" {
        sqlx::query("UPDATE assets SET status = 'failed' WHERE id = $1 AND status = 'pending'")
            .bind(&asset_id)
            .execute(&state.db)
            .await?;
    }

    Ok(StatusCode::NO_CONTENT)
}

async fn load_upload(state: &AppState, asset_id: &str) -> AppResult<UploadRow> {
    sqlx::query_as::<_, UploadRow>(
        r#"
        SELECT owner_id, board_id, mime_type, width, height, byte_size,
               storage_path, status, COALESCE(expires_at > now(), false) AS unexpired
        FROM assets
        WHERE id = $1
        "#,
    )
    .bind(asset_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Upload not found.".to_string()))
}

async fn authorize_upload(state: &AppState, user_id: &str, row: &UploadRow) -> AppResult<()> {
    if row.owner_id.as_deref() != Some(user_id) {
        return Err(AppError::NotFound("Upload not found.".to_string()));
    }
    let board_id = row
        .board_id
        .as_deref()
        .ok_or_else(|| AppError::NotFound("Upload not found.".to_string()))?;
    authz::require_board_role(&state.db, board_id, user_id, BoardRole::Editor).await?;
    Ok(())
}

fn upload_response(asset_id: &str, row: &UploadRow) -> UploadResponse {
    UploadResponse {
        asset_id: asset_id.to_string(),
        mime_type: row.mime_type.clone(),
        natural_width: row.width,
        natural_height: row.height,
        url: assets::asset_url(asset_id),
    }
}

/// Streams a previously uploaded asset's raw bytes.
#[utoipa::path(
    get,
    path = "/api/uploads/{asset_id}",
    tag = "uploads",
    params(("asset_id" = String, Path, description = "Completed asset ID")),
    responses(
        (status = 200, description = "Asset bytes", content_type = "application/octet-stream"),
        (status = 403, description = "Board access required"),
        (status = 404, description = "Asset not found")
    )
)]
pub async fn get_asset(
    State(state): State<AppState>,
    cookies: Cookies,
    Path(asset_id): Path<String>,
) -> AppResult<Response> {
    let board_ids = assets::asset_board_ids(&state.db, &asset_id).await?;
    let is_board_asset = !board_ids.is_empty();
    if is_board_asset {
        let user = session::require_current_user(&cookies, &state).await?;
        let mut authorized = false;
        for board_id in board_ids {
            if let Some(access) = authz::get_board_access(&state.db, &board_id, &user.id).await? {
                if access.role >= BoardRole::Viewer {
                    authorized = true;
                    break;
                }
            }
        }
        if !authorized {
            return Err(AppError::Forbidden(
                "You do not have permission to access this image.".to_string(),
            ));
        }
    }

    let asset = assets::read_asset(&state.db, &state.config, &asset_id)
        .await?
        .ok_or_else(|| AppError::NotFound("Not found".to_string()))?;
    let cache_control = if is_board_asset {
        "private, no-store"
    } else {
        "public, max-age=31536000, immutable"
    };

    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, asset.mime_type),
            (header::CACHE_CONTROL, cache_control.to_string()),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
        ],
        asset.buffer,
    )
        .into_response())
}
