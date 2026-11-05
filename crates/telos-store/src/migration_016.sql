CREATE TABLE repair_deferrals (
    verification_id TEXT PRIMARY KEY REFERENCES verifications(id),
    reason TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
INSERT INTO schema_migrations(version, applied_at) VALUES(16, unixepoch());
