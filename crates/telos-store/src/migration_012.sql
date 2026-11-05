CREATE TABLE delivery_snapshots (
    artifact TEXT PRIMARY KEY,
    workspace_id TEXT NOT NULL REFERENCES delivery_workspaces(id),
    plan_version INTEGER NOT NULL,
    step_count INTEGER NOT NULL,
    manifest TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
INSERT INTO schema_migrations(version, applied_at) VALUES(12, unixepoch());
