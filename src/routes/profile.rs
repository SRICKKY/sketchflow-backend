use axum::extract::{Multipart, State};
use axum::Json;
use serde::Serialize;
use tower_cookies::Cookies;

use crate::{
    assets,
    auth::session,
    error::{AppError, AppResult},
    profile::avatar::{
        avatar_change_quota_error, avatar_changes_used_this_month, avatar_file_error,
        avatar_month_key, AVATAR_CHANGES_PER_MONTH,
    },
    state::AppState,
};

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct AvatarResponse {
    pub image: Option<String>,
    #[serde(
        rename = "avatarChangesRemaining",
        skip_serializing_if = "Option::is_none"
    )]
    pub avatar_changes_remaining: Option<i32>,
}

#[derive(Debug, sqlx::FromRow)]
struct AvatarQuota {
    avatar_change_count: i32,
    avatar_change_month: Option<String>,
}

/// Upload a new avatar image for the signed-in user (limited to 5 changes
/// per calendar month).
#[utoipa::path(
    post,
    path = "/api/profile/avatar",
    tag = "profile",
    request_body(content = String, description = "multipart/form-data with a `file` field", content_type = "multipart/form-data"),
    responses(
        (status = 200, description = "Avatar updated", body = AvatarResponse),
        (status = 400, description = "Missing or invalid image"),
        (status = 401, description = "Not signed in"),
        (status = 404, description = "Account not found"),
        (status = 429, description = "Monthly avatar-change quota exceeded")
    )
)]
pub async fn upload_avatar(
    State(state): State<AppState>,
    cookies: Cookies,
    mut multipart: Multipart,
) -> AppResult<Json<AvatarResponse>> {
    let user = session::require_current_user(&cookies, &state).await?;

    let mut file_bytes: Option<Vec<u8>> = None;
    let mut file_mime: Option<String> = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|err| AppError::BadRequest(err.to_string()))?
    {
        if field.name() == Some("file") {
            file_mime = field
                .content_type()
                .map(str::to_string)
                .or(Some("application/octet-stream".to_string()));
            file_bytes = Some(
                field
                    .bytes()
                    .await
                    .map_err(|err| AppError::BadRequest(err.to_string()))?
                    .to_vec(),
            );
        }
    }

    let (Some(buffer), Some(mime_type)) = (file_bytes, file_mime) else {
        return Err(AppError::BadRequest("Choose an image".to_string()));
    };

    if let Some(message) = avatar_file_error(&mime_type, buffer.len() as i64) {
        return Err(AppError::BadRequest(message));
    }

    let month = avatar_month_key(chrono::Utc::now());
    let quota = sqlx::query_as::<_, AvatarQuota>(
        "SELECT avatar_change_count, avatar_change_month FROM users WHERE id = $1",
    )
    .bind(&user.id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Account not found".to_string()))?;

    let used = avatar_changes_used_this_month(
        quota.avatar_change_count,
        quota.avatar_change_month.as_deref(),
        &month,
    );
    if let Some(message) = avatar_change_quota_error(used) {
        return Err(AppError::TooManyRequests(message));
    }

    let asset = assets::save_asset(
        &state.db,
        &state.config.asset_storage_dir,
        &buffer,
        &mime_type,
        256,
        256,
    )
    .await?;
    let image = assets::asset_url(&asset.id);

    let same_month = quota.avatar_change_month.as_deref() == Some(month.as_str());
    let updated_rows = if same_month {
        sqlx::query(
            r#"
            UPDATE users
            SET image = $1, avatar_change_month = $2, avatar_change_count = avatar_change_count + 1
            WHERE id = $3 AND avatar_change_month = $2 AND avatar_change_count < $4
            "#,
        )
        .bind(&image)
        .bind(&month)
        .bind(&user.id)
        .bind(AVATAR_CHANGES_PER_MONTH)
        .execute(&state.db)
        .await?
        .rows_affected()
    } else {
        sqlx::query(
            r#"
            UPDATE users
            SET image = $1, avatar_change_month = $2, avatar_change_count = 1
            WHERE id = $3 AND (avatar_change_month IS NULL OR avatar_change_month <> $2)
            "#,
        )
        .bind(&image)
        .bind(&month)
        .bind(&user.id)
        .execute(&state.db)
        .await?
        .rows_affected()
    };

    if updated_rows == 0 {
        let message = avatar_change_quota_error(AVATAR_CHANGES_PER_MONTH)
            .unwrap_or_else(|| "Quota exceeded".to_string());
        return Err(AppError::TooManyRequests(message));
    }

    let remaining_used = if same_month { used + 1 } else { 1 };
    let avatar_changes_remaining = (AVATAR_CHANGES_PER_MONTH - remaining_used).max(0);

    Ok(Json(AvatarResponse {
        image: Some(image),
        avatar_changes_remaining: Some(avatar_changes_remaining),
    }))
}

/// Remove the signed-in user's avatar image.
#[utoipa::path(
    delete,
    path = "/api/profile/avatar",
    tag = "profile",
    responses(
        (status = 200, description = "Avatar removed", body = AvatarResponse),
        (status = 401, description = "Not signed in")
    )
)]
pub async fn delete_avatar(
    State(state): State<AppState>,
    cookies: Cookies,
) -> AppResult<Json<AvatarResponse>> {
    let user = session::require_current_user(&cookies, &state).await?;

    sqlx::query("UPDATE users SET image = NULL WHERE id = $1")
        .bind(&user.id)
        .execute(&state.db)
        .await?;

    Ok(Json(AvatarResponse {
        image: None,
        avatar_changes_remaining: None,
    }))
}
