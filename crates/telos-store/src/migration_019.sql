CREATE TABLE recovery_activations (
    singleton INTEGER PRIMARY KEY CHECK(singleton=1),
    manifest_sha256 TEXT NOT NULL,
    source_receipt_sha256 TEXT NOT NULL,
    target_state TEXT NOT NULL,
    proof TEXT NOT NULL,
    preflight TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
INSERT INTO schema_migrations(version, applied_at) VALUES(19, unixepoch());
