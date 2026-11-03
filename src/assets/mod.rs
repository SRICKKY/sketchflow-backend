//! Uploaded binary asset storage (avatars, pasted board images).
//!
//! When `S3_BUCKET` is configured, image bytes and metadata sidecars are
//! stored in S3. Otherwise, bytes are stored under `ASSET_STORAGE_DIR`.

use aws_credential_types::Credentials;
use aws_sdk_s3::{
    error::ProvideErrorMetadata, presigning::PresigningConfig, primitives::ByteStream, Client,
};
use serde::Serialize;
use sqlx::{FromRow, PgPool};
use std::time::Duration;
use tokio::{fs, sync::OnceCell};

use crate::{config::AppConfig, error::AppError};

static S3_CLIENT: OnceCell<Client> = OnceCell::const_new();
static S3_PRESIGN_CLIENT: OnceCell<Client> = OnceCell::const_new();

#[derive(Debug, Clone, FromRow)]
struct AssetRow {
    mime_type: String,
    storage_path: String,
}

#[derive(Debug, FromRow)]
struct CleanupAssetRow {
    id: String,
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
}

pub struct ReadAsset {
    pub buffer: Vec<u8>,
    pub mime_type: String,
}

pub struct PresignedUpload {
    pub upload_url: String,
    pub required_headers: std::collections::HashMap<String, String>,
}

pub fn s3_upload_storage_path(
    config: &AppConfig,
    id: &str,
    mime_type: &str,
) -> Result<String, AppError> {
    let bucket = config
        .s3_bucket
        .as_deref()
        .filter(|bucket| !bucket.trim().is_empty())
        .ok_or_else(|| {
            AppError::ServiceUnavailable(
                "Direct image uploads require S3-compatible storage.".to_string(),
            )
        })?;
    Ok(format!("s3://{bucket}/{}", s3_image_key(id, mime_type)))
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

async fn build_s3_client(
    config: &AppConfig,
    endpoint_url: Option<&str>,
) -> Result<Client, AppError> {
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
    let mut s3_config = aws_sdk_s3::config::Builder::from(&shared_config);
    if let Some(endpoint_url) = endpoint_url {
        s3_config = s3_config.endpoint_url(endpoint_url).force_path_style(true);
    }
    Ok(Client::from_conf(s3_config.build()))
}

async fn s3_client(config: &AppConfig) -> Result<&'static Client, AppError> {
    S3_CLIENT
        .get_or_try_init(|| async {
            build_s3_client(config, config.s3_endpoint_url.as_deref()).await
        })
        .await
}

async fn s3_presign_client(config: &AppConfig) -> Result<&'static Client, AppError> {
    let endpoint_url = config
        .s3_public_endpoint_url
        .as_deref()
        .or(config.s3_endpoint_url.as_deref());
    if endpoint_url == config.s3_endpoint_url.as_deref() {
        return s3_client(config).await;
    }
    S3_PRESIGN_CLIENT
        .get_or_try_init(|| async { build_s3_client(config, endpoint_url).await })
        .await
}

/// Same-origin URL the canvas / avatar uses to fetch the asset back.
pub fn asset_url(id: &str) -> String {
    format!("/api/uploads/{id}")
}

pub async fn presign_upload(
    config: &AppConfig,
    id: &str,
    mime_type: &str,
    expires_in: Duration,
) -> Result<PresignedUpload, AppError> {
    let bucket = config
        .s3_bucket
        .as_deref()
        .filter(|bucket| !bucket.trim().is_empty())
        .ok_or_else(|| {
            AppError::ServiceUnavailable(
                "Direct image uploads require S3-compatible storage.".to_string(),
            )
        })?;
    let key = s3_image_key(id, mime_type);
    let presigning_config = PresigningConfig::expires_in(expires_in)
        .map_err(|error| AppError::Internal(anyhow::anyhow!(error)))?;
    let request = s3_presign_client(config)
        .await?
        .put_object()
        .bucket(bucket)
        .key(&key)
        .content_type(mime_type)
        .presigned(presigning_config)
        .await
        .map_err(|error| AppError::Internal(anyhow::anyhow!(error)))?;
    let required_headers = request
        .headers()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect();

    Ok(PresignedUpload {
        upload_url: request.uri().to_string(),
        required_headers,
    })
}

pub async fn verify_uploaded_object(
    config: &AppConfig,
    storage_path: &str,
    mime_type: &str,
    expected_size: i64,
) -> Result<bool, AppError> {
    let Some((bucket, key)) = parse_s3_storage_path(storage_path) else {
        return Err(AppError::Internal(anyhow::anyhow!(
            "direct upload has an invalid S3 storage path"
        )));
    };
    let client = s3_client(config).await?;
    let head = match client.head_object().bucket(bucket).key(key).send().await {
        Ok(head) => head,
        Err(error)
            if matches!(
                error
                    .as_service_error()
                    .and_then(|service_error| service_error.code()),
                Some("NoSuchKey" | "NotFound" | "404")
            ) =>
        {
            return Ok(false);
        }
        Err(error) => return Err(AppError::Internal(anyhow::anyhow!(error))),
    };
    if head.content_length() != Some(expected_size) || head.content_type() != Some(mime_type) {
        return Ok(false);
    }

    let object = client
        .get_object()
        .bucket(bucket)
        .key(key)
        .range("bytes=0-11")
        .send()
        .await
        .map_err(|error| AppError::Internal(anyhow::anyhow!(error)))?;
    let prefix = object
        .body
        .collect()
        .await
        .map_err(|error| AppError::Internal(anyhow::anyhow!(error)))?
        .into_bytes();
    Ok(image_signature_matches(&prefix, mime_type))
}

pub async fn cleanup_abandoned_uploads(db: &PgPool, config: &AppConfig) -> Result<u64, AppError> {
    let rows = sqlx::query_as::<_, CleanupAssetRow>(
        r#"
        SELECT id, storage_path
        FROM assets AS asset
        WHERE cleanup_at IS NULL
          AND (
              (
                  status IN ('pending', 'failed', 'expired')
                  AND expires_at <= now()
              )
              OR (
                  status = 'complete'
                  AND board_id IS NOT NULL
                  AND completed_at <= now() - interval '24 hours'
                  AND NOT EXISTS (
                      SELECT 1
                      FROM board_elements
                      WHERE board_elements.type = 'image'
                        AND board_elements.data->>'assetId' = asset.id
                  )
              )
          )
        ORDER BY COALESCE(expires_at, completed_at)
        LIMIT 100
        "#,
    )
    .fetch_all(db)
    .await?;

    let mut cleaned = 0;
    for row in rows {
        delete_storage_object(config, &row.storage_path).await?;
        sqlx::query(
            r#"
            UPDATE assets
            SET status = CASE
                    WHEN status IN ('pending', 'complete') THEN 'expired'
                    ELSE status
                END,
                cleanup_at = now()
            WHERE id = $1 AND cleanup_at IS NULL
            "#,
        )
        .bind(&row.id)
        .execute(db)
        .await?;
        cleaned += 1;
    }

    Ok(cleaned)
}

async fn delete_storage_object(config: &AppConfig, storage_path: &str) -> Result<(), AppError> {
    if let Some((bucket, key)) = parse_s3_storage_path(storage_path) {
        s3_client(config)
            .await?
            .delete_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
            .map_err(|error| AppError::Internal(anyhow::anyhow!(error)))?;
        return Ok(());
    }

    match fs::remove_file(storage_path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(AppError::Internal(anyhow::anyhow!(error))),
    }
}

pub async fn asset_board_ids(db: &PgPool, asset_id: &str) -> Result<Vec<String>, AppError> {
    Ok(sqlx::query_scalar(
        r#"
        SELECT board_id FROM assets WHERE id = $1 AND board_id IS NOT NULL
        UNION
        SELECT board_id FROM board_elements
        WHERE type = 'image' AND data->>'assetId' = $1
        "#,
    )
    .bind(asset_id)
    .fetch_all(db)
    .await?)
}

fn image_signature_matches(bytes: &[u8], mime_type: &str) -> bool {
    match mime_type {
        "image/png" => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        "image/jpeg" => bytes.starts_with(b"\xff\xd8\xff"),
        "image/gif" => bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a"),
        "image/webp" => bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP",
        _ => false,
    }
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

    Ok(StoredAsset { id })
}

/// Reads a previously stored asset's bytes back, if it exists.
pub async fn read_asset(
    db: &PgPool,
    config: &AppConfig,
    id: &str,
) -> Result<Option<ReadAsset>, AppError> {
    let row = sqlx::query_as::<_, AssetRow>(
        "SELECT mime_type, storage_path FROM assets WHERE id = $1 AND status = 'complete'",
    )
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

    #[test]
    fn verifies_supported_image_signatures() {
        assert!(image_signature_matches(
            b"\x89PNG\r\n\x1a\nmore",
            "image/png"
        ));
        assert!(image_signature_matches(b"\xff\xd8\xffmore", "image/jpeg"));
        assert!(image_signature_matches(b"GIF89amore", "image/gif"));
        assert!(image_signature_matches(b"RIFF1234WEBPmore", "image/webp"));
        assert!(!image_signature_matches(b"not an image", "image/png"));
        assert!(!image_signature_matches(b"RIFF1234WAVE", "image/webp"));
    }
}
