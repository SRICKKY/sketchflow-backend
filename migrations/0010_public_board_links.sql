CREATE TABLE board_public_links (
    board_id      TEXT PRIMARY KEY REFERENCES boards(id) ON DELETE CASCADE,
    token_nonce   TEXT NOT NULL CHECK (length(token_nonce) = 32),
    created_by_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);
