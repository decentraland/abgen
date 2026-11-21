CREATE TABLE IF NOT EXISTS registry_records (
    entity_id TEXT PRIMARY KEY CHECK (entity_id <> ''),
    world_name TEXT NOT NULL DEFAULT '',
    entity_timestamp BIGINT NOT NULL CHECK (entity_timestamp >= 0),
    pointers TEXT[] NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('pending', 'complete', 'failed', 'fallback', 'obsolete')),
    bundles JSONB NOT NULL CHECK (jsonb_typeof(bundles) = 'object'),
    versions JSONB NOT NULL CHECK (jsonb_typeof(versions) = 'object'),
    updated_at BIGINT NOT NULL
);
