ALTER TABLE assets
    ADD COLUMN owner_id TEXT,
    ADD COLUMN board_id TEXT,
    ADD COLUMN status TEXT NOT NULL DEFAULT 'complete'
        CHECK (status IN ('pending', 'complete', 'failed', 'expired')),
    ADD COLUMN expires_at TIMESTAMPTZ,
    ADD COLUMN completed_at TIMESTAMPTZ,
    ADD COLUMN cleanup_at TIMESTAMPTZ;

CREATE INDEX assets_board_status_idx ON assets (board_id, status);
CREATE INDEX assets_upload_cleanup_idx ON assets (status, expires_at)
    WHERE cleanup_at IS NULL;
