CREATE TABLE board_templates (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    description TEXT,
    category TEXT NOT NULL DEFAULT 'Yours',
    created_by_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    elements JSONB NOT NULL CHECK (jsonb_typeof(elements) = 'array'),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX board_templates_creator_idx ON board_templates(created_by_id, created_at);
