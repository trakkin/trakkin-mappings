CREATE SCHEMA IF NOT EXISTS ingestion;

CREATE TABLE IF NOT EXISTS ingestion.sources (
    source text PRIMARY KEY,
    revision bigint NOT NULL DEFAULT 0 CHECK (revision >= 0),
    history_floor bigint NOT NULL DEFAULT 0 CHECK (history_floor >= 0 AND history_floor <= revision)
);

CREATE TABLE IF NOT EXISTS ingestion.record_versions (
    source text NOT NULL REFERENCES ingestion.sources(source),
    revision bigint NOT NULL CHECK (revision > 0),
    key text NOT NULL,
    payload text NOT NULL,
    hash text NOT NULL,
    deleted boolean NOT NULL,
    metadata jsonb NOT NULL CHECK (jsonb_typeof(metadata) = 'object'),
    recorded_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (source, revision, key)
);

CREATE TABLE IF NOT EXISTS ingestion.records (
    source text NOT NULL REFERENCES ingestion.sources(source),
    key text NOT NULL CHECK (strpos(key, ':') > 1 AND length(key) > strpos(key, ':')),
    domain text GENERATED ALWAYS AS (split_part(key, ':', 1)) STORED,
    revision bigint NOT NULL,
    hash text NOT NULL,
    deleted boolean NOT NULL,
    metadata jsonb NOT NULL CHECK (jsonb_typeof(metadata) = 'object'),
    seen_run text,
    PRIMARY KEY (source, key),
    FOREIGN KEY (source, revision, key) REFERENCES ingestion.record_versions(source, revision, key)
);

CREATE INDEX IF NOT EXISTS records_reconciliation
    ON ingestion.records (source, domain, key) WHERE NOT deleted;

CREATE INDEX IF NOT EXISTS records_parent
    ON ingestion.records (source, (metadata->>'parent'), key) WHERE NOT deleted;

CREATE TABLE IF NOT EXISTS ingestion.sync_state (
    source text NOT NULL REFERENCES ingestion.sources(source),
    scope text NOT NULL,
    value jsonb NOT NULL,
    PRIMARY KEY (source, scope)
);