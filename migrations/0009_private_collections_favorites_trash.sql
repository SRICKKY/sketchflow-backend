ALTER TABLE boards ADD COLUMN deleted_at TIMESTAMPTZ;

CREATE INDEX boards_owner_trash_idx
    ON boards (owner_id, updated_at DESC, id DESC)
    WHERE deleted_at IS NOT NULL;

ALTER TABLE boards
    ADD CONSTRAINT boards_id_owner_id_unique UNIQUE (id, owner_id);

CREATE TABLE private_collections (
    id         TEXT PRIMARY KEY,
    owner_id   TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name       TEXT NOT NULL CHECK (length(trim(name)) BETWEEN 1 AND 40),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (id, owner_id)
);

CREATE TABLE private_collection_boards (
    owner_id      TEXT NOT NULL,
    board_id      TEXT NOT NULL,
    collection_id TEXT NOT NULL,
    PRIMARY KEY (owner_id, board_id),
    FOREIGN KEY (board_id, owner_id)
        REFERENCES boards(id, owner_id) ON DELETE CASCADE,
    FOREIGN KEY (collection_id, owner_id)
        REFERENCES private_collections(id, owner_id) ON DELETE CASCADE
);

CREATE INDEX private_collection_boards_collection_idx
    ON private_collection_boards (owner_id, collection_id);

CREATE TABLE board_favorites (
    user_id    TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    board_id   TEXT NOT NULL REFERENCES boards(id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, board_id)
);
