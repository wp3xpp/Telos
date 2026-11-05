CREATE TABLE delivery_reviews (
    artifact TEXT PRIMARY KEY REFERENCES delivery_snapshots(artifact),
    evidence_id TEXT NOT NULL REFERENCES evidence(id),
    created_at INTEGER NOT NULL
);
INSERT INTO schema_migrations(version, applied_at) VALUES(13, unixepoch());
