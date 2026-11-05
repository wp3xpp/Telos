CREATE TABLE log_cursors (
    cycle_id TEXT NOT NULL REFERENCES cycles(id),
    criterion_id TEXT NOT NULL,
    plan_version INTEGER NOT NULL,
    version INTEGER NOT NULL,
    body TEXT NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY(cycle_id, criterion_id, plan_version)
);
INSERT INTO schema_migrations(version, applied_at) VALUES(8, unixepoch());
