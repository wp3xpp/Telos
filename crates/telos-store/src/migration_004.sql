CREATE TABLE verification_plans (
    loop_id TEXT NOT NULL REFERENCES loops(id),
    contract_revision INTEGER NOT NULL,
    version INTEGER NOT NULL,
    body TEXT NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY(loop_id, contract_revision)
);
INSERT INTO schema_migrations(version, applied_at) VALUES(4, unixepoch());
