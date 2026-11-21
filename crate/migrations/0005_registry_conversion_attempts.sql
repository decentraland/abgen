CREATE TABLE IF NOT EXISTS registry_conversion_attempts (
    entity_id TEXT NOT NULL,
    platform TEXT NOT NULL CHECK (platform IN ('windows', 'mac', 'webgl', 'linux')),
    generation BIGINT NOT NULL CHECK (generation > 0),
    PRIMARY KEY (entity_id, platform)
);
