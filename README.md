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
- **utoipa** + **utoipa-swagger-ui** — OpenAPI spec generation and Swagger UI
- **jsonwebtoken** + **argon2** — session cookies (JWT) and password hashing
- **reqwest** — Razorpay and Paddle HTTP clients
- **aws-sdk-s3** — optional image and avatar storage in Amazon S3
- **printpdf** — invoice PDF generation

## Getting started

```bash
cp .env.example .env
# edit .env with your Postgres URL, JWT secret, Razorpay keys, etc.

# start Postgres (example using Docker)
docker run --name sketchflow-db -e POSTGRES_USER=sketchflow \
  -e POSTGRES_PASSWORD=sketchflow -e POSTGRES_DB=sketchflow \
  -p 5432:5432 -d postgres:16

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
| Invoices | `GET /billing/{invoiceId}/pdf` |

Image and avatar uploads use S3 when `S3_BUCKET` is configured. Set `S3_REGION`
and optionally `S3_ACCESS_KEY_ID`, `S3_SECRET_ACCESS_KEY`, and
`S3_SESSION_TOKEN`; the backend also supports AWS environment credentials and
the standard AWS credential provider chain (including IAM roles). Without
`S3_BUCKET`, uploads continue to use local `ASSET_STORAGE_DIR`. S3 uploads
store image objects under `assets/` plus JSON metadata sidecars, and the
existing `GET /api/uploads/{assetId}` endpoint streams the image from S3.

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
