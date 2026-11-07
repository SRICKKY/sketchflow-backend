use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Extension, Path, State,
    },
    http::{header::ORIGIN, HeaderMap},
    response::Response,
    Json,
};
use futures_util::{SinkExt, StreamExt};
use redis::{aio::ConnectionManager, AsyncCommands, Client as RedisClient};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc, Mutex};
use tokio::time::{Duration, Instant};
use tower_cookies::Cookies;

use crate::{
    auth::session,
    authz::{self, BoardRole},
    error::{AppError, AppResult},
    models::User,
    request_logging::RequestId,
    state::AppState,
};

// Bound connection memory, payload validation work, and operation history growth.
const MAX_MESSAGE_BYTES: usize = 1_048_576;
const MAX_BATCH_ELEMENTS: usize = 500;
const MAX_SELECTION_SIZE: usize = 100;
const ROOM_CAPACITY: usize = 256;
const MAX_MESSAGES_PER_SECOND: usize = 60;
const SNAPSHOT_CACHE_TTL_SECONDS: u64 = 30;

// Version the JSON protocol and shared Redis channel independently of the HTTP API.
const PROTOCOL_VERSION: u64 = 1;
const REDIS_CHANNEL: &str = "sketchflow:collaboration:v1";

// Canvas element types accepted from clients; unknown types cannot be persisted.
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
/// Routes collaboration events between local WebSocket rooms and optional Redis.
pub struct CollaborationHub {
    // Local socket groups receive events from the current process.
    rooms: Arc<Mutex<HashMap<String, broadcast::Sender<String>>>>,
    // Shared publisher is present only when Redis fan-out is configured.
    redis_publisher: Option<ConnectionManager>,
}

// Redis envelopes carry the board routing key separately from the client protocol event.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RedisEvent {
    board_id: String,
    message: String,
}

impl CollaborationHub {
    /// Connects the publisher and subscriber before serving requests, then starts the relay.
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

    /// Adds a local socket to the board's broadcast room, creating the room if needed.
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

    /// Returns the active local room sender, or a temporary sender when no
    /// socket is connected. The temporary room is deliberately not retained.
    async fn room(&self, board_id: &str) -> broadcast::Sender<String> {
        if let Some(sender) = self.rooms.lock().await.get(board_id).cloned() {
            return sender;
        }
        broadcast::channel(ROOM_CAPACITY).0
    }

    /// Releases a room from memory after its final local socket disconnects.
    async fn remove_if_empty(&self, board_id: &str, sender: &broadcast::Sender<String>) {
        let mut rooms = self.rooms.lock().await;
        if sender.receiver_count() == 0 {
            rooms.remove(board_id);
        }
    }

    /// Publishes a board-scoped event so every backend instance can relay it to local sockets.
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

    /// Sends a received Redis event only to sockets connected to this board on this instance.
    async fn publish_local(&self, board_id: &str, message: String) {
        if let Some(sender) = self.rooms.lock().await.get(board_id).cloned() {
            let _ = sender.send(message);
        }
    }

    /// Notifies connected clients on every instance that a user's board access was revoked.
    pub async fn revoke_board_access(&self, board_id: &str, user_id: &str) {
        let message = serialize_server_message(&ServerMessage::AccessRevoked {
            user_id: user_id.to_string(),
            message: "Your permission to view this canvas has been revoked.".to_string(),
        });
        let room = self.room(board_id).await;
        if self.redis_publisher.is_none() {
            let _ = room.send(message);
        } else if let Err(error) = self.publish(board_id, message.clone()).await {
            tracing::error!(
                subsystem = "websocket",
                event = "redis_access_revocation_publish_failed",
                board_id,
                %error,
                "Failed to publish board access revocation"
            );
            let _ = room.send(message);
        }
    }

    /// Tells local clients to reload a snapshot after this instance may have missed events.
    async fn require_local_resync(&self) {
        let rooms = self.rooms.lock().await;
        let message = serialize_server_message(&ServerMessage::ResyncRequired);
        for sender in rooms.values() {
            let _ = sender.send(message.clone());
        }
    }
}

/// Subscribes to shared collaboration events and reconnects with bounded backoff on failure.
#[tracing::instrument(
    skip_all,
    fields(
        service = "sketchflow-api",
        environment = %crate::request_logging::app_environment(),
        subsystem = "websocket",
        component = "redis_subscriber"
    )
)]
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
                    Err(error) => tracing::warn!(
                        event = "redis_event_malformed",
                        %error,
                        "Ignoring malformed Redis collaboration event"
                    ),
                },
                Err(error) => tracing::warn!(
                    event = "redis_payload_invalid",
                    %error,
                    "Ignoring invalid Redis collaboration payload"
                ),
            }
        }
        drop(messages);

        tracing::warn!(
            event = "redis_subscription_disconnected",
            retry_delay_ms = retry_delay.as_millis(),
            "Redis collaboration subscription disconnected; reconnecting"
        );
        tokio::time::sleep(retry_delay).await;
        match client.get_async_pubsub().await {
            Ok(mut next_subscriber) => match next_subscriber.subscribe(REDIS_CHANNEL).await {
                Ok(()) => {
                    subscriber = next_subscriber;
                    retry_delay = Duration::from_millis(500);
                    tracing::info!(
                        event = "redis_subscription_reconnected",
                        "Redis collaboration subscription reconnected"
                    );
                    hub.require_local_resync().await;
                }
                Err(error) => {
                    tracing::warn!(
                        event = "redis_resubscribe_failed",
                        %error,
                        "Failed to resubscribe to Redis collaboration channel"
                    );
                    retry_delay = (retry_delay * 2).min(Duration::from_secs(15));
                }
            },
            Err(error) => {
                tracing::warn!(
                    event = "redis_reconnect_failed",
                    %error,
                    "Failed to reconnect Redis collaboration subscriber"
                );
                retry_delay = (retry_delay * 2).min(Duration::from_secs(15));
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
/// The canvas changes carried by an operation: full replacements and element deletions.
pub struct ElementBatch {
    pub upserts: Vec<Value>,
    pub deleted_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
/// Client requests accepted by the WebSocket protocol.
enum ClientMessage {
    /// Applies one revisioned set of canvas changes.
    ApplyOperation {
        #[serde(rename = "clientOperationId")]
        client_operation_id: String,
        #[serde(default, rename = "baseRevision")]
        base_revision: Option<i64>,
        batch: ElementBatch,
    },
    /// Shares transient cursor and selection state with other board participants.
    Presence {
        cursor: Option<Point>,
        #[serde(default, rename = "selectedIds")]
        selected_ids: Vec<String>,
    },
    /// Broadcasts a temporary canvas state without persisting it.
    Preview { sequence: u64, batch: ElementBatch },
    /// Answers the application-level heartbeat without changing board state.
    Ping,
}

/// A cursor location in canvas/world coordinates.
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
/// Events sent by the server to collaboration clients.
enum ServerMessage {
    /// Identifies this connection and its permissions before the initial snapshot.
    Connected {
        board_id: String,
        connection_id: String,
        role: String,
    },
    /// Supplies the current persisted document and revision for initial sync.
    Snapshot {
        board_id: String,
        revision: i64,
        elements: Vec<Value>,
    },
    /// Confirms a committed operation and broadcasts its assigned revision.
    OperationApplied {
        revision: i64,
        client_operation_id: String,
        actor_id: String,
        batch: ElementBatch,
    },
    /// Relays a temporary canvas state to peers without changing the document.
    PreviewUpdated {
        connection_id: String,
        user_id: String,
        sequence: u64,
        batch: ElementBatch,
    },
    /// Announces a newly connected participant.
    PresenceJoined {
        connection_id: String,
        user_id: String,
        name: Option<String>,
        image: Option<String>,
    },
    /// Updates a participant's ephemeral cursor and selection.
    PresenceUpdated {
        connection_id: String,
        user_id: String,
        cursor: Option<Point>,
        selected_ids: Vec<String>,
    },
    /// Removes a participant from the live presence list.
    PresenceLeft {
        connection_id: String,
        user_id: String,
    },
    /// Removes a participant from a board immediately after access is revoked.
    AccessRevoked { user_id: String, message: String },
    /// Response to a client heartbeat.
    Pong,
    /// Reports a rejected request or protocol problem.
    Error { message: String },
    /// Indicates that the client must fetch a fresh snapshot before continuing.
    ResyncRequired,
}

/// REST response body containing a consistent board snapshot and its revision.
#[derive(Debug, Deserialize, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotResponse {
    pub board_id: String,
    pub revision: i64,
    pub elements: Vec<Value>,
}

/// Minimal database projection for a canvas element in a snapshot.
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
    // Viewer permission is enough to read a document, but never to submit edits.
    let user = session::require_current_user(&cookies, &state).await?;
    authz::require_board_role(&state.db, &board_id, &user.id, BoardRole::Viewer).await?;

    let revision = sqlx::query_scalar::<_, i64>(
        "SELECT revision FROM board_collaboration_state WHERE board_id = $1",
    )
    .bind(&board_id)
    .fetch_optional(&state.db)
    .await?;
    if let Some(revision) = revision {
        let cache_key = snapshot_cache_key(&board_id, revision);
        match state.cache.get_json::<SnapshotResponse>(&cache_key).await {
            Ok(Some(snapshot))
                if snapshot.board_id == board_id && snapshot.revision == revision =>
            {
                return Ok(Json(snapshot));
            }
            Ok(Some(_)) => tracing::warn!(
                subsystem = "cache",
                event = "redis_cache_value_mismatch",
                cache = "board_snapshot",
                "Ignoring snapshot cache entry with mismatched revision"
            ),
            Ok(None) => {}
            Err(error) => tracing::warn!(
                subsystem = "cache",
                event = "redis_cache_read_failed",
                cache = "board_snapshot",
                %error,
                "Falling back to PostgreSQL for board snapshot"
            ),
        }
    }

    let result = load_snapshot(&state, &board_id).await?;
    let cache_key = snapshot_cache_key(&board_id, result.revision);
    if let Err(error) = state
        .cache
        .set_json(
            &cache_key,
            &result,
            Duration::from_secs(SNAPSHOT_CACHE_TTL_SECONDS),
        )
        .await
    {
        tracing::warn!(
            subsystem = "cache",
            event = "redis_cache_write_failed",
            cache = "board_snapshot",
            %error,
            "Board snapshot was not cached"
        );
    }
    Ok(Json(result))
}

fn snapshot_cache_key(board_id: &str, revision: i64) -> String {
    format!(
        "sketchflow:api-cache:v1:{}:board-snapshot:{board_id}:{revision}",
        crate::request_logging::app_environment()
    )
}

/// Opens an authenticated, board-scoped WebSocket for document operations and presence.
pub async fn websocket(
    State(state): State<AppState>,
    Path(board_id): Path<String>,
    cookies: Cookies,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
    request_id: Option<Extension<RequestId>>,
) -> AppResult<Response> {
    // WebSockets do not receive browser CORS enforcement, so validate Origin explicitly.
    validate_origin(&state, &headers)?;
    let user = session::require_current_user(&cookies, &state).await?;
    let access =
        authz::require_board_role(&state.db, &board_id, &user.id, BoardRole::Viewer).await?;
    let request_id = request_id
        .map(|Extension(request_id)| request_id.0)
        .unwrap_or_else(crate::request_logging::generate_request_id);
    Ok(ws
        .max_message_size(MAX_MESSAGE_BYTES)
        .max_frame_size(MAX_MESSAGE_BYTES)
        .on_upgrade(move |socket| {
            handle_socket(socket, state, board_id, user, access.role, request_id)
        }))
}

/// Rejects browser WebSocket handshakes from origins outside the configured allowlist.
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

#[tracing::instrument(
    skip_all,
    fields(
        service = "sketchflow-api",
        environment = %crate::request_logging::app_environment(),
        request_id = %request_id,
        subsystem = "websocket",
        board_id = %board_id,
        user_id = %user.id,
        connection_id = tracing::field::Empty
    )
)]
async fn handle_socket(
    mut socket: WebSocket,
    state: AppState,
    board_id: String,
    user: User,
    role: BoardRole,
    request_id: String,
) {
    let connection_started = Instant::now();
    let connection_id = cuid2::create_id();
    tracing::Span::current().record("connection_id", tracing::field::display(&connection_id));
    tracing::info!(event = "connection_started", "WebSocket connection started");
    // Subscribe before reading the snapshot so updates arriving during sync are buffered.
    let (room_tx, mut room_rx) = state.collaboration.subscribe(&board_id).await;

    let snapshot = match load_snapshot(&state, &board_id).await {
        Ok(snapshot) => snapshot,
        Err(error) => {
            tracing::warn!(
                event = "connection_failed",
                reason = "snapshot_failed",
                %error,
                "WebSocket connection setup failed"
            );
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
    // Send connection metadata and the initial document before announcing presence.
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
        tracing::warn!(
            event = "connection_failed",
            reason = "initial_sync_failed",
            "WebSocket initial synchronization failed"
        );
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
    tracing::info!(event = "connection_established", "WebSocket connected");

    let (mut socket_sender, mut socket_receiver) = socket.split();
    let (outgoing_tx, mut outgoing_rx) = mpsc::channel::<Message>(64);
    let writer_task = tokio::spawn(async move {
        while let Some(message) = outgoing_rx.recv().await {
            if socket_sender.send(message).await.is_err() {
                break;
            }
        }
    });

    let mut rate_window = Instant::now();
    let mut messages_in_window = 0;
    let mut last_activity = Instant::now();
    let mut heartbeat_check = tokio::time::interval(Duration::from_secs(15));
    heartbeat_check.tick().await;
    // Refresh membership so removed or downgraded users lose access on open sockets.
    let mut authorization_check = tokio::time::interval(Duration::from_secs(30));
    authorization_check.tick().await;

    let mut disconnect_reason = "socket_ended";
    loop {
        tokio::select! {
            incoming = socket_receiver.next() => {
                let Some(Ok(message)) = incoming else {
                    disconnect_reason = "socket_closed_or_receive_failed";
                    break;
                };
                last_activity = Instant::now();
                match message {
                    Message::Text(text) => {
                        let now = Instant::now();
                        if now.duration_since(rate_window) >= Duration::from_secs(1) {
                            rate_window = now;
                            messages_in_window = 0;
                        }
                        messages_in_window += 1;
                        if messages_in_window > MAX_MESSAGES_PER_SECOND {
                            disconnect_reason = "rate_limited";
                            tracing::warn!(
                                event = "collaboration_rate_limited",
                                "Collaboration client exceeded its message rate"
                            );
                            let _ = queue_message(&outgoing_tx, &ServerMessage::Error {
                                message: "Too many collaboration messages.".to_string(),
                            });
                            break;
                        }

                        let envelope = match serde_json::from_str::<Value>(text.as_str()) {
                            Ok(value) => value,
                            Err(_) => {
                                tracing::warn!(
                                    event = "collaboration_message_rejected",
                                    reason = "invalid_json",
                                    payload_bytes = text.len(),
                                    "Rejected malformed collaboration message"
                                );
                                if !queue_message(&outgoing_tx, &ServerMessage::Error {
                                    message: "Invalid collaboration message.".to_string(),
                                }) {
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
                            tracing::warn!(
                                event = "collaboration_message_rejected",
                                reason = "unsupported_protocol_version",
                                "Rejected unsupported collaboration protocol version"
                            );
                            if !queue_message(&outgoing_tx, &ServerMessage::Error {
                                message: "Unsupported collaboration protocol version.".to_string(),
                            }) {
                                break;
                            }
                            continue;
                        }
                        // Decode only after the envelope version is known to be supported.
                        let client_message = match serde_json::from_value::<ClientMessage>(envelope) {
                            Ok(message) => message,
                            Err(_) => {
                                tracing::warn!(
                                    event = "collaboration_message_rejected",
                                    reason = "invalid_message_shape",
                                    "Rejected invalid collaboration message"
                                );
                                if !queue_message(&outgoing_tx, &ServerMessage::Error {
                                    message: "Invalid collaboration message.".to_string(),
                                }) {
                                    break;
                                }
                                continue;
                            }
                        };
                        let message_type = match &client_message {
                            ClientMessage::ApplyOperation { .. } => "apply_operation",
                            ClientMessage::Preview { .. } => "preview",
                            ClientMessage::Presence { .. } => "presence",
                            ClientMessage::Ping => "ping",
                        };
                        tracing::debug!(
                            event = "collaboration_event_received",
                            message_type,
                            payload_bytes = text.len(),
                            "Collaboration event received"
                        );
                        match client_message {
                            ClientMessage::ApplyOperation { client_operation_id, base_revision, batch } => {
                                // Reject viewers early, then re-check database authorization before each write.
                                if role < BoardRole::Editor {
                                    if !queue_message(&outgoing_tx, &ServerMessage::Error {
                                        message: "You do not have permission to edit this board.".to_string(),
                                    }) {
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
                                                base_revision,
                                                batch,
                                            )
                                            .await,
                                            Err(error) => Err(error),
                                        }
                                    }
                                    Err(error) => Err(error),
                                };
                                match operation {
                                    // Persist before publishing so peers never observe an uncommitted change.
                                    Ok(operation) => {
                                        tracing::info!(
                                            event = "collaboration_operation_applied",
                                            revision = operation.revision,
                                            upsert_count = operation.batch.upserts.len(),
                                            delete_count = operation.batch.deleted_ids.len(),
                                            replayed = operation.replayed,
                                            "Collaboration operation applied"
                                        );
                                        publish_message(
                                            &state.collaboration,
                                            &board_id,
                                            &room_tx,
                                            &ServerMessage::OperationApplied {
                                                revision: operation.revision,
                                                client_operation_id: operation.client_operation_id,
                                                actor_id: operation.actor_id,
                                                batch: operation.batch,
                                            },
                                        ).await
                                    }
                                    Err(error) => {
                                        tracing::warn!(
                                            event = "collaboration_operation_failed",
                                            reason = "operation_rejected_or_persistence_failed",
                                            "Collaboration operation failed"
                                        );
                                        if !queue_message(&outgoing_tx, &ServerMessage::Error {
                                            message: error.to_string(),
                                        }) {
                                            break;
                                        }
                                    }
                                }
                            }
                            ClientMessage::Preview { sequence, batch } => {
                                if role < BoardRole::Editor {
                                    let _ = queue_message(
                                        &outgoing_tx,
                                        &ServerMessage::Error {
                                            message: "You do not have permission to publish previews."
                                                .to_string(),
                                        },
                                    );
                                    break;
                                }
                                match validate_preview(batch) {
                                    Ok(batch) => publish_message(
                                        &state.collaboration,
                                        &board_id,
                                        &room_tx,
                                        &ServerMessage::PreviewUpdated {
                                            connection_id: connection_id.clone(),
                                            user_id: user.id.clone(),
                                            sequence,
                                            batch,
                                        },
                                    ).await,
                                    Err(error) => {
                                        if !queue_message(&outgoing_tx, &ServerMessage::Error {
                                            message: error.to_string(),
                                        }) {
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
                                        if !queue_message(&outgoing_tx, &ServerMessage::Error {
                                            message: error.to_string(),
                                        }) {
                                            break;
                                        }
                                    }
                                }
                            }
                            ClientMessage::Ping => {
                                if !queue_message(&outgoing_tx, &ServerMessage::Pong) {
                                    break;
                                }
                            }
                        }
                    }
                    Message::Ping(payload) => {
                        if outgoing_tx.try_send(Message::Pong(payload)).is_err() {
                            break;
                        }
                    }
                    Message::Close(_) => {
                        disconnect_reason = "client_closed";
                        break;
                    }
                    Message::Binary(_) => {
                        tracing::warn!(
                            event = "collaboration_message_rejected",
                            reason = "binary_message_unsupported",
                            "Rejected binary collaboration message"
                        );
                        if !queue_message(&outgoing_tx, &ServerMessage::Error {
                            message: "Binary collaboration messages are not supported.".to_string(),
                        }) {
                            break;
                        }
                    }
                    Message::Pong(_) => {}
                }
            }
            _ = authorization_check.tick() => {
                match authz::get_board_access(&state.db, &board_id, &user.id).await {
                    Ok(Some(access)) if access.role >= BoardRole::Viewer => {}
                    Ok(_) => {
                        disconnect_reason = "authorization_revoked";
                        tracing::info!(
                            event = "connection_authorization_revoked",
                            "Closing collaboration connection after access revocation"
                        );
                        break;
                    }
                    Err(error) => {
                        tracing::warn!(
                            event = "connection_authorization_check_failed",
                            error = %error,
                            "Failed to refresh collaboration authorization"
                        );
                        disconnect_reason = "authorization_check_failed";
                        break;
                    }
                }
            }
            _ = heartbeat_check.tick() => {
                if last_activity.elapsed() > Duration::from_secs(60) {
                    tracing::info!(
                        event = "connection_idle_timeout",
                        "Closing inactive collaboration connection"
                    );
                    disconnect_reason = "idle_timeout";
                    break;
                }
            }
            outgoing = room_rx.recv() => {
                match outgoing {
                    Ok(text) => {
                        if outgoing_tx
                            .try_send(Message::Text(text.into()))
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        // A slow socket may have missed events; disconnect it so it can resync.
                        disconnect_reason = "client_lagged";
                        tracing::warn!(
                            event = "connection_client_lagged",
                            "Disconnecting slow collaboration client"
                        );
                        let _ = queue_message(&outgoing_tx, &ServerMessage::ResyncRequired);
                        break;
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        disconnect_reason = "room_closed";
                        break;
                    }
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
    tracing::info!(
        event = "connection_disconnected",
        reason = disconnect_reason,
        duration_ms = connection_started.elapsed().as_millis() as u64,
        "WebSocket disconnected"
    );
    drop(outgoing_tx);
    writer_task.abort();
    let _ = writer_task.await;
    // Broadcast departure before releasing the room receiver and its resources.
    drop(room_rx);
    state
        .collaboration
        .remove_if_empty(&board_id, &room_tx)
        .await;
}

/// Checks operation IDs, batch limits, element types, and duplicate IDs before persistence.
pub(super) fn validate_operation(
    client_operation_id: &str,
    batch: ElementBatch,
) -> AppResult<ElementBatch> {
    // Operation IDs are constrained because they are persisted as idempotency keys.
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
    // Bound each batch and require actual content before iterating its elements.
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

    // An element may be changed or deleted only once within one batch.
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

fn validate_preview(batch: ElementBatch) -> AppResult<ElementBatch> {
    if batch.upserts.is_empty() && batch.deleted_ids.is_empty() {
        return Ok(batch);
    }
    validate_operation("preview", batch)
}

/// Checks transient cursor and selection values before they are broadcast.
fn validate_presence(
    cursor: Option<Point>,
    selected_ids: Vec<String>,
) -> AppResult<(Option<Point>, Vec<String>)> {
    // Presence is ephemeral, but still bounded and checked before being fanned out.
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

/// Reads board elements with the revision that identifies their persisted state.
async fn load_snapshot(state: &AppState, board_id: &str) -> AppResult<SnapshotResponse> {
    // Older boards may not yet have a collaboration revision row.
    sqlx::query(
        r#"INSERT INTO board_collaboration_state (board_id, revision)
           SELECT id, 0 FROM boards WHERE id = $1
           ON CONFLICT (board_id) DO NOTHING"#,
    )
    .bind(board_id)
    .execute(&state.db)
    .await?;

    let mut tx = state.db.begin().await?;
    // Read revision and elements from one database snapshot so they cannot disagree.
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

/// Applies and records one operation atomically, allocating its board revision.
async fn persist_operation(
    state: &AppState,
    board_id: &str,
    actor_id: &str,
    client_operation_id: &str,
    base_revision: Option<i64>,
    batch: ElementBatch,
) -> AppResult<crate::editor::store::AppliedOperation> {
    let operation = crate::editor::store::apply_patch(
        &state.db,
        board_id,
        actor_id,
        client_operation_id,
        base_revision,
        batch,
    )
    .await?;
    if !operation.replayed {
        crate::editor::store::maybe_create_auto_version(&state.db, board_id, actor_id).await;
    }
    Ok(operation)
}

pub async fn broadcast_editor_operation(
    hub: &CollaborationHub,
    board_id: &str,
    operation: &crate::editor::store::AppliedOperation,
    resync: bool,
) {
    if operation.replayed {
        return;
    }
    let room = hub.room(board_id).await;
    publish_message(
        hub,
        board_id,
        &room,
        &ServerMessage::OperationApplied {
            revision: operation.revision,
            client_operation_id: operation.client_operation_id.clone(),
            actor_id: operation.actor_id.clone(),
            batch: operation.batch.clone(),
        },
    )
    .await;
    if resync {
        publish_message(hub, board_id, &room, &ServerMessage::ResyncRequired).await;
    }
}

/// Serializes and sends one protocol-versioned message to a WebSocket.
async fn send_message(socket: &mut WebSocket, message: &ServerMessage) -> Result<(), axum::Error> {
    let json = serialize_server_message(message);
    socket.send(Message::Text(json.into())).await
}

/// Queues a protocol message without blocking the connection's read loop.
fn queue_message(sender: &mpsc::Sender<Message>, message: &ServerMessage) -> bool {
    sender
        .try_send(Message::Text(serialize_server_message(message).into()))
        .is_ok()
}

/// Fans an event out locally or through Redis, asking local clients to resync on failure.
async fn publish_message(
    hub: &CollaborationHub,
    board_id: &str,
    local_sender: &broadcast::Sender<String>,
    message: &ServerMessage,
) {
    let json = serialize_server_message(message);
    if hub.redis_publisher.is_none() {
        // Without Redis, this process is the complete collaboration cluster.
        let _ = local_sender.send(json);
        return;
    }
    // With Redis, this instance's subscriber delivers the event locally as well as remotely.
    if let Err(error) = hub.publish(board_id, json).await {
        tracing::error!(
            subsystem = "websocket",
            event = "collaboration_event_publish_failed",
            board_id,
            %error,
            "Failed to publish collaboration event; local clients must resync"
        );
        let resync = serialize_server_message(&ServerMessage::ResyncRequired);
        let _ = local_sender.send(resync);
    }
}

/// Converts a server event to the common JSON envelope used by every transport.
fn serialize_server_message(message: &ServerMessage) -> String {
    // Attach the protocol version uniformly so every server event is versioned.
    let mut value = serde_json::to_value(message).expect("server message is serializable");
    value
        .as_object_mut()
        .expect("server messages serialize as objects")
        .insert("protocolVersion".to_string(), json!(PROTOCOL_VERSION));
    serde_json::to_string(&value).expect("server message is serializable")
}

/// Maps database authorization roles to the stable role names in the wire protocol.
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
    fn validates_ephemeral_preview_batches_without_document_operation_ids() {
        let message = serde_json::from_value::<ClientMessage>(json!({
            "type": "preview",
            "sequence": 3,
            "batch": {
                "upserts": [element("el-1", "rectangle")],
                "deletedIds": []
            }
        }))
        .unwrap();
        let ClientMessage::Preview { sequence, batch } = message else {
            panic!("expected preview request");
        };
        assert_eq!(sequence, 3);
        assert!(validate_operation("preview", batch).is_ok());
        assert!(validate_preview(ElementBatch {
            upserts: vec![],
            deleted_ids: vec![],
        })
        .is_ok());
    }

    #[test]
    fn includes_protocol_version_in_server_envelopes() {
        let message = serialize_server_message(&ServerMessage::Pong);
        let envelope: Value = serde_json::from_str(&message).unwrap();
        assert_eq!(envelope["type"], "pong");
        assert_eq!(envelope["protocolVersion"], PROTOCOL_VERSION);
    }

    #[test]
    fn serializes_targeted_access_revocation_events() {
        let message = serialize_server_message(&ServerMessage::AccessRevoked {
            user_id: "user-1".to_string(),
            message: "Your permission to view this canvas has been revoked.".to_string(),
        });
        let envelope: Value = serde_json::from_str(&message).unwrap();
        assert_eq!(envelope["type"], "access_revoked");
        assert_eq!(envelope["userId"], "user-1");
        assert_eq!(
            envelope["message"],
            "Your permission to view this canvas has been revoked."
        );
        assert_eq!(envelope["protocolVersion"], PROTOCOL_VERSION);
    }

    #[tokio::test]
    async fn broadcasts_access_revocation_to_board_connections() {
        let hub = CollaborationHub::default();
        let (_, mut receiver) = hub.subscribe("board-1").await;

        hub.revoke_board_access("board-1", "user-1").await;

        let message: Value = serde_json::from_str(&receiver.recv().await.unwrap()).unwrap();
        assert_eq!(message["type"], "access_revoked");
        assert_eq!(message["userId"], "user-1");
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

    #[tokio::test]
    async fn a_slow_room_consumer_does_not_block_healthy_consumers() {
        let hub = CollaborationHub::default();
        let (sender, mut slow) = hub.subscribe("board-1").await;
        let (_, mut healthy) = hub.subscribe("board-1").await;

        for sequence in 0..=ROOM_CAPACITY {
            sender.send(sequence.to_string()).unwrap();
            assert_eq!(healthy.recv().await.unwrap(), sequence.to_string());
        }
        assert!(matches!(
            slow.recv().await,
            Err(broadcast::error::RecvError::Lagged(_))
        ));
    }
}
