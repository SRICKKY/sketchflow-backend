# SketchFlow Backend

A Rust + [Axum](https://github.com/tokio-rs/axum) backend implementing the
SketchFlow REST API (see
[`../SketchFlow/docs/API_ENDPOINTS.md`](../SketchFlow/docs/API_ENDPOINTS.md)
for the original Next.js route reference), with interactive OpenAPI/Swagger
documentation.

## Stack

- **axum** — HTTP server/routing
- **sqlx** (Postgres) — database access, schema mirrors the SketchFlow Prisma
  schema for the models this API owns
- **Redis** — optional shared Pub/Sub fan-out for collaboration across
  backend instances
- **utoipa** + **utoipa-swagger-ui** — OpenAPI spec generation and Swagger UI
- **jsonwebtoken** + **argon2** — session cookies (JWT) and password hashing
- **reqwest** — Razorpay and Paddle HTTP clients
- **aws-sdk-s3** — image and avatar storage in S3-compatible object stores,
  including local Silo
- **printpdf** — invoice PDF generation

## Getting started

### Docker Compose (local development)

With Docker Desktop running, execute these commands from this backend directory:

```bash
docker compose up -d --wait --wait-timeout 120 silo
docker compose --profile setup run --rm silo-init
docker compose up --build -d --wait --wait-timeout 120 backend
curl --fail http://localhost:8080/health
docker compose logs -f backend
```

The first build compiles Rust dependencies and can take several minutes. Compose
starts PostgreSQL 16, Redis 7, and Silo, then creates the asset bucket before
starting the API. The API automatically applies its embedded SQL migrations on
startup; you do not need Rust or `sqlx-cli` installed on your host.

- API: `http://localhost:8080`
- Swagger UI: `http://localhost:8080/docs`
- Silo console: `http://localhost:9001`
- WebSocket: `ws://localhost:8080/api/boards/{boardId}/collaboration`
- PostgreSQL data, Silo objects, and fallback uploads persist in named Docker
  volumes. The existing `minio_data` volume is retained for compatible object
  data when switching from MinIO.
- PostgreSQL, Redis, and Silo's S3 API are internal to the Compose network;
  the API and Silo console are published only on localhost.

No `.env` file is required. This setup intentionally uses a **separate local
database**, not the Next.js app's Neon database. Existing Neon users and boards
will not appear in it. The backend's migrations use a separate, lowercase SQL
schema; do not point them at the Next.js database without a reviewed migration
and authentication integration plan.

Compose reads optional `JWT_SECRET`, `JWT_SESSION_DAYS`, `CORS_ALLOWED_ORIGINS`,
`BACKEND_PORT`, `S3_BUCKET`, `S3_REGION`, `S3_ACCESS_KEY_ID`,
`S3_SECRET_ACCESS_KEY`, and `RUST_LOG` values from
your shell or backend `.env`. It deliberately ignores `DATABASE_URL` and
`REDIS_URL` there and uses the local services instead. The Compose backend
connects to Silo at `http://silo:9000`. The development database password,
Silo credentials, and default JWT secret are **local-only defaults**, not
production configuration.

If port 8080 is already occupied:

```bash
BACKEND_PORT=8081 docker compose up --build -d --wait --wait-timeout 120
curl --fail http://localhost:8081/health
```

To stop the stack while retaining users, boards, and uploads:

```bash
docker compose down
```

Do not add `--volumes` unless you intend to permanently delete the local
database and uploaded files. After editing Rust code, run the `up --build`
command again to rebuild and restart the API.

The React development server can reach this API at `http://localhost:8080`
through its `/api` proxy. Register and sign in against this local API; the
separate local database does not contain accounts from the production app.
Expo web origins at `http://localhost:8081` and `http://localhost:8082` are
also allowed for local development. If the frontend uses a different origin,
add that exact origin to `CORS_ALLOWED_ORIGINS` and restart the backend.

### Local Silo

```bash
docker compose up -d --wait --wait-timeout 120 silo
docker compose --profile setup run --rm silo-init
```

Silo is a MinIO-compatible S3 server maintained by PGSTY. Compose pins the
`RELEASE.2026-10-26T00-00-00Z` image, persists data in the existing
`minio_data` volume, and creates the `sketchflow-assets` bucket automatically.
Open `http://localhost:9001` and sign in with the local credentials
`sketchflow` / `local-silo-development-only`. These credentials are for local
development only. The S3 API is available at `http://localhost:9000`; Cargo-run
backends use that address, while the Compose backend uses `http://silo:9000`.

### Run directly with Cargo

```bash
cp .env.example .env
# edit .env with your Postgres URL, JWT secret, Razorpay keys, etc.
# set REDIS_URL to enable cross-instance collaboration fan-out

# start Postgres (example using Docker)
docker run --name sketchflow-db -e POSTGRES_USER=sketchflow \
  -e POSTGRES_PASSWORD=sketchflow -e POSTGRES_DB=sketchflow \
  -p 5432:5432 -d postgres:16

# start Silo and create the configured asset bucket
docker compose up -d --wait --wait-timeout 120 silo
docker compose --profile setup run --rm silo-init

# run migrations
cargo install sqlx-cli --no-default-features --features rustls,postgres
sqlx migrate run

# start the server
cargo run
```

The server listens on `http://localhost:8080` by default.

- Health check: `GET /health`
- Swagger UI: `GET /docs`
- OpenAPI JSON: `GET /api-docs/openapi.json`

## Endpoints

| Area | Routes |
|---|---|
| Auth | `POST /api/auth/register`, `POST /api/auth/login`, `POST /api/auth/logout`, `GET /api/auth/session` |
| Billing | `POST /api/create-order`, `POST /api/verify-payment`, `POST /api/paddle/verify-payment`, `POST /api/paddle/webhook` |
| Profile | `POST /api/profile/avatar`, `DELETE /api/profile/avatar` |
| Uploads | `POST /api/uploads`, `GET /api/uploads/{assetId}` |
| Notifications | `GET /api/notifications`, `GET /api/notifications/unread-count`, `PATCH /api/notifications/{notificationId}`, `POST /api/notifications/read-all` |
| Collaboration | `GET /api/boards/{boardId}/snapshot`, `GET /api/boards/{boardId}/collaboration` (WebSocket) |
| Invoices | `GET /billing/{invoiceId}/pdf` |

Notification routes require the signed-in `sketchflow_session` cookie and
return private, non-cacheable responses. The list endpoint accepts
`filter=all|unread`, `limit=1..50` (default `20`), and an opaque `cursor`;
it returns `items`, `nextCursor`, `unreadCount`, and `readAllBefore`.
`PATCH /api/notifications/{notificationId}` marks one accessible notification
read. `POST /api/notifications/read-all` accepts
`{"readAllBefore":"<notification-id>"}` and marks only notifications up to
that stable boundary, leaving newer arrivals unread. Notifications are hidden
when the recipient no longer has access to their board or workspace.
Board comments notify the board owner, replies notify the root author and
previous thread participants, and newly added comment reactions notify the
comment author. Removing a reaction does not create a notification.

The collaboration WebSocket requires the existing `sketchflow_session` cookie
and an `Origin` listed in `CORS_ALLOWED_ORIGINS`. It sends an initial
`connected` event and a revisioned `snapshot`. All JSON messages have
`protocolVersion: 1`. Clients submit
`{"type":"apply_operation","protocolVersion":1,"clientOperationId":"...","batch":{"upserts":[...],"deletedIds":[]}}`
messages; the backend validates and atomically persists each operation,
assigns its board revision, and broadcasts `operation_applied` to the board.
Retries with the same actor and operation ID are idempotent while the
operation remains in the latest 1,000-operation history for that board. A
client can send `presence` messages containing a cursor and selected element
IDs; those events are transient and are never stored. Editors can also send
`preview` messages with a monotonically increasing connection-local sequence
and an incremental element batch. Preview batches are bounded to 500 upserts
and 500 deletions, are broadcast without database writes, and are replaced by
the next preview from that connection. An empty preview batch clears its
current preview. Committed operations remain the only source of persisted
document state.

The WebSocket message shapes are:

| Direction | `type` | Fields |
|---|---|---|
| Server | `connected` | `boardId`, `connectionId`, `role` |
| Server | `snapshot` | `boardId`, `revision`, `elements` |
| Client | `apply_operation` | `clientOperationId`, `batch: { upserts, deletedIds }` |
| Server | `operation_applied` | `revision`, `clientOperationId`, `actorId`, `batch` |
| Client | `preview` | `sequence`, `batch: { upserts, deletedIds }` |
| Server | `preview_updated` | `connectionId`, `userId`, `sequence`, `batch` |
| Client | `presence` | optional `cursor: { x, y }`, `selectedIds` |
| Server | `presence_joined` | `connectionId`, `userId`, `name`, `image` |
| Server | `presence_updated` | `connectionId`, `userId`, `cursor`, `selectedIds` |
| Server | `presence_left` | `connectionId`, `userId` |
| Client | `ping` | no additional fields |
| Server | `pong` | no additional fields |
| Server | `error` | `message` |
| Server | `resync_required` | reconnect and fetch a fresh snapshot |

All client messages include `protocolVersion: 1`. For example:

```json
{
  "type": "presence",
  "protocolVersion": 1,
  "cursor": { "x": 120, "y": 80 },
  "selectedIds": ["element-id"]
}
```
Connections are limited to 60 JSON messages per second and 1 MiB per message.
The client sends application heartbeats every 20 seconds; the server closes
connections with no inbound activity for 60 seconds so presence is cleaned up.
Board access is periodically revalidated while the socket remains open.

Owners and editors can submit document changes. Viewers can receive snapshots
and presence but cannot edit. Simultaneous edits are ordered by the backend;
the last accepted update to a given element wins.

Set `REDIS_URL` to a Redis connection URL to enable shared Pub/Sub fan-out;
`rediss://` URLs use TLS. All backend instances must use the same Redis service
and channel. When Redis is unset, collaboration uses in-process fan-out and
requires a single backend instance. Redis Pub/Sub is transient rather than
durable: Postgres remains authoritative, and clients must honor
`resync_required` or a revision gap by fetching a fresh snapshot. Redis is
required at startup when `REDIS_URL` is configured; subscriber connections
retry after runtime disconnections.

Canvas edits made through the existing Next.js server actions do not produce
collaboration operations. Enable collaborative editing only after clients
write through this backend, or after all legacy write paths have been updated
to participate in its revision and broadcast flow.

Image and avatar uploads use S3-compatible storage when `S3_BUCKET` is
configured. Set `S3_REGION`, credentials, and optionally `S3_ENDPOINT_URL` for
services such as Silo; custom endpoints use path-style bucket addressing.
Without `S3_BUCKET`, uploads continue to use local `ASSET_STORAGE_DIR`. Uploads
store image objects under `assets/` plus JSON metadata sidecars, and the
existing `GET /api/uploads/{assetId}` endpoint streams the image from storage.
For local development, Compose configures Silo automatically. The Compose
backend uses `http://silo:9000`; a Cargo-hosted API uses
`http://localhost:9000` from `.env.example`.

`POST /api/create-order` uses `PAYMENT_GATEWAY` (`razorpay` by default, or
`paddle`). Razorpay keeps the existing order/verification flow. Paddle returns
`transactionId`, `clientToken`, and `environment`; the frontend confirms the
checkout with Paddle, then calls `/api/paddle/verify-payment`. Configure the
Paddle environment, API key, client token, webhook secret, and one-time USD
price IDs with `PADDLE_*` variables. Paddle's monthly/yearly USD prices must
match the INR plan amounts converted to USD cents, as done by the SketchFlow
checkout.

Cashfree payment endpoints are intentionally not implemented in this backend.
The app's Auth.js provider/callback endpoints are likewise not replicated;
this service exposes its own register, login, logout, and session routes.

### Endpoint gaps addressed

Compared with the app's payment routes, the backend now supports Paddle
transaction creation through `POST /api/create-order`, transaction verification
through `POST /api/paddle/verify-payment`, and signed completion webhooks through
`POST /api/paddle/webhook`. Cashfree routes remain intentionally excluded.

## Development

```bash
cargo build
cargo test
cargo fmt
cargo clippy
```

### Build a Linux deployment artifact

The **Build Linux release** GitHub Actions workflow builds the release
executable on Ubuntu 22.04 for x86_64. It runs for pull requests and pushes to
`main`, and can also be started manually from the repository's Actions tab. A
successful run publishes the `sketchflow-backend-linux-x86_64` artifact for 14
days.

Download and extract the artifact, then copy the executable to the EC2
instance. For example, from the machine where the artifact was downloaded:

```bash
scp -i /path/to/key.pem sketchflow-backend \
  ubuntu@YOUR_EC2_IP:~/sketchflow-backend/sketchflow-backend
```

On EC2, run the executable from the backend directory so it can read the
instance's existing `.env` file:

```bash
cd ~/sketchflow-backend
chmod +x ./sketchflow-backend
./sketchflow-backend
```

This artifact is for x86_64 Linux instances, such as the EC2 T3 family. Check
the instance architecture with `uname -m` before deployment. Keep runtime
secrets in the EC2 environment; the build workflow does not require or upload
the `.env` file. Deploy artifacts only from trusted workflow runs.
