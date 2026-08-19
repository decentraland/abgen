
CREATE TABLE IF NOT EXISTS denylist (
    entity_id  TEXT PRIMARY KEY,
    reason     TEXT,
    created_by TEXT,
    created_at BIGINT NOT NULL DEFAULT (extract(epoch from now()) * 1000)::bigint,
    updated_at BIGINT NOT NULL DEFAULT (extract(epoch from now()) * 1000)::bigint
);

CREATE TABLE IF NOT EXISTS world_spawn_coordinates (
    world_name  TEXT PRIMARY KEY,
    x           BIGINT NOT NULL,
    y           BIGINT NOT NULL,
    is_user_set BOOLEAN NOT NULL DEFAULT false,
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
