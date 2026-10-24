CREATE INDEX boards_personal_owner_page_idx
    ON boards (owner_id, updated_at DESC, id DESC)
    WHERE workspace_id IS NULL;

CREATE INDEX boards_workspace_page_idx
    ON boards (workspace_id, updated_at DESC, id DESC)
    WHERE workspace_id IS NOT NULL;
