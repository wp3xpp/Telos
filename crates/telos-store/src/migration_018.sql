CREATE TABLE recovery_project_holds (
    project_id TEXT PRIMARY KEY REFERENCES projects(id),
    manifest_sha256 TEXT NOT NULL,
    state TEXT NOT NULL CHECK(state IN ('held','cleared')),
    proof TEXT,
    created_at INTEGER NOT NULL,
    cleared_at INTEGER
);
INSERT INTO recovery_project_holds(project_id,manifest_sha256,state,created_at)
SELECT p.id,r.manifest_sha256,'held',unixepoch() FROM projects p
CROSS JOIN recovery_preparations r WHERE r.singleton=1;
INSERT INTO events(loop_id,kind,body,created_at)
SELECT NULL,'recovery_project_holds_migrated',json_object('projects',COUNT(*)),unixepoch()
FROM recovery_project_holds HAVING COUNT(*)>0;
INSERT INTO schema_migrations(version,applied_at) VALUES(18,unixepoch());
