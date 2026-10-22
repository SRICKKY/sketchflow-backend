use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, State,
    },
    http::{header::ORIGIN, HeaderMap},
    response::Response,
    Json,
};
use futures_util::StreamExt;
use redis::{aio::ConnectionManager, AsyncCommands, Client as RedisClient};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{broadcast, Mutex};
use tokio::time::{Duration, Instant};
use tower_cookies::Cookies;

use crate::{
    auth::session,
    authz::{self, BoardRole},
    error::{AppError, AppResult},
    models::User,
    state::AppState,
};

const MAX_MESSAGE_BYTES: usize = 1_048_576;
const MAX_BATCH_ELEMENTS: usize = 500;
const MAX_SELECTION_SIZE: usize = 100;
const ROOM_CAPACITY: usize = 256;
const MAX_MESSAGES_PER_SECOND: usize = 60;
const OPERATION_RETENTION: i64 = 1_000;
const PROTOCOL_VERSION: u64 = 1;
const REDIS_CHANNEL: &str = "sketchflow:collaboration:v1";
const ALLOWED_ELEMENT_TYPES: [&str; 11] = [
    "rectangle",
    "ellipse",
    "diamond",
    "line",
    "arrow",
    "freehand",
    "text",
    "image",
    "note",
    "frame",
    "embed",
];

#[derive(Clone, Default)]
pub struct CollaborationHub {
    rooms: Arc<Mutex<HashMap<String, broadcast::Sender<String>>>>,
    redis_publisher: Option<ConnectionManager>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RedisEvent {
    board_id: String,
    message: String,
}

impl CollaborationHub {
    pub async fn with_redis(redis_url: &str) -> redis::RedisResult<Self> {
        let client = RedisClient::open(redis_url)?;
        let publisher = ConnectionManager::new(client.clone()).await?;
        let mut subscriber = client.get_async_pubsub().await?;
        subscriber.subscribe(REDIS_CHANNEL).await?;

        let hub = Self {
            rooms: Arc::new(Mutex::new(HashMap::new())),
            redis_publisher: Some(publisher),
        };
        tokio::spawn(run_redis_subscriber(client, subscriber, hub.clone()));
        Ok(hub)
    }

    async fn subscribe(
        &self,
        board_id: &str,
    ) -> (broadcast::Sender<String>, broadcast::Receiver<String>) {
        let mut rooms = self.rooms.lock().await;
        let sender = rooms
            .entry(board_id.to_string())
            .or_insert_with(|| broadcast::channel(ROOM_CAPACITY).0)
            .clone();
        let receiver = sender.subscribe();
        (sender, receiver)
    }

    async fn remove_if_empty(&self, board_id: &str, sender: &broadcast::Sender<String>) {
        let mut rooms = self.rooms.lock().await;
        if sender.receiver_count() == 0 {
            rooms.remove(board_id);
        }
    }

    async fn publish(&self, board_id: &str, message: String) -> redis::RedisResult<()> {
        let Some(publisher) = &self.redis_publisher else {
            return Ok(());
        };
        let event = serde_json::to_string(&RedisEvent {
            board_id: board_id.to_string(),
            message,
        })
        .expect("Redis collaboration event is serializable");
        let mut connection = publisher.clone();
        connection.publish(REDIS_CHANNEL, event).await
    }

    async fn publish_local(&self, board_id: &str, message: String) {
        if let Some(sender) = self.rooms.lock().await.get(board_id).cloned() {
            let _ = sender.send(message);
        }
    }

    async fn require_local_resync(&self) {
        let rooms = self.rooms.lock().await;
        let message = serialize_server_message(&ServerMessage::ResyncRequired);
        for sender in rooms.values() {
            let _ = sender.send(message.clone());
        }
    }
}

async fn run_redis_subscriber(
    client: RedisClient,
    mut subscriber: redis::aio::PubSub,
    hub: CollaborationHub,
) {
    let mut retry_delay = Duration::from_millis(500);
    loop {
        let mut messages = subscriber.on_message();
        while let Some(message) = messages.next().await {
            match message.get_payload::<String>() {
                Ok(payload) => match serde_json::from_str::<RedisEvent>(&payload) {
                    Ok(event) => hub.publish_local(&event.board_id, event.message).await,
                    Err(error) => {
                        tracing::warn!(%error, "ignoring malformed Redis collaboration event")
                    }
                },
                Err(error) => {
                    tracing::warn!(%error, "ignoring invalid Redis collaboration payload")
                }
            }
        }
        drop(messages);

        tracing::warn!(
            retry_delay_ms = retry_delay.as_millis(),
            "Redis collaboration subscription disconnected; reconnecting"
        );
        tokio::time::sleep(retry_delay).await;
        match client.get_async_pubsub().await {
            Ok(mut next_subscriber) => match next_subscriber.subscribe(REDIS_CHANNEL).await {
                Ok(()) => {
                    subscriber = next_subscriber;
                    retry_delay = Duration::from_millis(500);
                    tracing::info!("Redis collaboration subscription reconnected");
                    hub.require_local_resync().await;
                }
                Err(error) => {
                    tracing::warn!(%error, "failed to resubscribe to Redis collaboration channel");
                    retry_delay = (retry_delay * 2).min(Duration::from_secs(15));
                }
            },
            Err(error) => {
                tracing::warn!(%error, "failed to reconnect Redis collaboration subscriber");
                retry_delay = (retry_delay * 2).min(Duration::from_secs(15));
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ElementBatch {
    pub upserts: Vec<Value>,
    pub deleted_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientMessage {
    ApplyOperation {
        #[serde(rename = "clientOperationId")]
        client_operation_id: String,
        batch: ElementBatch,
    },
    Presence {
        cursor: Option<Point>,
        #[serde(default, rename = "selectedIds")]
        selected_ids: Vec<String>,
    },
    Ping,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Point {
    x: f64,
    y: f64,
}

#[derive(Debug, Serialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
enum ServerMessage {
    Connected {
        board_id: String,
        connection_id: String,
        role: String,
    },
    Snapshot {
        board_id: String,
        revision: i64,
        elements: Vec<Value>,
    },
    OperationApplied {
        revision: i64,
        client_operation_id: String,
        actor_id: String,
        batch: ElementBatch,
    },
    PresenceJoined {
        connection_id: String,
        user_id: String,
        name: Option<String>,
        image: Option<String>,
    },
    PresenceUpdated {
        connection_id: String,
        user_id: String,
        cursor: Option<Point>,
        selected_ids: Vec<String>,
    },
    PresenceLeft {
        connection_id: String,
        user_id: String,
    },
    Pong,
    Error {
        message: String,
    },
    ResyncRequired,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotResponse {
    pub board_id: String,
    pub revision: i64,
    pub elements: Vec<Value>,
}

#[derive(Debug, sqlx::FromRow)]
struct StoredOperation {
    revision: i64,
    operation: Value,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredOperationPayload {
    client_operation_id: String,
    batch: ElementBatch,
}

#[derive(Debug, Clone)]
struct AppliedOperation {
    revision: i64,
    client_operation_id: String,
    actor_id: String,
    batch: ElementBatch,
}

#[derive(Debug, sqlx::FromRow)]
struct ElementRow {
    data: Value,
}

/// Fetches a board snapshot and its collaboration revision.
#[utoipa::path(
    get,
    path = "/api/boards/{boardId}/snapshot",
    tag = "collaboration",
    params(("boardId" = String, Path, description = "Board ID")),
    responses(
        (status = 200, description = "Current board snapshot", body = SnapshotResponse),
        (status = 401, description = "Not signed in"),
        (status = 403, description = "Insufficient board role"),
        (status = 404, description = "Board not found")
    )
)]
pub async fn snapshot(
    State(state): State<AppState>,
    Path(board_id): Path<String>,
    cookies: Cookies,
) -> AppResult<Json<SnapshotResponse>> {
    let user = session::require_current_user(&cookies, &state).await?;
    authz::require_board_role(&state.db, &board_id, &user.id, BoardRole::Viewer).await?;
    let result = load_snapshot(&state, &board_id).await?;
    Ok(Json(result))
}

/// Opens an authenticated, board-scoped WebSocket for document operations and presence.
pub async fn websocket(
    State(state): State<AppState>,
    Path(board_id): Path<String>,
    cookies: Cookies,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> AppResult<Response> {
    validate_origin(&state, &headers)?;
    let user = session::require_current_user(&cookies, &state).await?;
    let access =
        authz::require_board_role(&state.db, &board_id, &user.id, BoardRole::Viewer).await?;
    Ok(ws
        .max_message_size(MAX_MESSAGE_BYTES)
        .max_frame_size(MAX_MESSAGE_BYTES)
        .on_upgrade(move |socket| handle_socket(socket, state, board_id, user, access.role)))
}

fn validate_origin(state: &AppState, headers: &HeaderMap) -> AppResult<()> {
    let origin = headers
        .get(ORIGIN)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| AppError::Forbidden("WebSocket origin is not allowed.".to_string()))?;
    if state
        .config
        .cors_allowed_origins
        .iter()
        .any(|allowed| allowed == origin)
    {
        Ok(())
    } else {
        Err(AppError::Forbidden(
            "WebSocket origin is not allowed.".to_string(),
        ))
    }
}

async fn handle_socket(
    mut socket: WebSocket,
    state: AppState,
    board_id: String,
    user: User,
    role: BoardRole,
) {
    let connection_id = cuid2::create_id();
    let (room_tx, mut room_rx) = state.collaboration.subscribe(&board_id).await;

    let snapshot = match load_snapshot(&state, &board_id).await {
        Ok(snapshot) => snapshot,
        Err(error) => {
            let _ = send_message(
                &mut socket,
                &ServerMessage::Error {
                    message: error.to_string(),
                },
            )
            .await;
            drop(room_rx);
            state
                .collaboration
                .remove_if_empty(&board_id, &room_tx)
                .await;
            return;
        }
    };

    let connected = ServerMessage::Connected {
        board_id: board_id.clone(),
        connection_id: connection_id.clone(),
        role: role.as_str().to_string(),
    };
    if send_message(&mut socket, &connected).await.is_err()
        || send_message(
            &mut socket,
            &ServerMessage::Snapshot {
                board_id: board_id.clone(),
                revision: snapshot.revision,
                elements: snapshot.elements,
            },
        )
        .await
        .is_err()
    {
        drop(room_rx);
        state
            .collaboration
            .remove_if_empty(&board_id, &room_tx)
            .await;
        return;
    }

    publish_message(
        &state.collaboration,
        &board_id,
        &room_tx,
        &ServerMessage::PresenceJoined {
            connection_id: connection_id.clone(),
            user_id: user.id.clone(),
            name: user.name.clone(),
            image: user.image.clone(),
        },
    )
    .await;

    let mut rate_window = Instant::now();
    let mut messages_in_window = 0;
    let mut authorization_check = tokio::time::interval(Duration::from_secs(30));
    authorization_check.tick().await;

    loop {
        tokio::select! {
            incoming = socket.recv() => {
                let Some(Ok(message)) = incoming else {
                    break;
                };
                match message {
                    Message::Text(text) => {
                        let now = Instant::now();
                        if now.duration_since(rate_window) >= Duration::from_secs(1) {
                            rate_window = now;
                            messages_in_window = 0;
                        }
                        messages_in_window += 1;
                        if messages_in_window > MAX_MESSAGES_PER_SECOND {
                            let _ = send_message(&mut socket, &ServerMessage::Error {
                                message: "Too many collaboration messages.".to_string(),
                            }).await;
                            break;
                        }

                        let envelope = match serde_json::from_str::<Value>(text.as_str()) {
                            Ok(value) => value,
                            Err(_) => {
                                if send_message(&mut socket, &ServerMessage::Error {
                                    message: "Invalid collaboration message.".to_string(),
                                }).await.is_err() {
                                    break;
                                }
                                continue;
                            }
                        };
                        if envelope
                            .get("protocolVersion")
                            .and_then(Value::as_u64)
                            != Some(PROTOCOL_VERSION)
                        {
                            if send_message(&mut socket, &ServerMessage::Error {
                                message: "Unsupported collaboration protocol version.".to_string(),
                            }).await.is_err() {
                                break;
                            }
                            continue;
                        }
                        let client_message = match serde_json::from_value::<ClientMessage>(envelope) {
                            Ok(message) => message,
                            Err(_) => {
                                if send_message(&mut socket, &ServerMessage::Error {
                                    message: "Invalid collaboration message.".to_string(),
                                }).await.is_err() {
                                    break;
                                }
                                continue;
                            }
                        };
                        match client_message {
                            ClientMessage::ApplyOperation { client_operation_id, batch } => {
                                if role < BoardRole::Editor {
                                    if send_message(&mut socket, &ServerMessage::Error {
                                        message: "You do not have permission to edit this board.".to_string(),
                                    }).await.is_err() {
                                        break;
                                    }
                                    continue;
                                }
                                let operation = match validate_operation(&client_operation_id, batch)
                                    .map(|batch| (batch, client_operation_id))
                                {
                                    Ok((batch, operation_id)) => {
                                        match authz::require_board_role(
                                            &state.db,
                                            &board_id,
                                            &user.id,
                                            BoardRole::Editor,
                                        )
                                        .await
                                        {
                                            Ok(_) => persist_operation(
                                                &state,
                                                &board_id,
                                                &user.id,
                                                &operation_id,
                                                batch,
                                            )
                                            .await,
                                            Err(error) => Err(error),
                                        }
                                    }
                                    Err(error) => Err(error),
                                };
                                match operation {
                                    Ok(operation) => publish_message(
                                        &state.collaboration,
                                        &board_id,
                                        &room_tx,
                                        &ServerMessage::OperationApplied {
                                            revision: operation.revision,
                                            client_operation_id: operation.client_operation_id,
                                            actor_id: operation.actor_id,
                                            batch: operation.batch,
                                        },
                                    ).await,
                                    Err(error) => {
                                        if send_message(&mut socket, &ServerMessage::Error {
                                            message: error.to_string(),
                                        }).await.is_err() {
                                            break;
                                        }
                                    }
                                }
                            }
                            ClientMessage::Presence { cursor, selected_ids } => {
                                match validate_presence(cursor, selected_ids) {
                                    Ok((cursor, selected_ids)) => publish_message(
                                        &state.collaboration,
                                        &board_id,
                                        &room_tx,
                                        &ServerMessage::PresenceUpdated {
                                            connection_id: connection_id.clone(),
                                            user_id: user.id.clone(),
                                            cursor,
                                            selected_ids,
                                        },
                                    ).await,
                                    Err(error) => {
                                        if send_message(&mut socket, &ServerMessage::Error {
                                            message: error.to_string(),
                                        }).await.is_err() {
                                            break;
                                        }
                                    }
                                }
                            }
                            ClientMessage::Ping => {
                                if send_message(&mut socket, &ServerMessage::Pong).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                    Message::Ping(payload) => {
                        if socket.send(Message::Pong(payload)).await.is_err() {
                            break;
                        }
                    }
                    Message::Close(_) => break,
                    Message::Binary(_) => {
                        if send_message(&mut socket, &ServerMessage::Error {
                            message: "Binary collaboration messages are not supported.".to_string(),
                        }).await.is_err() {
                            break;
                        }
                    }
                    Message::Pong(_) => {}
                }
            }
            _ = authorization_check.tick() => {
                match authz::get_board_access(&state.db, &board_id, &user.id).await {
                    Ok(Some(access)) if access.role >= BoardRole::Viewer => {}
                    Ok(_) => break,
                    Err(error) => {
                        tracing::warn!(
                            board_id,
                            user_id = %user.id,
                            error = %error,
                            "failed to refresh collaboration authorization"
                        );
                        break;
                    }
                }
            }
            outgoing = room_rx.recv() => {
                match outgoing {
                    Ok(text) => {
                        if socket.send(Message::Text(text.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let _ = send_message(&mut socket, &ServerMessage::ResyncRequired).await;
                        break;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }

    publish_message(
        &state.collaboration,
        &board_id,
        &room_tx,
        &ServerMessage::PresenceLeft {
            connection_id,
            user_id: user.id,
        },
    )
    .await;
    drop(room_rx);
    state
        .collaboration
        .remove_if_empty(&board_id, &room_tx)
        .await;
}

fn validate_operation(client_operation_id: &str, batch: ElementBatch) -> AppResult<ElementBatch> {
    if client_operation_id.is_empty()
        || client_operation_id.len() > 128
        || !client_operation_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err(AppError::BadRequest(
            "Invalid client operation ID.".to_string(),
        ));
    }
    if batch.upserts.len() > MAX_BATCH_ELEMENTS || batch.deleted_ids.len() > MAX_BATCH_ELEMENTS {
        return Err(AppError::BadRequest(
            "Operation contains too many elements.".to_string(),
        ));
    }
    if batch.upserts.is_empty() && batch.deleted_ids.is_empty() {
        return Err(AppError::BadRequest(
            "Operation must change at least one element.".to_string(),
        ));
    }

    let mut ids = HashSet::new();
    for element in &batch.upserts {
        let id = element
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty() && id.len() <= 128)
            .ok_or_else(|| AppError::BadRequest("Invalid element ID.".to_string()))?;
        let element_type = element
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| AppError::BadRequest("Invalid element type.".to_string()))?;
        if !ALLOWED_ELEMENT_TYPES.contains(&element_type) {
            return Err(AppError::BadRequest(
                "Unsupported canvas element type.".to_string(),
            ));
        }
        if !ids.insert(id.to_string()) {
            return Err(AppError::BadRequest(
                "Operation contains duplicate element IDs.".to_string(),
            ));
        }
    }
    for id in &batch.deleted_ids {
        if id.is_empty() || id.len() > 128 || !ids.insert(id.clone()) {
            return Err(AppError::BadRequest(
                "Operation contains an invalid or duplicate element ID.".to_string(),
            ));
        }
    }
    Ok(batch)
}

fn validate_presence(
    cursor: Option<Point>,
    selected_ids: Vec<String>,
) -> AppResult<(Option<Point>, Vec<String>)> {
    if selected_ids.len() > MAX_SELECTION_SIZE
        || selected_ids
            .iter()
            .any(|id| id.is_empty() || id.len() > 128)
    {
        return Err(AppError::BadRequest(
            "Invalid presence selection.".to_string(),
        ));
    }
    if cursor
        .as_ref()
        .is_some_and(|point| !point.x.is_finite() || !point.y.is_finite())
    {
        return Err(AppError::BadRequest("Invalid cursor position.".to_string()));
    }
    Ok((cursor, selected_ids))
}

async fn load_snapshot(state: &AppState, board_id: &str) -> AppResult<SnapshotResponse> {
    sqlx::query(
        r#"INSERT INTO board_collaboration_state (board_id, revision)
           SELECT id, 0 FROM boards WHERE id = $1
           ON CONFLICT (board_id) DO NOTHING"#,
    )
    .bind(board_id)
    .execute(&state.db)
    .await?;

    let mut tx = state.db.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .execute(&mut *tx)
        .await?;
    let revision = sqlx::query_scalar::<_, i64>(
        "SELECT revision FROM board_collaboration_state WHERE board_id = $1",
    )
    .bind(board_id)
    .fetch_one(&mut *tx)
    .await?;
    let rows = sqlx::query_as::<_, ElementRow>(
        "SELECT data FROM board_elements WHERE board_id = $1 ORDER BY created_at ASC, element_id ASC",
    )
    .bind(board_id)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;

    Ok(SnapshotResponse {
        board_id: board_id.to_string(),
        revision,
        elements: rows.into_iter().map(|row| row.data).collect(),
    })
}

async fn persist_operation(
    state: &AppState,
    board_id: &str,
    actor_id: &str,
    client_operation_id: &str,
    batch: ElementBatch,
) -> AppResult<AppliedOperation> {
    let payload = StoredOperationPayload {
        client_operation_id: client_operation_id.to_string(),
        batch: batch.clone(),
    };
    let payload_json = serde_json::to_value(&payload)
        .map_err(|error| AppError::Internal(anyhow::anyhow!(error)))?;
    let mut tx = state.db.begin().await?;
    sqlx::query(
        "INSERT INTO board_collaboration_state (board_id, revision) VALUES ($1, 0) ON CONFLICT (board_id) DO NOTHING",
    )
    .bind(board_id)
    .execute(&mut *tx)
    .await?;
    let revision = sqlx::query_scalar::<_, i64>(
        "SELECT revision FROM board_collaboration_state WHERE board_id = $1 FOR UPDATE",
    )
    .bind(board_id)
    .fetch_one(&mut *tx)
    .await?;

    if let Some(existing) = sqlx::query_as::<_, StoredOperation>(
        r#"SELECT revision, operation FROM board_operations
           WHERE board_id = $1 AND actor_id = $2 AND client_operation_id = $3"#,
    )
    .bind(board_id)
    .bind(actor_id)
    .bind(client_operation_id)
    .fetch_optional(&mut *tx)
    .await?
    {
        if existing.operation != payload_json {
            return Err(AppError::Conflict(
                "Client operation ID was already used for a different change.".to_string(),
            ));
        }
        tx.commit().await?;
        return applied_operation(existing.revision, actor_id, existing.operation);
    }

    let next_revision = revision
        .checked_add(1)
        .ok_or_else(|| AppError::Internal(anyhow::anyhow!("board revision overflow")))?;
    sqlx::query(
        "UPDATE board_collaboration_state SET revision = $2, updated_at = now() WHERE board_id = $1",
    )
    .bind(board_id)
    .bind(next_revision)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        r#"INSERT INTO board_operations
           (id, board_id, revision, actor_id, client_operation_id, operation)
           VALUES ($1, $2, $3, $4, $5, $6)"#,
    )
    .bind(cuid2::create_id())
    .bind(board_id)
    .bind(next_revision)
    .bind(actor_id)
    .bind(client_operation_id)
    .bind(&payload_json)
    .execute(&mut *tx)
    .await?;

    for element in &batch.upserts {
        let element_id = element
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| AppError::BadRequest("Invalid element ID.".to_string()))?;
        let element_type = element
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| AppError::BadRequest("Invalid element type.".to_string()))?;
        sqlx::query(
            r#"INSERT INTO board_elements (id, board_id, element_id, type, data)
               VALUES ($1, $2, $3, $4, $5)
               ON CONFLICT (board_id, element_id) DO UPDATE
               SET type = EXCLUDED.type, data = EXCLUDED.data, updated_at = now()"#,
        )
        .bind(cuid2::create_id())
        .bind(board_id)
        .bind(element_id)
        .bind(element_type)
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
    sqlx::query("UPDATE boards SET updated_at = now() WHERE id = $1")
        .bind(board_id)
        .execute(&mut *tx)
        .await?;
    if next_revision > OPERATION_RETENTION {
        sqlx::query("DELETE FROM board_operations WHERE board_id = $1 AND revision <= $2")
            .bind(board_id)
            .bind(next_revision - OPERATION_RETENTION)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;

    Ok(AppliedOperation {
        revision: next_revision,
        client_operation_id: client_operation_id.to_string(),
        actor_id: actor_id.to_string(),
        batch,
    })
}

fn applied_operation(
    revision: i64,
    actor_id: &str,
    operation: Value,
) -> AppResult<AppliedOperation> {
    let payload: StoredOperationPayload = serde_json::from_value(operation)
        .map_err(|error| AppError::Internal(anyhow::anyhow!(error)))?;
    Ok(AppliedOperation {
        revision,
        client_operation_id: payload.client_operation_id,
        actor_id: actor_id.to_string(),
        batch: payload.batch,
    })
}

async fn send_message(socket: &mut WebSocket, message: &ServerMessage) -> Result<(), axum::Error> {
    let json = serialize_server_message(message);
    socket.send(Message::Text(json.into())).await
}

async fn publish_message(
    hub: &CollaborationHub,
    board_id: &str,
    local_sender: &broadcast::Sender<String>,
    message: &ServerMessage,
) {
    let json = serialize_server_message(message);
    if hub.redis_publisher.is_none() {
        let _ = local_sender.send(json);
        return;
    }
    if let Err(error) = hub.publish(board_id, json).await {
        tracing::error!(
            board_id,
            %error,
            "failed to publish collaboration event to Redis; local clients must resync"
        );
        let resync = serialize_server_message(&ServerMessage::ResyncRequired);
        let _ = local_sender.send(resync);
    }
}

fn serialize_server_message(message: &ServerMessage) -> String {
    let mut value = serde_json::to_value(message).expect("server message is serializable");
    value
        .as_object_mut()
        .expect("server messages serialize as objects")
        .insert("protocolVersion".to_string(), json!(PROTOCOL_VERSION));
    serde_json::to_string(&value).expect("server message is serializable")
}

impl BoardRole {
    fn as_str(self) -> &'static str {
        match self {
            BoardRole::Viewer => "viewer",
            BoardRole::Editor => "editor",
            BoardRole::Owner => "owner",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn element(id: &str, element_type: &str) -> Value {
        json!({"id": id, "type": element_type})
    }

    #[test]
    fn validates_well_formed_element_batches() {
        let batch = ElementBatch {
            upserts: vec![element("el-1", "rectangle")],
            deleted_ids: vec!["el-2".to_string()],
        };
        assert!(validate_operation("client-1", batch).is_ok());
    }

    #[test]
    fn rejects_duplicate_or_unsupported_elements() {
        let duplicate = ElementBatch {
            upserts: vec![element("same", "rectangle"), element("same", "ellipse")],
            deleted_ids: vec![],
        };
        assert!(validate_operation("client-1", duplicate).is_err());

        let unsupported = ElementBatch {
            upserts: vec![element("el-1", "script")],
            deleted_ids: vec![],
        };
        assert!(validate_operation("client-1", unsupported).is_err());
    }

    #[test]
    fn rejects_ambiguous_empty_and_invalid_operation_ids() {
        let empty = ElementBatch {
            upserts: vec![],
            deleted_ids: vec![],
        };
        assert!(validate_operation("client-1", empty.clone()).is_err());
        assert!(validate_operation("invalid id", empty).is_err());
    }

    #[test]
    fn rejects_invalid_presence_coordinates_and_oversized_selection() {
        let point = Point {
            x: f64::INFINITY,
            y: 10.0,
        };
        assert!(validate_presence(Some(point), vec![]).is_err());
        assert!(
            validate_presence(None, vec!["element".to_string(); MAX_SELECTION_SIZE + 1]).is_err()
        );
    }

    #[test]
    fn includes_protocol_version_in_server_envelopes() {
        let message = serialize_server_message(&ServerMessage::Pong);
        let envelope: Value = serde_json::from_str(&message).unwrap();
        assert_eq!(envelope["type"], "pong");
        assert_eq!(envelope["protocolVersion"], PROTOCOL_VERSION);
    }

    #[test]
    fn redis_events_keep_board_routing_separate_from_protocol_message() {
        let event = RedisEvent {
            board_id: "board-1".to_string(),
            message: serialize_server_message(&ServerMessage::Pong),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["boardId"], "board-1");
        assert_eq!(
            serde_json::from_str::<Value>(json["message"].as_str().unwrap()).unwrap()["type"],
            "pong"
        );
    }

    #[test]
    fn redis_client_accepts_tls_urls_without_connecting() {
        assert!(RedisClient::open("rediss://example.invalid:6379").is_ok());
    }
}
