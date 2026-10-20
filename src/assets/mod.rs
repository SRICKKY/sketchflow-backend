//! Uploaded binary asset storage (avatars, pasted board images).
//!
//! When `S3_BUCKET` is configured, image bytes and metadata sidecars are
//! stored in S3. Otherwise, bytes are stored under `ASSET_STORAGE_DIR`.

use aws_credential_types::Credentials;
use aws_sdk_s3::{error::ProvideErrorMetadata, primitives::ByteStream, Client};
use serde::Serialize;
use sqlx::{FromRow, PgPool};
use tokio::{fs, sync::OnceCell};

use crate::{config::AppConfig, error::AppError};

static S3_CLIENT: OnceCell<Client> = OnceCell::const_new();

#[derive(Debug, Clone, FromRow)]
struct AssetRow {
    mime_type: String,
    storage_path: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AssetMetadata<'a> {
    id: &'a str,
    mime_type: &'a str,
    width: i32,
    height: i32,
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

fn s3_image_key(id: &str, mime_type: &str) -> String {
    format!("assets/{id}{}", mime_to_ext(mime_type))
}

fn s3_metadata_key(id: &str) -> String {
    format!("assets/{id}.json")
}

fn parse_s3_storage_path(path: &str) -> Option<(&str, &str)> {
    let location = path.strip_prefix("s3://")?;
    let (bucket, key) = location.split_once('/')?;
    (!bucket.is_empty() && !key.is_empty()).then_some((bucket, key))
}

async fn s3_client(config: &AppConfig) -> Result<&'static Client, AppError> {
    S3_CLIENT
        .get_or_try_init(|| async {
            let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest());
            if let Some(region) = config.s3_region.as_deref() {
                loader = loader.region(aws_config::Region::new(region.to_string()));
            }
            if let (Some(access_key), Some(secret_key)) = (
                config.s3_access_key_id.as_deref(),
                config.s3_secret_access_key.as_deref(),
            ) {
                loader = loader.credentials_provider(Credentials::new(
                    access_key,
                    secret_key,
                    config.s3_session_token.clone(),
                    None,
                    "S3 environment variables",
                ));
            }
            let shared_config = loader.load().await;
            Ok::<_, AppError>(Client::new(&shared_config))
        })
        .await
}

/// Same-origin URL the canvas / avatar uses to fetch the asset back.
pub fn asset_url(id: &str) -> String {
    format!("/api/uploads/{id}")
}

/// Writes to S3 when configured, otherwise to local disk, and records asset
/// metadata in the `assets` table.
pub async fn save_asset(
    db: &PgPool,
    config: &AppConfig,
    buffer: &[u8],
    mime_type: &str,
    width: i32,
    height: i32,
) -> Result<StoredAsset, AppError> {
    let id = nanoid::nanoid!(16);
    let storage_path = if let Some(bucket) = config
        .s3_bucket
        .as_deref()
        .filter(|bucket| !bucket.trim().is_empty())
    {
        let client = s3_client(config).await?;
        let image_key = s3_image_key(&id, mime_type);
        let metadata = serde_json::to_vec(&AssetMetadata {
            id: &id,
            mime_type,
            width,
            height,
        })
        .map_err(|err| AppError::Internal(anyhow::anyhow!(err)))?;

        client
            .put_object()
            .bucket(bucket)
            .key(&image_key)
            .content_type(mime_type)
            .body(ByteStream::from(buffer.to_vec()))
            .send()
            .await
            .map_err(|err| AppError::Internal(anyhow::anyhow!(err)))?;
        client
            .put_object()
            .bucket(bucket)
            .key(s3_metadata_key(&id))
            .content_type("application/json")
            .body(ByteStream::from(metadata))
            .send()
            .await
            .map_err(|err| AppError::Internal(anyhow::anyhow!(err)))?;

        format!("s3://{bucket}/{image_key}")
    } else {
        fs::create_dir_all(&config.asset_storage_dir)
            .await
            .map_err(|err| AppError::Internal(anyhow::anyhow!(err)))?;
        let file_name = format!("{id}{}", mime_to_ext(mime_type));
        let storage_path = format!("{}/{file_name}", config.asset_storage_dir);
        fs::write(&storage_path, buffer)
            .await
            .map_err(|err| AppError::Internal(anyhow::anyhow!(err)))?;
        storage_path
    };

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
pub async fn read_asset(
    db: &PgPool,
    config: &AppConfig,
    id: &str,
) -> Result<Option<ReadAsset>, AppError> {
    let row =
        sqlx::query_as::<_, AssetRow>("SELECT mime_type, storage_path FROM assets WHERE id = $1")
            .bind(id)
            .fetch_optional(db)
            .await?;

    let Some(row) = row else {
        return Ok(None);
    };

    if let Some((bucket, key)) = parse_s3_storage_path(&row.storage_path) {
        let client = s3_client(config).await?;
        let object = match client.get_object().bucket(bucket).key(key).send().await {
            Ok(object) => object,
            Err(err)
                if err
                    .as_service_error()
                    .and_then(|service_error| service_error.code())
                    == Some("NoSuchKey") =>
            {
                return Ok(None);
            }
            Err(err) => return Err(AppError::Internal(anyhow::anyhow!(err))),
        };
        let buffer = object
            .body
            .collect()
            .await
            .map_err(|err| AppError::Internal(anyhow::anyhow!(err)))?
            .into_bytes()
            .to_vec();
        return Ok(Some(ReadAsset {
            buffer,
            mime_type: row.mime_type,
        }));
    }

    match fs::read(&row.storage_path).await {
        Ok(buffer) => Ok(Some(ReadAsset {
            buffer,
            mime_type: row.mime_type,
        })),
        Err(_) => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uses_the_same_s3_object_keys_as_the_next_app() {
        assert_eq!(
            s3_image_key("asset123", "image/jpeg"),
            "assets/asset123.jpg"
        );
        assert_eq!(
            s3_image_key("asset123", "image/webp"),
            "assets/asset123.webp"
        );
        assert_eq!(s3_metadata_key("asset123"), "assets/asset123.json");
    }

    #[test]
    fn parses_s3_storage_paths_without_misclassifying_local_paths() {
        assert_eq!(
            parse_s3_storage_path("s3://sketchflow-assets/assets/asset123.png"),
            Some(("sketchflow-assets", "assets/asset123.png"))
        );
        assert_eq!(parse_s3_storage_path("./uploads/asset123.png"), None);
        assert_eq!(parse_s3_storage_path("s3:///assets/asset123.png"), None);
    }
}
