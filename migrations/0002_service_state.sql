CREATE TABLE IF NOT EXISTS service_state (
    id          INTEGER PRIMARY KEY CHECK (id = 1),
    paused      INTEGER NOT NULL DEFAULT 0,
    updated_at  TEXT NOT NULL DEFAULT (datetime('now'))
);

INSERT OR IGNORE INTO service_state (id, paused) VALUES (1, 0);
