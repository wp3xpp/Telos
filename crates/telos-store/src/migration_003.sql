CREATE TABLE project_execution_policies (
    project_id TEXT PRIMARY KEY REFERENCES projects(id),
    revision INTEGER NOT NULL,
    body TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);
ALTER TABLE step_runs ADD COLUMN project_policy_revision INTEGER;
INSERT INTO schema_migrations(version, applied_at) VALUES(3, unixepoch());
