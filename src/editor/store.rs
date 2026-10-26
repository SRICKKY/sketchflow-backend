//! The single revisioned writer for board documents. WebSocket operations,
//! REST operations, document replacement and version restore all allocate
//! revisions here under the `board_collaboration_state` row lock.

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Postgres, Transaction};

use crate::{
    billing::{self, limits, BillingStatus},
    error::{AppError, AppResult},
    routes::collaboration::ElementBatch,
};

/// Operations older than this are pruned; stale bases beyond it must resync.
pub const OPERATION_RETENTION: i64 = 1_000;
pub const AUTO_VERSION_INTERVAL_SECS: i64 = 10 * 60;
pub const FREE_MAX_VERSIONS: i64 = 10;
pub const PRO_MAX_VERSIONS: i64 = 50;
pub const VERSIONS_LIMIT_MESSAGE: &str =
    "Free plan can restore the last 10 snapshots. Upgrade to Pro for full history.";

pub type Tx<'a> = Transaction<'a, Postgres>;

/// JSON stored in `board_operations.operation`.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredOperationPayload {
    client_operation_id: String,
    batch: ElementBatch,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    base_revision: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    request_hash: Option<String>,
}

/// A committed (or idempotently replayed) document operation.
#[derive(Debug, Clone)]
pub struct AppliedOperation {
    pub revision: i64,
    pub client_operation_id: String,
    pub actor_id: String,
    pub batch: ElementBatch,
    pub replayed: bool,
}

pub fn version_cap(is_pro: bool) -> i64 {
    if is_pro {
        PRO_MAX_VERSIONS
    } else {
        FREE_MAX_VERSIONS
    }
}

pub async fn owner_is_pro(db: &PgPool, owner_id: &str) -> AppResult<bool> {
    Ok(billing::get_billing_state(db, owner_id).await?.status == BillingStatus::Active)
}

/// Validates the idempotency key shared by WebSocket and REST writers.
pub fn validate_client_operation_id(value: &str) -> AppResult<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err(AppError::BadRequest("Invalid client operation ID.".into()));
    }
    Ok(())
}

fn request_hash(value: &Value) -> String {
    hex::encode(Sha256::digest(value.to_string().as_bytes()))
}

/// Ensures a revision row exists, locks it, and returns the current revision.
pub async fn lock_revision(tx: &mut Tx<'_>, board_id: &str) -> AppResult<i64> {
    sqlx::query(
        "INSERT INTO board_collaboration_state (board_id, revision)
         SELECT id, 0 FROM boards WHERE id = $1 ON CONFLICT (board_id) DO NOTHING",
    )
    .bind(board_id)
    .execute(&mut **tx)
    .await?;
    sqlx::query_scalar::<_, i64>(
        "SELECT revision FROM board_collaboration_state WHERE board_id = $1 FOR UPDATE",
    )
    .bind(board_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| AppError::NotFound("Board not found".into()))
}

/// Returns the original result when `client_operation_id` was already applied
/// with the same request; rejects reuse of the ID for a different request.
async fn replayed_operation(
    tx: &mut Tx<'_>,
    board_id: &str,
    actor_id: &str,
    client_operation_id: &str,
    hash: &str,
    legacy_payload: &Value,
) -> AppResult<Option<AppliedOperation>> {
    let Some((revision, operation)) = sqlx::query_as::<_, (i64, Value)>(
        "SELECT revision, operation FROM board_operations
         WHERE board_id = $1 AND actor_id = $2 AND client_operation_id = $3",
    )
    .bind(board_id)
    .bind(actor_id)
    .bind(client_operation_id)
    .fetch_optional(&mut **tx)
    .await?
    else {
        return Ok(None);
    };
    let same = match operation.get("requestHash").and_then(Value::as_str) {
        Some(stored) => stored == hash,
        // Rows written before request hashing stored the bare WebSocket payload.
        None => &operation == legacy_payload,
    };
    if !same {
        return Err(AppError::Conflict(
            "Client operation ID was already used for a different change.".into(),
        ));
    }
    let payload: StoredOperationPayload = serde_json::from_value(operation)
        .map_err(|error| AppError::Internal(anyhow::anyhow!(error)))?;
    Ok(Some(AppliedOperation {
        revision,
        client_operation_id: payload.client_operation_id,
        actor_id: actor_id.to_owned(),
        batch: payload.batch,
        replayed: true,
    }))
}

/// Rejects writes based on a revision the server cannot reconcile.
///
/// With `touched = None` (full-document replacement) the base must equal the
/// current revision. Otherwise only operations after `base` that changed one of
/// the touched element IDs conflict, so concurrent edits elsewhere still merge.
async fn check_base_revision(
    tx: &mut Tx<'_>,
    board_id: &str,
    current: i64,
    base: i64,
    touched: Option<&HashSet<&str>>,
) -> AppResult<()> {
    let stale = |message: &str| AppError::RevisionConflict {
        code: "REVISION_STALE",
        message: message.into(),
        revision: current,
    };
    if base < 0 || base > current {
        return Err(stale(
            "Base revision is not valid for this board. Reload the board.",
        ));
    }
    if base == current {
        return Ok(());
    }
    let Some(touched) = touched else {
        return Err(AppError::RevisionConflict {
            code: "REVISION_CONFLICT",
            message: "The board changed since it was loaded. Reload before replacing it.".into(),
            revision: current,
        });
    };
    let operations = sqlx::query_scalar::<_, Value>(
        "SELECT operation FROM board_operations
         WHERE board_id = $1 AND revision > $2 ORDER BY revision",
    )
    .bind(board_id)
    .bind(base)
    .fetch_all(&mut **tx)
    .await?;
    if operations.len() as i64 != current - base {
        return Err(stale(
            "Base revision is older than the retained history. Reload the board.",
        ));
    }
    for operation in operations {
        let batch = &operation["batch"];
        let ids = batch["upserts"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|element| element["id"].as_str())
            .chain(
                batch["deletedIds"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str),
            );
        for id in ids {
            if touched.contains(id) {
                return Err(AppError::RevisionConflict {
                    code: "REVISION_CONFLICT",
                    message: "Another change touched the same elements. Reload and retry.".into(),
                    revision: current,
                });
            }
        }
    }
    Ok(())
}

async fn board_owner(tx: &mut Tx<'_>, board_id: &str) -> AppResult<String> {
    sqlx::query_scalar("SELECT owner_id FROM boards WHERE id = $1")
        .bind(board_id)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or_else(|| AppError::NotFound("Board not found".into()))
}

/// Mirrors `assertImageCountAllowed` using the board owner's plan.
pub async fn assert_image_count(db: &PgPool, owner_id: &str, count: i64) -> AppResult<()> {
    let is_pro = owner_is_pro(db, owner_id).await?;
    let cap = limits::image_count_cap(is_pro);
    if count > cap {
        return Err(AppError::Forbidden(if is_pro {
            format!("Pro plan allows {cap} images per board.")
        } else {
            limits::IMAGE_COUNT_MESSAGE.into()
        }));
    }
    Ok(())
}

/// Allocates the next revision and records the operation for history and retries.
async fn record_operation(
    tx: &mut Tx<'_>,
    board_id: &str,
    current: i64,
    actor_id: &str,
    payload: &Value,
    client_operation_id: &str,
) -> AppResult<i64> {
    let next = current
        .checked_add(1)
        .ok_or_else(|| AppError::Internal(anyhow::anyhow!("board revision overflow")))?;
    sqlx::query(
        "UPDATE board_collaboration_state SET revision = $2, updated_at = now() WHERE board_id = $1",
    )
    .bind(board_id)
    .bind(next)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "INSERT INTO board_operations (id, board_id, revision, actor_id, client_operation_id, operation)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(cuid2::create_id())
    .bind(board_id)
    .bind(next)
    .bind(actor_id)
    .bind(client_operation_id)
    .bind(payload)
    .execute(&mut **tx)
    .await?;
    sqlx::query("UPDATE boards SET updated_at = now() WHERE id = $1")
        .bind(board_id)
        .execute(&mut **tx)
        .await?;
    if next > OPERATION_RETENTION {
        sqlx::query("DELETE FROM board_operations WHERE board_id = $1 AND revision <= $2")
            .bind(board_id)
            .bind(next - OPERATION_RETENTION)
            .execute(&mut **tx)
            .await?;
    }
    Ok(next)
}

fn payload(
    client_operation_id: &str,
    batch: &ElementBatch,
    base_revision: Option<i64>,
    hash: Option<String>,
) -> AppResult<Value> {
    serde_json::to_value(StoredOperationPayload {
        client_operation_id: client_operation_id.to_owned(),
        batch: batch.clone(),
        base_revision,
        request_hash: hash,
    })
    .map_err(|error| AppError::Internal(anyhow::anyhow!(error)))
}

/// Applies an already validated element batch (upserts + deletes).
pub async fn apply_patch(
    db: &PgPool,
    board_id: &str,
    actor_id: &str,
    client_operation_id: &str,
    base_revision: Option<i64>,
    batch: ElementBatch,
) -> AppResult<AppliedOperation> {
    let legacy = payload(client_operation_id, &batch, None, None)?;
    let hash =
        request_hash(&json!({"kind": "patch", "batch": batch, "baseRevision": base_revision}));
    let mut tx = db.begin().await?;
    let current = lock_revision(&mut tx, board_id).await?;
    if let Some(replayed) = replayed_operation(
        &mut tx,
        board_id,
        actor_id,
        client_operation_id,
        &hash,
        &legacy,
    )
    .await?
    {
        tx.commit().await?;
        return Ok(replayed);
    }
    let touched: HashSet<&str> = batch
        .upserts
        .iter()
        .filter_map(|element| element["id"].as_str())
        .chain(batch.deleted_ids.iter().map(String::as_str))
        .collect();
    if let Some(base) = base_revision {
        check_base_revision(&mut tx, board_id, current, base, Some(&touched)).await?;
    }

    if batch
        .upserts
        .iter()
        .any(|element| element["type"] == "image")
    {
        let owner_id = board_owner(&mut tx, board_id).await?;
        let mut ids: HashSet<String> = sqlx::query_scalar(
            "SELECT element_id FROM board_elements WHERE board_id = $1 AND type = 'image'",
        )
        .bind(board_id)
        .fetch_all(&mut *tx)
        .await?
        .into_iter()
        .collect();
        for id in &batch.deleted_ids {
            ids.remove(id);
        }
        for element in &batch.upserts {
            let id = element["id"].as_str().unwrap_or_default().to_owned();
            if element["type"] == "image" {
                ids.insert(id);
            } else {
                ids.remove(&id);
            }
        }
        assert_image_count(db, &owner_id, ids.len() as i64).await?;
    }

    let stored = payload(client_operation_id, &batch, base_revision, Some(hash))?;
    let revision = record_operation(
        &mut tx,
        board_id,
        current,
        actor_id,
        &stored,
        client_operation_id,
    )
    .await?;
    for element in &batch.upserts {
        sqlx::query(
            "INSERT INTO board_elements (id, board_id, element_id, type, data)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (board_id, element_id) DO UPDATE
             SET type = EXCLUDED.type, data = EXCLUDED.data, updated_at = now()",
        )
        .bind(cuid2::create_id())
        .bind(board_id)
        .bind(element["id"].as_str())
        .bind(element["type"].as_str())
        .bind(element)
        .execute(&mut *tx)
        .await?;
    }
    if !batch.deleted_ids.is_empty() {
        sqlx::query("DELETE FROM board_elements WHERE board_id = $1 AND element_id = ANY($2)")
            .bind(board_id)
            .bind(&batch.deleted_ids)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(AppliedOperation {
        revision,
        client_operation_id: client_operation_id.to_owned(),
        actor_id: actor_id.to_owned(),
        batch,
        replayed: false,
    })
}

/// Replaces every element inside the caller's transaction after `lock_revision`.
/// `request` identifies the logical request for idempotent retries.
#[allow(clippy::too_many_arguments)]
pub async fn replace_elements(
    tx: &mut Tx<'_>,
    db: &PgPool,
    board_id: &str,
    current: i64,
    actor_id: &str,
    client_operation_id: &str,
    base_revision: Option<i64>,
    elements: Vec<Value>,
    request: &Value,
) -> AppResult<AppliedOperation> {
    let hash = request_hash(
        &json!({"kind": "replace", "request": request, "baseRevision": base_revision}),
    );
    if let Some(replayed) = replayed_operation(
        tx,
        board_id,
        actor_id,
        client_operation_id,
        &hash,
        &Value::Null,
    )
    .await?
    {
        return Ok(replayed);
    }
    if let Some(base) = base_revision {
        check_base_revision(tx, board_id, current, base, None).await?;
    }
    let mut ids = HashSet::new();
    for element in &elements {
        if !ids.insert(element["id"].as_str().unwrap_or_default().to_owned()) {
            return Err(AppError::BadRequest(
                "Document contains duplicate element IDs.".into(),
            ));
        }
    }
    let owner_id = board_owner(tx, board_id).await?;
    let images = elements
        .iter()
        .filter(|element| element["type"] == "image")
        .count();
    assert_image_count(db, &owner_id, images as i64).await?;

    let existing: Vec<String> =
        sqlx::query_scalar("SELECT element_id FROM board_elements WHERE board_id = $1")
            .bind(board_id)
            .fetch_all(&mut **tx)
            .await?;
    let batch = ElementBatch {
        deleted_ids: existing
            .into_iter()
            .filter(|id| !ids.contains(id))
            .collect(),
        upserts: elements,
    };
    let stored = payload(client_operation_id, &batch, base_revision, Some(hash))?;
    let revision = record_operation(
        tx,
        board_id,
        current,
        actor_id,
        &stored,
        client_operation_id,
    )
    .await?;
    sqlx::query("DELETE FROM board_elements WHERE board_id = $1")
        .bind(board_id)
        .execute(&mut **tx)
        .await?;
    if !batch.upserts.is_empty() {
        let element_ids: Vec<&str> = batch
            .upserts
            .iter()
            .map(|e| e["id"].as_str().unwrap_or_default())
            .collect();
        let types: Vec<&str> = batch
            .upserts
            .iter()
            .map(|e| e["type"].as_str().unwrap_or_default())
            .collect();
        let row_ids: Vec<String> = batch.upserts.iter().map(|_| cuid2::create_id()).collect();
        // Distinct microsecond offsets keep snapshot order equal to array order.
        sqlx::query(
            "INSERT INTO board_elements (id, board_id, element_id, type, data, created_at)
             SELECT r.id, $1, r.element_id, r.type, r.data, now() + (r.ord - 1) * interval '1 microsecond'
             FROM unnest($2::text[], $3::text[], $4::text[], $5::jsonb[]) WITH ORDINALITY
                  AS r(id, element_id, type, data, ord)",
        )
        .bind(board_id)
        .bind(&row_ids)
        .bind(&element_ids)
        .bind(&types)
        .bind(&batch.upserts)
        .execute(&mut **tx)
        .await?;
    }
    Ok(AppliedOperation {
        revision,
        client_operation_id: client_operation_id.to_owned(),
        actor_id: actor_id.to_owned(),
        batch,
        replayed: false,
    })
}

pub async fn save_presentation(
    db: &PgPool,
    board_id: &str,
    actor_id: &str,
    operation_id: &str,
    base: i64,
    presentation: Value,
) -> AppResult<AppliedOperation> {
    let mut tx = db.begin().await?;
    let current = lock_revision(&mut tx, board_id).await?;
    let hash = request_hash(
        &json!({"kind":"presentation","baseRevision":base,"presentation":presentation}),
    );
    if let Some(replay) = replayed_operation(
        &mut tx,
        board_id,
        actor_id,
        operation_id,
        &hash,
        &Value::Null,
    )
    .await?
    {
        tx.commit().await?;
        return Ok(replay);
    }
    check_base_revision(&mut tx, board_id, current, base, None).await?;
    let batch = ElementBatch {
        upserts: vec![],
        deleted_ids: vec![],
    };
    let stored = payload(operation_id, &batch, Some(base), Some(hash))?;
    let revision =
        record_operation(&mut tx, board_id, current, actor_id, &stored, operation_id).await?;
    sqlx::query("UPDATE boards SET presentation = $2 WHERE id = $1")
        .bind(board_id)
        .bind(presentation)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(AppliedOperation {
        revision,
        client_operation_id: operation_id.into(),
        actor_id: actor_id.into(),
        batch,
        replayed: false,
    })
}

/// Current elements in snapshot order.
pub async fn current_elements(tx: &mut Tx<'_>, board_id: &str) -> AppResult<Vec<Value>> {
    Ok(sqlx::query_scalar(
        "SELECT data FROM board_elements WHERE board_id = $1 ORDER BY created_at ASC, element_id ASC",
    )
    .bind(board_id)
    .fetch_all(&mut **tx)
    .await?)
}

/// Port of `idsToPrune`: drop the oldest auto versions first, then the oldest.
pub fn ids_to_prune(versions: &[(String, String, DateTime<Utc>)], max_keep: usize) -> Vec<String> {
    if versions.len() <= max_keep {
        return vec![];
    }
    let overflow = versions.len() - max_keep;
    let mut oldest_first: Vec<_> = versions.iter().collect();
    oldest_first.sort_by_key(|(_, _, created_at)| *created_at);
    let mut delete: Vec<String> = oldest_first
        .iter()
        .filter(|(_, source, _)| source == "auto")
        .take(overflow)
        .map(|(id, _, _)| id.clone())
        .collect();
    for (id, _, _) in oldest_first {
        if delete.len() >= overflow {
            break;
        }
        if !delete.contains(id) {
            delete.push(id.clone());
        }
    }
    delete
}

/// Port of `snapshotBoard`: stores current elements (no-op for an empty board)
/// and prunes to the owner's plan cap. Returns the new version ID.
pub async fn snapshot_board(
    tx: &mut Tx<'_>,
    db: &PgPool,
    board_id: &str,
    created_by_id: &str,
    source: &str,
    label: Option<&str>,
) -> AppResult<Option<String>> {
    let elements = current_elements(tx, board_id).await?;
    if elements.is_empty() {
        return Ok(None);
    }
    let version_id = cuid2::create_id();
    sqlx::query(
        "INSERT INTO board_versions (id, board_id, created_by_id, label, source, element_count, elements, presentation, created_at)
         SELECT $1, $2, $3, $4, $5, $6, $7, presentation, clock_timestamp() FROM boards WHERE id = $2",
    )
    .bind(&version_id)
    .bind(board_id)
    .bind(created_by_id)
    .bind(label.filter(|label| !label.is_empty()))
    .bind(source)
    .bind(elements.len() as i32)
    .bind(Value::Array(elements))
    .execute(&mut **tx)
    .await?;
    let owner_id = board_owner(tx, board_id).await?;
    let cap = version_cap(owner_is_pro(db, &owner_id).await?) as usize;
    let versions: Vec<(String, String, DateTime<Utc>)> =
        sqlx::query_as("SELECT id, source, created_at FROM board_versions WHERE board_id = $1")
            .bind(board_id)
            .fetch_all(&mut **tx)
            .await?;
    let prune = ids_to_prune(&versions, cap);
    if !prune.is_empty() {
        sqlx::query("DELETE FROM board_versions WHERE id = ANY($1)")
            .bind(&prune)
            .execute(&mut **tx)
            .await?;
    }
    Ok(Some(version_id))
}

/// Port of `maybeCreateAutoVersion`, run after a committed edit. Failures are
/// logged and never fail the edit that triggered them.
pub async fn maybe_create_auto_version(db: &PgPool, board_id: &str, actor_id: &str) {
    let result: AppResult<()> = async {
        let due: bool = sqlx::query_scalar(
            "SELECT NOT EXISTS (SELECT 1 FROM board_versions WHERE board_id = $1 AND source = 'auto'
             AND created_at > now() - make_interval(secs => $2))",
        )
        .bind(board_id)
        .bind(AUTO_VERSION_INTERVAL_SECS as f64)
        .fetch_one(db)
        .await?;
        if !due {
            return Ok(());
        }
        let mut tx = db.begin().await?;
        lock_revision(&mut tx, board_id).await?;
        // Re-check under the board writer lock so concurrent editors create one version.
        let still_due: bool = sqlx::query_scalar(
            "SELECT NOT EXISTS (SELECT 1 FROM board_versions WHERE board_id = $1 AND source = 'auto'
             AND created_at > now() - make_interval(secs => $2))",
        )
        .bind(board_id)
        .bind(AUTO_VERSION_INTERVAL_SECS as f64)
        .fetch_one(&mut *tx)
        .await?;
        if still_due {
            snapshot_board(&mut tx, db, board_id, actor_id, "auto", None).await?;
        }
        tx.commit().await?;
        Ok(())
    }
    .await;
    if let Err(error) = result {
        tracing::warn!(board_id, %error, "failed to create automatic board version");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prunes_oldest_auto_versions_first() {
        let at = |s: i64| DateTime::<Utc>::from_timestamp(s, 0).unwrap();
        let versions = vec![
            ("m1".to_string(), "manual".to_string(), at(1)),
            ("a1".to_string(), "auto".to_string(), at(2)),
            ("m2".to_string(), "manual".to_string(), at(3)),
            ("a2".to_string(), "auto".to_string(), at(4)),
        ];
        assert!(ids_to_prune(&versions, 4).is_empty());
        assert_eq!(ids_to_prune(&versions, 3), vec!["a1"]);
        assert_eq!(ids_to_prune(&versions, 1), vec!["a1", "a2", "m1"]);
    }

    #[test]
    fn validates_client_operation_ids() {
        assert!(validate_client_operation_id("op_1-a").is_ok());
        for invalid in ["", "has space", "ü", &"x".repeat(129)] {
            assert!(validate_client_operation_id(invalid).is_err());
        }
    }
}
