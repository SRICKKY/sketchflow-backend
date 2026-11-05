CREATE TABLE lifetime_purchases (
    id                 TEXT PRIMARY KEY,
    user_id            TEXT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    status             TEXT NOT NULL DEFAULT 'active',
    amount_minor       INTEGER NOT NULL,
    credit_minor       INTEGER NOT NULL DEFAULT 0,
    currency           TEXT NOT NULL,
    gateway            TEXT NOT NULL,
    gateway_order_id   TEXT,
    gateway_payment_id TEXT NOT NULL UNIQUE,
    starts_at          TIMESTAMPTZ NOT NULL,
    expires_at         TIMESTAMPTZ NOT NULL,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at         TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX lifetime_purchases_user_id_status_idx
    ON lifetime_purchases (user_id, status);
CREATE INDEX lifetime_purchases_expires_at_idx
    ON lifetime_purchases (expires_at);
CREATE UNIQUE INDEX lifetime_purchases_one_active_per_user_idx
    ON lifetime_purchases (user_id)
    WHERE status = 'active';
