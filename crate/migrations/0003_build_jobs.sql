CREATE TABLE IF NOT EXISTS build_jobs (
    entity_id    TEXT NOT NULL,
    platform     TEXT NOT NULL,
    status       TEXT NOT NULL DEFAULT 'pending',
    requested_by TEXT,
    enqueued_at  BIGINT NOT NULL DEFAULT (extract(epoch from now()) * 1000)::bigint,
    updated_at   BIGINT NOT NULL DEFAULT (extract(epoch from now()) * 1000)::bigint,
    PRIMARY KEY (entity_id, platform)
);

CREATE INDEX IF NOT EXISTS build_jobs_status_platform_idx
    ON build_jobs (status, platform);
