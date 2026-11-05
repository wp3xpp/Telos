CREATE TABLE loop_schedules (
    loop_id TEXT PRIMARY KEY REFERENCES loops(id),
    contract_revision INTEGER NOT NULL,
    policy_revision INTEGER NOT NULL,
    enabled INTEGER NOT NULL CHECK(enabled IN (0,1)),
    suspended_reason TEXT,
    updated_at INTEGER NOT NULL
);
INSERT INTO schema_migrations(version, applied_at) VALUES(5, unixepoch());
