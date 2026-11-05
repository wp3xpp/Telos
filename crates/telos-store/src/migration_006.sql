ALTER TABLE outbox ADD COLUMN lease_token TEXT;
ALTER TABLE outbox ADD COLUMN lease_until INTEGER;
ALTER TABLE outbox ADD COLUMN next_attempt_at INTEGER NOT NULL DEFAULT 0;
ALTER TABLE outbox ADD COLUMN last_error TEXT;
INSERT INTO schema_migrations(version, applied_at) VALUES(6, unixepoch());
