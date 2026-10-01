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
- **reqwest** — Razorpay HTTP client
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
| Billing | `POST /api/create-order`, `POST /api/verify-payment` |
| Profile | `POST /api/profile/avatar`, `DELETE /api/profile/avatar` |
| Uploads | `POST /api/uploads`, `GET /api/uploads/{assetId}` |
| Invoices | `GET /billing/{invoiceId}/pdf` |

## Development

```bash
cargo build
cargo test
cargo fmt
cargo clippy
```
