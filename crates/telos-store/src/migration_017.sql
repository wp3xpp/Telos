CREATE TABLE recovery_preparations (
    singleton INTEGER PRIMARY KEY CHECK(singleton=1),
    manifest_sha256 TEXT NOT NULL,
    database_sha256 TEXT NOT NULL,
    original_state TEXT NOT NULL,
    state TEXT NOT NULL CHECK(state IN ('prepared')),
    report TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
INSERT INTO schema_migrations(version, applied_at) VALUES(17, unixepoch());
