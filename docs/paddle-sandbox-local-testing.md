# Test Paddle Sandbox Locally

Use this setup to test a sandbox checkout and webhook against your local Rust
backend. It does not require changing the EC2 environment or sending webhooks to
`backend.sketchflow.online`.

## 1. Configure and run the local backend

Follow the [direct Cargo setup](../README.md#run-directly-with-cargo), using a
local database rather than a production database. The backend reads the local
`.env` file and listens on port `8080` by default. Docker Compose also passes
the gateway and Paddle settings from `.env` into the backend container.

Set the Paddle values in the local `.env` to your **Sandbox** credentials and
price IDs:

```dotenv
PADDLE_ENV=sandbox
PADDLE_API_KEY=
PADDLE_CLIENT_TOKEN=
PADDLE_WEBHOOK_SECRET=
PADDLE_PRICE_ID_MONTHLY=
PADDLE_PRICE_ID_YEARLY=
PADDLE_PRICE_ID_LIFETIME=
```

Keep these values local. Do not commit or deploy the sandbox `.env` values.
EC2's environment is separate and does not need to be changed for this test.

Start the backend using the same mode you normally use. For Docker Compose:

```bash
docker compose up --build -d --force-recreate --wait --wait-timeout 120 backend
```

For a direct Cargo run, stop the Compose backend first, then run:

```bash
cargo run
```

## 2. Expose the local webhook endpoint

In another terminal, forward public HTTPS traffic to the backend:

```bash
ngrok http 8080
```

Copy the HTTPS forwarding address printed by ngrok, for example
`https://example.ngrok-free.app`.

## 3. Configure Paddle Sandbox notifications

In Paddle Sandbox, create a notification destination with this URL:

```text
https://<ngrok-host>/api/paddle/webhook
```

Subscribe to the `transaction.completed` event. Copy the signing secret
generated for this destination into `PADDLE_WEBHOOK_SECRET` in the local
`.env`, then restart the backend. The secret must match this Sandbox
destination; do not use the live-mode secret.

## 4. Complete a local sandbox checkout

Run the local React app with its `/api` proxy pointed at
`http://localhost:8080`, then complete a checkout using Paddle Sandbox. The
backend's local database should reflect the resulting purchase after the
`transaction.completed` webhook is accepted.

Keep both the backend and ngrok running while testing. The ngrok URL can change
after it restarts; if it does, update the Sandbox notification destination.
