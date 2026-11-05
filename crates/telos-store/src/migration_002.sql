ALTER TABLE step_runs ADD COLUMN spec TEXT NOT NULL DEFAULT '{}';
ALTER TABLE step_runs ADD COLUMN due_at INTEGER NOT NULL DEFAULT 0;
ALTER TABLE step_runs ADD COLUMN finished_at INTEGER;
CREATE INDEX ready_steps ON step_runs(state,due_at,created_at);
CREATE TABLE enqueue_keys (
    cycle_id TEXT NOT NULL REFERENCES cycles(id), request_key TEXT NOT NULL,
    step_id TEXT NOT NULL REFERENCES step_runs(id),
    PRIMARY KEY(cycle_id,request_key)
);
INSERT INTO schema_migrations(version,applied_at) VALUES(2,unixepoch());
