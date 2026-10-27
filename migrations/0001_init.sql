CREATE TABLE IF NOT EXISTS requests (
    id            TEXT PRIMARY KEY,
    received_at   TEXT NOT NULL DEFAULT (datetime('now')),
    started_at    TEXT,
    finished_at   TEXT,
    capability    TEXT NOT NULL,
    status        TEXT NOT NULL DEFAULT 'queued',
    duration_ms   INTEGER,
    source        TEXT NOT NULL DEFAULT 'api',
    title         TEXT NOT NULL DEFAULT '',
    preview       TEXT NOT NULL DEFAULT '',
    payload_json  TEXT NOT NULL DEFAULT '{}',
    result_json   TEXT,
    summary       TEXT NOT NULL DEFAULT '',
    error         TEXT
);

CREATE INDEX IF NOT EXISTS idx_requests_received
    ON requests(received_at DESC);
CREATE INDEX IF NOT EXISTS idx_requests_capability
    ON requests(capability, received_at DESC);
