DO $$ BEGIN
    CREATE TYPE workspace_role AS ENUM ('OWNER', 'ADMIN', 'MEMBER');
EXCEPTION
    WHEN duplicate_object THEN NULL;
END $$;

CREATE TABLE IF NOT EXISTS workspaces (
    id            TEXT PRIMARY KEY,
    name          TEXT NOT NULL,
    slug          TEXT NOT NULL UNIQUE,
    created_by_id TEXT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS workspace_members (
    id           TEXT PRIMARY KEY,
    workspace_id TEXT NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    user_id      TEXT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    role         workspace_role NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, user_id)
);

CREATE INDEX IF NOT EXISTS workspace_members_user_id_idx ON workspace_members (user_id);

CREATE TABLE board_collaboration_state (
    board_id   TEXT PRIMARY KEY REFERENCES boards (id) ON DELETE CASCADE,
    revision   BIGINT NOT NULL DEFAULT 0 CHECK (revision >= 0),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE board_operations (
    id                   TEXT PRIMARY KEY,
    board_id             TEXT NOT NULL REFERENCES boards (id) ON DELETE CASCADE,
    revision             BIGINT NOT NULL,
    actor_id             TEXT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    client_operation_id  TEXT NOT NULL,
    operation            JSONB NOT NULL,
    created_at           TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (board_id, revision),
    UNIQUE (board_id, actor_id, client_operation_id)
);

CREATE INDEX board_operations_board_created_at_idx
    ON board_operations (board_id, created_at);
