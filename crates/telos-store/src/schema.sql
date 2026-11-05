CREATE TABLE IF NOT EXISTS schema_migrations (
    version INTEGER PRIMARY KEY, applied_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS projects (
    id TEXT PRIMARY KEY, root TEXT NOT NULL UNIQUE, created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS loops (
    id TEXT PRIMARY KEY, project_id TEXT NOT NULL REFERENCES projects(id),
    name TEXT NOT NULL, kind TEXT NOT NULL CHECK(kind IN ('delivery','continuous')),
    state TEXT NOT NULL CHECK(state IN ('active','paused','blocked','completed','stopped')),
    revision INTEGER NOT NULL, version INTEGER NOT NULL,
    next_due_at INTEGER, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS contracts (
    loop_id TEXT NOT NULL REFERENCES loops(id), revision INTEGER NOT NULL,
    body TEXT NOT NULL, created_at INTEGER NOT NULL,
    PRIMARY KEY(loop_id, revision)
);
CREATE TABLE IF NOT EXISTS cycles (
    id TEXT PRIMARY KEY, loop_id TEXT NOT NULL REFERENCES loops(id),
    contract_revision INTEGER NOT NULL,
    state TEXT NOT NULL CHECK(state IN ('observing','acting','verifying','blocked','closed','cancelled')),
    trigger_key TEXT NOT NULL, conclusion TEXT,
    created_at INTEGER NOT NULL, closed_at INTEGER,
    UNIQUE(loop_id, trigger_key)
);
CREATE UNIQUE INDEX IF NOT EXISTS one_open_cycle ON cycles(loop_id)
    WHERE state NOT IN ('closed','cancelled');
CREATE TABLE IF NOT EXISTS work_items (
    id TEXT PRIMARY KEY, cycle_id TEXT NOT NULL REFERENCES cycles(id),
    goal TEXT NOT NULL, state TEXT NOT NULL, created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS step_runs (
    id TEXT PRIMARY KEY, work_item_id TEXT NOT NULL REFERENCES work_items(id),
    contract_revision INTEGER NOT NULL, state TEXT NOT NULL,
    attempt INTEGER NOT NULL DEFAULT 0, lease_generation INTEGER NOT NULL DEFAULT 0,
    worker_id TEXT, lease_until INTEGER, result TEXT, created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS evidence (
    id TEXT PRIMARY KEY, cycle_id TEXT NOT NULL REFERENCES cycles(id),
    contract_revision INTEGER NOT NULL, artifact TEXT NOT NULL, environment TEXT NOT NULL,
    path TEXT NOT NULL, sha256 TEXT NOT NULL, byte_length INTEGER NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS verifications (
    id TEXT PRIMARY KEY, cycle_id TEXT NOT NULL REFERENCES cycles(id),
    contract_revision INTEGER NOT NULL, verdict TEXT NOT NULL,
    body TEXT NOT NULL, created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS events (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT, loop_id TEXT REFERENCES loops(id),
    kind TEXT NOT NULL, body TEXT NOT NULL, created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS outbox (
    id INTEGER PRIMARY KEY AUTOINCREMENT, event_sequence INTEGER NOT NULL UNIQUE REFERENCES events(sequence),
    state TEXT NOT NULL DEFAULT 'pending', attempts INTEGER NOT NULL DEFAULT 0
);
INSERT OR IGNORE INTO schema_migrations(version, applied_at) VALUES(1, unixepoch());
