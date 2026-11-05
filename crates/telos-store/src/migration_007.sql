CREATE TABLE loop_answers (
    id TEXT PRIMARY KEY,
    loop_id TEXT NOT NULL REFERENCES loops(id),
    contract_revision INTEGER NOT NULL,
    request_key TEXT NOT NULL,
    body TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    UNIQUE(loop_id, contract_revision, request_key)
);
INSERT INTO schema_migrations(version, applied_at) VALUES(7, unixepoch());
