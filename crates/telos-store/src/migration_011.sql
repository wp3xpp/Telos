ALTER TABLE step_runs ADD COLUMN workspace_id TEXT REFERENCES delivery_workspaces(id);
INSERT INTO schema_migrations(version, applied_at) VALUES(11, unixepoch());
