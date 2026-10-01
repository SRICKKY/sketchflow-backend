-- Mirrors the subset of the SketchFlow Prisma schema (prisma/schema.prisma)
-- that this backend owns, plus an `assets` table for uploaded files
-- (avatars + board images) which the Next.js app stores outside Postgres.

CREATE TABLE users (
    id                  TEXT PRIMARY KEY,
    name                TEXT,
    email               TEXT NOT NULL UNIQUE,
    email_verified      TIMESTAMPTZ,
    image               TEXT,
    password_hash       TEXT,
    plan                TEXT NOT NULL DEFAULT 'free',
    plan_expires_at     TIMESTAMPTZ,
    avatar_change_count INTEGER NOT NULL DEFAULT 0,
    avatar_change_month TEXT,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE sessions (
    session_token TEXT PRIMARY KEY,
    user_id       TEXT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    expires       TIMESTAMPTZ NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX sessions_user_id_idx ON sessions (user_id);

CREATE TABLE accounts (
    user_id             TEXT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    type                TEXT NOT NULL,
    provider            TEXT NOT NULL,
    provider_account_id TEXT NOT NULL,
    refresh_token       TEXT,
    access_token        TEXT,
    expires_at          BIGINT,
    token_type          TEXT,
    scope               TEXT,
    id_token            TEXT,
    session_state       TEXT,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (provider, provider_account_id)
);

CREATE INDEX accounts_user_id_idx ON accounts (user_id);

CREATE TABLE verification_tokens (
    identifier TEXT NOT NULL,
    token      TEXT NOT NULL,
    expires    TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (identifier, token)
);

CREATE TABLE subscriptions (
    id                 TEXT PRIMARY KEY,
    user_id            TEXT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    interval           TEXT NOT NULL,
    status             TEXT NOT NULL DEFAULT 'active',
    starts_at          TIMESTAMPTZ NOT NULL,
    expires_at         TIMESTAMPTZ NOT NULL,
    payment_id         TEXT NOT NULL UNIQUE,
    gateway_order_id   TEXT,
    gateway_payment_id TEXT,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at         TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX subscriptions_user_id_status_idx ON subscriptions (user_id, status);
CREATE INDEX subscriptions_expires_at_idx ON subscriptions (expires_at);

CREATE TABLE invoices (
    id                 TEXT PRIMARY KEY,
    user_id            TEXT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    subscription_id    TEXT REFERENCES subscriptions (id) ON DELETE SET NULL,
    number             TEXT NOT NULL UNIQUE,
    interval           TEXT NOT NULL,
    amount_minor       INTEGER NOT NULL,
    currency           TEXT NOT NULL,
    status             TEXT NOT NULL DEFAULT 'paid',
    paid_at            TIMESTAMPTZ NOT NULL,
    period_start       TIMESTAMPTZ NOT NULL,
    period_end         TIMESTAMPTZ NOT NULL,
    gateway_order_id   TEXT,
    gateway_payment_id TEXT UNIQUE,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX invoices_user_id_paid_at_idx ON invoices (user_id, paid_at);
CREATE INDEX invoices_subscription_id_idx ON invoices (subscription_id);

CREATE TABLE boards (
    id           TEXT PRIMARY KEY,
    name         TEXT NOT NULL,
    owner_id     TEXT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    workspace_id TEXT,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX boards_owner_id_idx ON boards (owner_id);
CREATE INDEX boards_workspace_id_idx ON boards (workspace_id);

CREATE TYPE board_role AS ENUM ('OWNER', 'EDITOR', 'VIEWER');

CREATE TABLE board_members (
    id         TEXT PRIMARY KEY,
    board_id   TEXT NOT NULL REFERENCES boards (id) ON DELETE CASCADE,
    user_id    TEXT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    role       board_role NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (board_id, user_id)
);

CREATE INDEX board_members_user_id_idx ON board_members (user_id);
CREATE INDEX board_members_user_id_role_idx ON board_members (user_id, role);

CREATE TABLE board_elements (
    id         TEXT PRIMARY KEY,
    board_id   TEXT NOT NULL REFERENCES boards (id) ON DELETE CASCADE,
    element_id TEXT NOT NULL,
    type       TEXT NOT NULL,
    data       JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (board_id, element_id)
);

CREATE INDEX board_elements_board_id_created_at_idx ON board_elements (board_id, created_at);
CREATE INDEX board_elements_board_id_type_idx ON board_elements (board_id, type);

-- Uploaded binary assets (avatars, pasted board images). The Next.js app
-- stores these via `@/lib/storage`; this backend persists metadata here and
-- the bytes on disk under `ASSET_STORAGE_DIR`.
CREATE TABLE assets (
    id            TEXT PRIMARY KEY,
    mime_type     TEXT NOT NULL,
    width         INTEGER NOT NULL,
    height        INTEGER NOT NULL,
    byte_size     INTEGER NOT NULL,
    storage_path  TEXT NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);
