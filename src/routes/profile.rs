use axum::extract::{Multipart, State};
use axum::Json;
use serde::{Deserialize, Serialize};
use tower_cookies::Cookies;

use crate::{
    assets,
    auth::{password, session},
    error::{AppError, AppResult},
    extract::AppJson,
    profile::avatar::{
        avatar_change_quota_error, avatar_changes_used_this_month, avatar_file_error,
        avatar_month_key, AVATAR_CHANGES_PER_MONTH,
    },
    state::AppState,
};

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct UpdateProfileNameRequest {
    /// Display name shown to collaborators.
    pub name: String,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ProfileNameResponse {
    pub name: String,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UpdateProfilePasswordRequest {
    /// Required when the account already has a password.
    pub current_password: Option<String>,
    pub new_password: String,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProfilePasswordResponse {
    pub has_password: bool,
}

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

fn validate_profile_name(value: &str) -> AppResult<String> {
    let name = value.trim();
    if name.is_empty() {
        return Err(AppError::BadRequest("Name is required".to_string()));
    }
    if name.encode_utf16().count() > 80 {
        return Err(AppError::BadRequest("Name is too long".to_string()));
    }
    Ok(name.to_string())
}

fn validate_new_password(value: &str) -> AppResult<()> {
    if value.encode_utf16().count() < 8 {
        return Err(AppError::BadRequest(
            "Password must be at least 8 characters.".to_string(),
        ));
    }
    if !value
        .chars()
        .any(|character| !character.is_ascii_alphanumeric())
    {
        return Err(AppError::BadRequest(
            "Password must include a special character.".to_string(),
        ));
    }
    Ok(())
}

/// Update the signed-in user's display name.
#[utoipa::path(
    patch,
    path = "/api/profile",
    tag = "profile",
    request_body = UpdateProfileNameRequest,
    responses(
        (status = 200, description = "Profile name updated", body = ProfileNameResponse),
        (status = 400, description = "Invalid name"),
        (status = 401, description = "Not signed in"),
        (status = 404, description = "Account not found")
    )
)]
pub async fn update_profile_name(
    State(state): State<AppState>,
    cookies: Cookies,
    AppJson(body): AppJson<UpdateProfileNameRequest>,
) -> AppResult<Json<ProfileNameResponse>> {
    let user = session::require_current_user(&cookies, &state).await?;
    let name = validate_profile_name(&body.name)?;
    let result = sqlx::query("UPDATE users SET name = $1 WHERE id = $2")
        .bind(&name)
        .bind(&user.id)
        .execute(&state.db)
        .await?;
    if result.rows_affected() == 0 {
        return Err(AppError::NotFound("Account not found".to_string()));
    }
    Ok(Json(ProfileNameResponse { name }))
}

/// Set or update the signed-in user's password.
#[utoipa::path(
    patch,
    path = "/api/profile/password",
    tag = "profile",
    request_body = UpdateProfilePasswordRequest,
    responses(
        (status = 200, description = "Password updated", body = ProfilePasswordResponse),
        (status = 400, description = "Invalid password or current password"),
        (status = 401, description = "Not signed in"),
        (status = 409, description = "Password changed by another request")
    )
)]
pub async fn update_profile_password(
    State(state): State<AppState>,
    cookies: Cookies,
    AppJson(body): AppJson<UpdateProfilePasswordRequest>,
) -> AppResult<Json<ProfilePasswordResponse>> {
    let user = session::require_current_user(&cookies, &state).await?;
    validate_new_password(&body.new_password)?;

    if let Some(current_hash) = user.password_hash.as_deref() {
        let current_password = body
            .current_password
            .as_deref()
            .ok_or_else(|| AppError::BadRequest("Current password is required.".to_string()))?;
        if !password::verify_password(current_password, current_hash) {
            return Err(AppError::BadRequest(
                "Current password is incorrect.".to_string(),
            ));
        }
        if current_password == body.new_password {
            return Err(AppError::BadRequest(
                "Choose a new password that is different from the current one.".to_string(),
            ));
        }

        let new_hash = password::hash_password(&body.new_password)?;
        let result =
            sqlx::query("UPDATE users SET password_hash = $1 WHERE id = $2 AND password_hash = $3")
                .bind(new_hash)
                .bind(&user.id)
                .bind(current_hash)
                .execute(&state.db)
                .await?;
        if result.rows_affected() == 0 {
            return Err(AppError::Conflict(
                "Password changed before this update completed. Please try again.".to_string(),
            ));
        }
    } else {
        if body
            .current_password
            .as_deref()
            .is_some_and(|value| !value.is_empty())
        {
            return Err(AppError::BadRequest(
                "This account does not have a current password.".to_string(),
            ));
        }
        let new_hash = password::hash_password(&body.new_password)?;
        let result = sqlx::query(
            "UPDATE users SET password_hash = $1 WHERE id = $2 AND password_hash IS NULL",
        )
        .bind(new_hash)
        .bind(&user.id)
        .execute(&state.db)
        .await?;
        if result.rows_affected() == 0 {
            return Err(AppError::Conflict(
                "A password was set before this update completed. Please try again.".to_string(),
            ));
        }
    }

    Ok(Json(ProfilePasswordResponse { has_password: true }))
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
    while let Some(field) = multipart.next_field().await? {
        if field.name() == Some("file") {
            file_mime = field
                .content_type()
                .map(str::to_string)
                .or(Some("application/octet-stream".to_string()));
            file_bytes = Some(field.bytes().await?.to_vec());
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

    let asset = assets::save_asset(&state.db, &state.config, &buffer, &mime_type, 256, 256).await?;
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

#[cfg(test)]
mod tests {
    use super::{validate_new_password, validate_profile_name};

    #[test]
    fn trims_valid_profile_names() {
        assert_eq!(
            validate_profile_name("  Ada Lovelace  ").unwrap(),
            "Ada Lovelace"
        );
    }

    #[test]
    fn rejects_empty_profile_names() {
        assert!(validate_profile_name("  ").is_err());
    }

    #[test]
    fn enforces_the_name_limit_in_utf16_code_units() {
        assert!(validate_profile_name(&"😀".repeat(41)).is_err());
        assert!(validate_profile_name(&"😀".repeat(40)).is_ok());
    }

    #[test]
    fn validates_password_length_and_special_character() {
        assert!(validate_new_password("short!").is_err());
        assert!(validate_new_password("password").is_err());
        assert!(validate_new_password("password!").is_ok());
    }
}
