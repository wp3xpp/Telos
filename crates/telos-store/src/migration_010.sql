CREATE TABLE delivery_workspaces (
    id TEXT PRIMARY KEY,
    cycle_id TEXT NOT NULL UNIQUE REFERENCES cycles(id),
    contract_revision INTEGER NOT NULL,
    policy_revision INTEGER NOT NULL,
    path TEXT NOT NULL UNIQUE,
    repository_git_dir TEXT NOT NULL,
    base_commit TEXT NOT NULL,
    state TEXT NOT NULL CHECK(state IN ('preparing','ready','uncertain')),
    last_error TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
INSERT INTO schema_migrations(version, applied_at) VALUES(10, unixepoch());
