ALTER TABLE boards ADD COLUMN presentation JSONB;

CREATE TABLE board_versions (
    id TEXT PRIMARY KEY,
    board_id TEXT NOT NULL REFERENCES boards(id) ON DELETE CASCADE,
    created_by_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    label TEXT,
    source TEXT NOT NULL CHECK (source IN ('manual', 'auto', 'restore')),
    element_count INTEGER NOT NULL,
    elements JSONB NOT NULL,
    presentation JSONB,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX board_versions_board_created_idx ON board_versions(board_id, created_at DESC);

CREATE TABLE board_comments (
    id TEXT PRIMARY KEY,
    board_id TEXT NOT NULL REFERENCES boards(id) ON DELETE CASCADE,
    author_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    parent_id TEXT REFERENCES board_comments(id) ON DELETE CASCADE,
    body TEXT NOT NULL,
    x DOUBLE PRECISION NOT NULL,
    y DOUBLE PRECISION NOT NULL,
    resolved BOOLEAN NOT NULL DEFAULT false,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX board_comments_board_created_idx ON board_comments(board_id, created_at);
CREATE INDEX board_comments_parent_idx ON board_comments(parent_id);

CREATE TABLE comment_reactions (
    id TEXT PRIMARY KEY,
    comment_id TEXT NOT NULL REFERENCES board_comments(id) ON DELETE CASCADE,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    emoji TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE(comment_id, user_id, emoji)
);
