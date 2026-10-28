CREATE TYPE notification_type AS ENUM (
    'BOARD_SHARED',
    'BOARD_ROLE_CHANGED',
    'WORKSPACE_JOINED',
    'BOARD_COMMENT',
    'COMMENT_REPLY'
);

CREATE TABLE notifications (
    id              TEXT PRIMARY KEY,
    sequence        BIGSERIAL NOT NULL UNIQUE,
    recipient_id    TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    actor_id        TEXT REFERENCES users(id) ON DELETE SET NULL,
    notification_type notification_type NOT NULL,
    event_key       TEXT NOT NULL,
    board_id        TEXT REFERENCES boards(id) ON DELETE CASCADE,
    workspace_id    TEXT REFERENCES workspaces(id) ON DELETE CASCADE,
    comment_id      TEXT REFERENCES board_comments(id) ON DELETE CASCADE,
    payload_version INTEGER NOT NULL DEFAULT 1 CHECK (payload_version = 1),
    payload         JSONB NOT NULL CHECK (jsonb_typeof(payload) = 'object'),
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    read_at         TIMESTAMPTZ,
    CONSTRAINT notifications_destination_check
        CHECK (board_id IS NOT NULL OR workspace_id IS NOT NULL),
    CONSTRAINT notifications_recipient_event_key UNIQUE (recipient_id, event_key)
);

CREATE INDEX notifications_recipient_created_idx
    ON notifications(recipient_id, created_at DESC, id DESC);
CREATE INDEX notifications_recipient_unread_idx
    ON notifications(recipient_id, created_at DESC, id DESC)
    WHERE read_at IS NULL;
CREATE INDEX notifications_recipient_sequence_idx
    ON notifications(recipient_id, sequence DESC);
CREATE INDEX notifications_board_idx ON notifications(board_id);
CREATE INDEX notifications_workspace_idx ON notifications(workspace_id);
CREATE INDEX notifications_comment_idx ON notifications(comment_id);
