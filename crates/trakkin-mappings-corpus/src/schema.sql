CREATE SCHEMA IF NOT EXISTS corpus;

CREATE TABLE IF NOT EXISTS corpus.engine (
    singleton boolean PRIMARY KEY DEFAULT true CHECK (singleton),
    revision text NOT NULL
);

CREATE TABLE IF NOT EXISTS corpus.scheduler (
    singleton boolean PRIMARY KEY DEFAULT true CHECK (singleton),
    match_next boolean NOT NULL DEFAULT false
);
INSERT INTO corpus.scheduler(singleton) VALUES(true) ON CONFLICT DO NOTHING;

CREATE TABLE IF NOT EXISTS corpus.policy (
    singleton boolean PRIMARY KEY DEFAULT true CHECK (singleton),
    document jsonb NOT NULL,
    generation bigint NOT NULL DEFAULT 1,
    published_generation bigint NOT NULL DEFAULT 0
);
INSERT INTO corpus.policy(singleton,document)
VALUES(true,'{"format_version":1,"exclusive_source_pairs":[]}') ON CONFLICT DO NOTHING;

CREATE TABLE IF NOT EXISTS corpus.consumers (
    source text PRIMARY KEY REFERENCES ingestion.sources(source),
    revision bigint NOT NULL,
    after_key text,
    bootstrap boolean NOT NULL DEFAULT true,
    bootstrap_key text
);
ALTER TABLE corpus.consumers ADD COLUMN IF NOT EXISTS scheduled_at timestamptz NOT NULL DEFAULT now();

CREATE TABLE IF NOT EXISTS corpus.records (
    source text NOT NULL,
    key text NOT NULL,
    revision bigint NOT NULL,
    uri text NOT NULL UNIQUE,
    parent text,
    deleted boolean NOT NULL,
    kind text NOT NULL,
    engine text NOT NULL,
    PRIMARY KEY (source, key),
    FOREIGN KEY (source, revision, key) REFERENCES ingestion.record_versions(source, revision, key)
);
CREATE INDEX IF NOT EXISTS records_parent ON corpus.records(source, parent, key) WHERE NOT deleted;

CREATE TABLE IF NOT EXISTS corpus.blocks (
    scope text NOT NULL,
    uri text NOT NULL REFERENCES corpus.records(uri) ON DELETE CASCADE,
    PRIMARY KEY (scope, uri)
);
CREATE INDEX IF NOT EXISTS blocks_uri ON corpus.blocks(uri, scope);

CREATE TABLE IF NOT EXISTS corpus.jobs (
    scope text PRIMARY KEY,
    after_left text NOT NULL DEFAULT '',
    after_right text NOT NULL DEFAULT '',
    diagnostic text
);
CREATE INDEX IF NOT EXISTS jobs_pending ON corpus.jobs(scope) WHERE diagnostic IS NULL;
ALTER TABLE corpus.jobs ADD COLUMN IF NOT EXISTS scheduled_at timestamptz NOT NULL DEFAULT now();
CREATE INDEX IF NOT EXISTS jobs_schedule ON corpus.jobs(scheduled_at, scope) WHERE diagnostic IS NULL;

CREATE TABLE IF NOT EXISTS corpus.candidates (
    claim_id text PRIMARY KEY,
    record text NOT NULL,
    revision bigint NOT NULL DEFAULT 1
);
CREATE TABLE IF NOT EXISTS corpus.supports (
    id text PRIMARY KEY,
    claim_id text NOT NULL REFERENCES corpus.candidates(claim_id),
    method text NOT NULL,
    engine text NOT NULL,
    evidence jsonb NOT NULL
);
CREATE INDEX IF NOT EXISTS supports_candidate ON corpus.supports(claim_id, id);
CREATE INDEX IF NOT EXISTS supports_title_evidence ON corpus.supports USING gin(evidence jsonb_path_ops) WHERE method='title-year';
CREATE TABLE IF NOT EXISTS corpus.support_inputs (
    support_id text NOT NULL REFERENCES corpus.supports(id) ON DELETE CASCADE,
    source text NOT NULL,
    revision bigint NOT NULL,
    key text NOT NULL,
    PRIMARY KEY (support_id, source, key),
    FOREIGN KEY (source, revision, key) REFERENCES ingestion.record_versions(source, revision, key)
);
CREATE INDEX IF NOT EXISTS support_input_record ON corpus.support_inputs(source, key, support_id);
CREATE INDEX IF NOT EXISTS support_input_version ON corpus.support_inputs(source, revision, key);

CREATE TABLE IF NOT EXISTS corpus.decision_events (
    version bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    claim_id text NOT NULL,
    record text NOT NULL,
    action text NOT NULL CHECK (action IN ('accepted', 'rejected', 'retracted')),
    note text NOT NULL CHECK (length(trim(note)) > 0),
    decided_at timestamptz NOT NULL DEFAULT clock_timestamp()
);
CREATE INDEX IF NOT EXISTS decision_history ON corpus.decision_events(claim_id, version);
CREATE TABLE IF NOT EXISTS corpus.decision_inputs (
    version bigint NOT NULL REFERENCES corpus.decision_events(version),
    source text NOT NULL,
    revision bigint NOT NULL,
    key text NOT NULL,
    evidence jsonb NOT NULL,
    PRIMARY KEY (version, source, key),
    FOREIGN KEY (source, revision, key) REFERENCES ingestion.record_versions(source, revision, key)
);
CREATE INDEX IF NOT EXISTS decision_input_version ON corpus.decision_inputs(source, revision, key);
CREATE TABLE IF NOT EXISTS corpus.decisions (
    claim_id text PRIMARY KEY,
    version bigint NOT NULL REFERENCES corpus.decision_events(version),
    record text NOT NULL,
    action text NOT NULL CHECK (action IN ('accepted', 'rejected', 'retracted'))
);
CREATE TABLE IF NOT EXISTS corpus.support_decisions (
    support_id text NOT NULL REFERENCES corpus.supports(id) ON DELETE CASCADE,
    version bigint NOT NULL REFERENCES corpus.decision_events(version),
    PRIMARY KEY (support_id, version)
);
CREATE TABLE IF NOT EXISTS corpus.decision_endpoints (
    claim_id text NOT NULL REFERENCES corpus.decisions(claim_id),
    uri text NOT NULL,
    PRIMARY KEY (claim_id, uri)
);
CREATE INDEX IF NOT EXISTS decision_endpoint_uri ON corpus.decision_endpoints(uri, claim_id);

CREATE SEQUENCE IF NOT EXISTS corpus.publication_generation;
CREATE TABLE IF NOT EXISTS corpus.pending_publication (
    claim_id text PRIMARY KEY REFERENCES corpus.decisions(claim_id),
    generation bigint NOT NULL DEFAULT nextval('corpus.publication_generation')
);
CREATE TABLE IF NOT EXISTS corpus.publications (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    repository text NOT NULL,
    prepared jsonb NOT NULL,
    generations jsonb NOT NULL,
    acknowledged boolean NOT NULL DEFAULT false
);
CREATE UNIQUE INDEX IF NOT EXISTS one_pending_publication ON corpus.publications(repository) WHERE NOT acknowledged;
CREATE TABLE IF NOT EXISTS corpus.publication_inputs (
    publication_id bigint NOT NULL REFERENCES corpus.publications(id) ON DELETE CASCADE,
    source text NOT NULL,
    revision bigint NOT NULL,
    key text NOT NULL,
    PRIMARY KEY (publication_id, source, key),
    FOREIGN KEY (source, revision, key) REFERENCES ingestion.record_versions(source, revision, key)
);
CREATE INDEX IF NOT EXISTS publication_input_version ON corpus.publication_inputs(source, revision, key);

CREATE TABLE IF NOT EXISTS corpus.footprints (
    claim_id text NOT NULL REFERENCES corpus.decisions(claim_id),
    origin text NOT NULL,
    target_source text NOT NULL,
    target text NOT NULL,
    PRIMARY KEY (claim_id, origin, target_source, target)
);
CREATE INDEX IF NOT EXISTS footprint_conflicts ON corpus.footprints(origin, target_source, target);