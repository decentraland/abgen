
CREATE TABLE IF NOT EXISTS queue_control (
    id         INTEGER PRIMARY KEY DEFAULT 1 CHECK (id = 1),
    paused     BOOLEAN NOT NULL DEFAULT false,
    updated_by TEXT,
    updated_at BIGINT NOT NULL DEFAULT (extract(epoch from now()) * 1000)::bigint
);

INSERT INTO queue_control (id, paused) VALUES (1, false)
ON CONFLICT (id) DO NOTHING;

CREATE TABLE IF NOT EXISTS build_retries (
    entity_id    TEXT PRIMARY KEY,
    requested_by TEXT,
    requested_at BIGINT NOT NULL DEFAULT (extract(epoch from now()) * 1000)::bigint,
    attempts     INTEGER NOT NULL DEFAULT 1
);
