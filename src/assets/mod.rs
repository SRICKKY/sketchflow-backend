//! Uploaded binary asset storage (avatars, pasted board images).
//!
//! Mirrors `@/lib/storage` in the Next.js app: bytes are written to disk
//! under `ASSET_STORAGE_DIR` and metadata is persisted in the `assets`
//! table so `GET /api/uploads/{assetId}` can serve them back with the
//! right `Content-Type`.

use sqlx::{FromRow, PgPool};
use tokio::fs;

use crate::error::AppError;

#[derive(Debug, Clone, FromRow)]
struct AssetRow {
    mime_type: String,
    storage_path: String,
}

#[derive(Debug, Clone)]
pub struct StoredAsset {
    pub id: String,
    pub mime_type: String,
    pub width: i32,
    pub height: i32,
}

pub struct ReadAsset {
    pub buffer: Vec<u8>,
    pub mime_type: String,
}

fn mime_to_ext(mime: &str) -> &'static str {
    match mime {
        "image/jpeg" => ".jpg",
        "image/webp" => ".webp",
        "image/gif" => ".gif",
        _ => ".png",
    }
}

/// Same-origin URL the canvas / avatar uses to fetch the asset back.
pub fn asset_url(id: &str) -> String {
    format!("/api/uploads/{id}")
}

/// Writes `buffer` to disk under `storage_dir` and records its metadata in
/// the `assets` table.
pub async fn save_asset(
    db: &PgPool,
    storage_dir: &str,
    buffer: &[u8],
    mime_type: &str,
    width: i32,
    height: i32,
) -> Result<StoredAsset, AppError> {
    fs::create_dir_all(storage_dir)
        .await
        .map_err(|err| AppError::Internal(anyhow::anyhow!(err)))?;

    let id = nanoid::nanoid!(16);
    let ext = mime_to_ext(mime_type);
    let file_name = format!("{id}{ext}");
    let storage_path = format!("{storage_dir}/{file_name}");

    fs::write(&storage_path, buffer)
        .await
        .map_err(|err| AppError::Internal(anyhow::anyhow!(err)))?;

    sqlx::query(
        r#"
        INSERT INTO assets (id, mime_type, width, height, byte_size, storage_path)
        VALUES ($1, $2, $3, $4, $5, $6)
        "#,
    )
    .bind(&id)
    .bind(mime_type)
    .bind(width)
    .bind(height)
    .bind(buffer.len() as i32)
    .bind(&storage_path)
    .execute(db)
    .await?;

    Ok(StoredAsset {
        id,
        mime_type: mime_type.to_string(),
        width,
        height,
    })
}

/// Reads a previously stored asset's bytes back, if it exists.
pub async fn read_asset(db: &PgPool, id: &str) -> Result<Option<ReadAsset>, AppError> {
    let row =
        sqlx::query_as::<_, AssetRow>("SELECT mime_type, storage_path FROM assets WHERE id = $1")
            .bind(id)
            .fetch_optional(db)
            .await?;

    let Some(row) = row else {
        return Ok(None);
    };

    match fs::read(&row.storage_path).await {
        Ok(buffer) => Ok(Some(ReadAsset {
            buffer,
            mime_type: row.mime_type,
        })),
        Err(_) => Ok(None),
    }
}
