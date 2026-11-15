ALTER TABLE private_collections
    ADD COLUMN deleted_at TIMESTAMPTZ;

ALTER TABLE private_collection_boards
    DROP CONSTRAINT private_collection_boards_pkey,
    ADD CONSTRAINT private_collection_boards_pkey
        PRIMARY KEY (owner_id, board_id, collection_id);

ALTER TABLE personal_libraries RENAME TO libraries;
ALTER TABLE libraries DROP CONSTRAINT personal_libraries_owner_id_key;
ALTER TABLE libraries
    ADD COLUMN scope TEXT NOT NULL DEFAULT 'user',
    ADD COLUMN collection_id TEXT,
    ADD COLUMN updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    ADD CONSTRAINT libraries_scope_check
        CHECK (scope IN ('user', 'collection')),
    ADD CONSTRAINT libraries_scope_collection_check
        CHECK (
            (scope = 'user' AND collection_id IS NULL)
            OR (scope = 'collection' AND collection_id IS NOT NULL)
        ),
    ADD CONSTRAINT libraries_collection_fk
        FOREIGN KEY (collection_id) REFERENCES private_collections(id) ON DELETE CASCADE,
    ADD CONSTRAINT libraries_collection_owner_fk
        FOREIGN KEY (collection_id, owner_id)
        REFERENCES private_collections(id, owner_id) ON DELETE CASCADE;

CREATE UNIQUE INDEX libraries_user_owner_unique
    ON libraries (owner_id)
    WHERE scope = 'user';
CREATE UNIQUE INDEX libraries_collection_unique
    ON libraries (collection_id)
    WHERE scope = 'collection';

ALTER TABLE personal_library_items RENAME TO library_items;
ALTER TABLE library_items
    ADD COLUMN position BIGINT NOT NULL DEFAULT 0 CHECK (position >= 0),
    ADD COLUMN content_hash TEXT,
    ADD COLUMN content_hash_version SMALLINT NOT NULL DEFAULT 0
        CHECK (content_hash_version IN (0, 1));

WITH ranked AS (
    SELECT id, row_number() OVER (
        PARTITION BY library_id ORDER BY created_at DESC, id
    ) - 1 AS position
    FROM library_items
)
UPDATE library_items item
SET position = ranked.position
FROM ranked
WHERE item.id = ranked.id;

ALTER TABLE personal_library_assets RENAME TO library_item_assets;
ALTER INDEX personal_library_items_library_created_idx
    RENAME TO library_items_library_created_idx;
ALTER INDEX personal_library_assets_asset_idx
    RENAME TO library_item_assets_asset_idx;
CREATE INDEX library_items_library_position_idx
    ON library_items (library_id, position, created_at DESC, id);
CREATE INDEX library_items_content_hash_idx
    ON library_items (content_hash)
    WHERE content_hash_version = 1;

CREATE TABLE library_audit_events (
    id                BIGSERIAL PRIMARY KEY,
    actor_id          TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    event             TEXT NOT NULL CHECK (event IN (
        'library_created', 'item_added', 'item_copied', 'item_moved',
        'item_deleted', 'collection_duplicated'
    )),
    library_id        TEXT NOT NULL,
    target_library_id TEXT,
    item_count        BIGINT NOT NULL DEFAULT 1 CHECK (item_count >= 0),
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX library_audit_events_actor_created_idx
    ON library_audit_events (actor_id, created_at DESC);

CREATE TABLE library_write_rate_limits (
    actor_id     TEXT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    window_start TIMESTAMPTZ NOT NULL,
    request_count INTEGER NOT NULL CHECK (request_count > 0)
);
