ALTER TABLE verifications ADD COLUMN plan_version INTEGER;
INSERT INTO schema_migrations(version, applied_at) VALUES(9, unixepoch());
