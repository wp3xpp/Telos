CREATE TABLE verification_test_attempts (
    lease_id TEXT PRIMARY KEY REFERENCES verification_leases(id),
    cycle_id TEXT NOT NULL REFERENCES cycles(id),
    plan_version INTEGER NOT NULL,
    created_at INTEGER NOT NULL
);
INSERT INTO schema_migrations(version, applied_at) VALUES(15, unixepoch());
