use axum::extract::{Multipart, Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;
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
/// Matches the Next.js app's `usesRemoteAssetStorage()` ? 4.5MB : 8MB infra
/// ceiling; this backend always stores on local disk, so the larger bound
/// applies.
const INFRA_MAX_BYTES: i64 = 8 * 1024 * 1024;

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct UploadResponse {
    #[serde(rename = "assetId")]
    pub asset_id: String,
    #[serde(rename = "mimeType")]
    pub mime_type: String,
    #[serde(rename = "naturalWidth")]
    pub natural_width: i32,
    #[serde(rename = "naturalHeight")]
    pub natural_height: i32,
    pub url: String,
}

#[derive(Debug, sqlx::FromRow)]
struct ImageCountRow {
    count: i64,
}

/// Upload an image asset onto a board (requires `EDITOR` role or higher).
#[utoipa::path(
    post,
    path = "/api/uploads",
    tag = "uploads",
    request_body(content = String, description = "multipart/form-data with `boardId`, `file`, and optional `width`/`height` fields", content_type = "multipart/form-data"),
    responses(
        (status = 200, description = "Asset uploaded", body = UploadResponse),
        (status = 400, description = "Missing file, unsupported type, or too large"),
        (status = 401, description = "Not signed in"),
        (status = 403, description = "Insufficient board role or image count limit"),
        (status = 404, description = "Board not found")
    )
)]
pub async fn upload_image(
    State(state): State<AppState>,
    cookies: Cookies,
    mut multipart: Multipart,
) -> AppResult<Json<UploadResponse>> {
    let user = session::require_current_user(&cookies, &state).await?;

    let mut board_id: Option<String> = None;
    let mut width: Option<i32> = None;
    let mut height: Option<i32> = None;
    let mut file_bytes: Option<Vec<u8>> = None;
    let mut file_mime: Option<String> = None;

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|err| AppError::BadRequest(err.to_string()))?
    {
        match field.name().map(str::to_string).as_deref() {
            Some("boardId") => {
                board_id = Some(
                    field
                        .text()
                        .await
                        .map_err(|err| AppError::BadRequest(err.to_string()))?,
                );
            }
            Some("width") => {
                width = field.text().await.ok().and_then(|v| v.parse::<i32>().ok());
            }
            Some("height") => {
                height = field.text().await.ok().and_then(|v| v.parse::<i32>().ok());
            }
            Some("file") => {
                file_mime = field.content_type().map(str::to_string);
                file_bytes = Some(
                    field
                        .bytes()
                        .await
                        .map_err(|err| AppError::BadRequest(err.to_string()))?
                        .to_vec(),
                );
            }
            _ => {}
        }
    }

    let board_id = board_id.filter(|s| !s.is_empty());
    let (Some(board_id), Some(buffer), Some(mime_type)) = (board_id, file_bytes, file_mime) else {
        return Err(AppError::BadRequest("Missing file".to_string()));
    };

    let access =
        authz::require_board_role(&state.db, &board_id, &user.id, BoardRole::Editor).await?;

    if !ALLOWED_MIME_TYPES.contains(&mime_type.as_str()) {
        return Err(AppError::BadRequest("Unsupported image type".to_string()));
    }

    let billing = get_billing_state(&state.db, &access.owner_id).await?;
    let is_pro = billing.status == BillingStatus::Active;
    let plan_max = limits::image_byte_cap(is_pro);
    let max_bytes = plan_max.min(INFRA_MAX_BYTES);
    let byte_len = buffer.len() as i64;
    if byte_len > max_bytes {
        let message = if byte_len > plan_max {
            limits::IMAGE_SIZE_MESSAGE
        } else {
            "Image is too large"
        };
        return Err(AppError::BadRequest(message.to_string()));
    }

    let current_images = sqlx::query_as::<_, ImageCountRow>(
        "SELECT count(*) AS count FROM board_elements WHERE board_id = $1 AND type = 'image'",
    )
    .bind(&board_id)
    .fetch_one(&state.db)
    .await?
    .count;

    let next_count = current_images + 1;
    let count_cap = limits::image_count_cap(is_pro);
    if next_count > count_cap {
        let message = if is_pro {
            format!("Pro plan allows {count_cap} images per board.")
        } else {
            limits::IMAGE_COUNT_MESSAGE.to_string()
        };
        return Err(AppError::BadRequest(message));
    }

    let asset = assets::save_asset(
        &state.db,
        &state.config.asset_storage_dir,
        &buffer,
        &mime_type,
        width.filter(|w| *w > 0).unwrap_or(800),
        height.filter(|h| *h > 0).unwrap_or(600),
    )
    .await?;

    Ok(Json(UploadResponse {
        asset_id: asset.id.clone(),
        mime_type: asset.mime_type,
        natural_width: asset.width,
        natural_height: asset.height,
        url: assets::asset_url(&asset.id),
    }))
}

/// Streams a previously uploaded asset's raw bytes.
#[utoipa::path(
    get,
    path = "/api/uploads/{assetId}",
    tag = "uploads",
    params(("assetId" = String, Path, description = "Asset ID returned by `POST /api/uploads` or `/api/profile/avatar`")),
    responses(
        (status = 200, description = "Asset bytes", content_type = "application/octet-stream"),
        (status = 404, description = "Asset not found")
    )
)]
pub async fn get_asset(
    State(state): State<AppState>,
    Path(asset_id): Path<String>,
) -> AppResult<Response> {
    let asset = assets::read_asset(&state.db, &asset_id)
        .await?
        .ok_or_else(|| AppError::NotFound("Not found".to_string()))?;

    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, asset.mime_type),
            (
                header::CACHE_CONTROL,
                "public, max-age=31536000, immutable".to_string(),
            ),
        ],
        asset.buffer,
    )
        .into_response())
}
