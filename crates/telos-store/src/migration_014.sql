CREATE TABLE verification_leases (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL REFERENCES projects(id),
    loop_id TEXT NOT NULL REFERENCES loops(id),
    cycle_id TEXT NOT NULL REFERENCES cycles(id),
    contract_revision INTEGER NOT NULL,
    plan_version INTEGER NOT NULL,
    state TEXT NOT NULL CHECK(state IN ('running','finished','outcome_unknown')),
    created_at INTEGER NOT NULL,
    finished_at INTEGER
);
CREATE UNIQUE INDEX verification_project_exclusive ON verification_leases(project_id)
WHERE state IN ('running','outcome_unknown');
INSERT INTO schema_migrations(version, applied_at) VALUES(14, unixepoch());
