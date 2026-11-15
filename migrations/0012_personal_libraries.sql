CREATE TABLE personal_libraries (
    id         TEXT PRIMARY KEY,
    owner_id   TEXT NOT NULL UNIQUE REFERENCES users(id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE personal_library_items (
    id            TEXT PRIMARY KEY,
    library_id    TEXT NOT NULL REFERENCES personal_libraries(id) ON DELETE CASCADE,
    name          TEXT NOT NULL CHECK (length(trim(name)) BETWEEN 1 AND 80),
    elements      JSONB NOT NULL CHECK (jsonb_typeof(elements) = 'array'),
    element_count INTEGER NOT NULL CHECK (element_count BETWEEN 1 AND 5000),
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX personal_library_items_library_created_idx
    ON personal_library_items (library_id, created_at DESC, id);

CREATE TABLE personal_library_assets (
    library_item_id TEXT NOT NULL
        REFERENCES personal_library_items(id) ON DELETE CASCADE,
    asset_id        TEXT NOT NULL
        REFERENCES assets(id) ON DELETE CASCADE,
    PRIMARY KEY (library_item_id, asset_id)
);

CREATE INDEX personal_library_assets_asset_idx
    ON personal_library_assets (asset_id);
