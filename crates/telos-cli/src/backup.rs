//! Offline, verifiable state snapshots. Restore needs separate rollback/replay policy.
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::{sqlite::SqliteConnectOptions, Connection, Row, SqliteConnection};
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::Path,
};

fn regular(path: &Path) -> Result<File> {
    ensure!(
        std::fs::symlink_metadata(path)?.is_file(),
        "backup input must be a regular file"
    );
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    ensure!(
        file.metadata()?.is_file(),
        "backup input must be a regular file"
    );
    Ok(file)
}
fn inventory(
    root: &Path,
    path: &Path,
    files: &mut BTreeMap<String, Value>,
    depth: usize,
) -> Result<()> {
    inventory_under(root, path, files, depth, &root.join("workspaces"))
}
fn inventory_under(
    root: &Path,
    path: &Path,
    files: &mut BTreeMap<String, Value>,
    depth: usize,
    workspaces: &Path,
) -> Result<()> {
    ensure!(depth < 64, "backup directory nesting exceeds limit");
    for item in std::fs::read_dir(path)? {
        let item = item?;
        let path = item.path();
        let kind = item.file_type()?;
        if kind.is_symlink() {
            ensure!(
                preserved_link(&path, workspaces),
                "symlink outside preserved workspace/runtime namespaces: {}",
                path.display()
            );
            let target = std::fs::read_link(&path)?;
            let target_bytes = target.as_os_str().as_encoded_bytes();
            files.insert(path.strip_prefix(root)?.to_str().context("non-UTF8 backup path")?.to_owned(),json!({"type":"symlink","target_bytes":target_bytes,"sha256":format!("{:x}",Sha256::digest(target_bytes)),"bytes":target_bytes.len()}));
            ensure!(files.len() <= 100_000, "backup file count exceeds limit");
        } else if kind.is_dir() {
            inventory_under(root, &path, files, depth + 1, workspaces)?;
        } else {
            let mut f =
                regular(&path).with_context(|| format!("inventory file {}", path.display()))?;
            let mut h = Sha256::new();
            let mut size = 0u64;
            let mut buf = [0u8; 65536];
            loop {
                let n = f.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                h.update(&buf[..n]);
                size += n as u64;
            }
            let relative = path
                .strip_prefix(root)?
                .to_str()
                .context("non-UTF8 backup path")?
                .to_owned();
            let mut entry = json!({"sha256":format!("{:x}",h.finalize()),"bytes":size});
            if f.metadata()?.permissions().mode() & 0o111 != 0 {
                entry["executable"] = json!(true);
            }
            files.insert(relative, entry);
            ensure!(files.len() <= 100_000, "backup file count exceeds limit");
        }
    }
    Ok(())
}
fn copy_tree(source: &Path, dest: &Path, depth: usize, exclude_database: bool) -> Result<()> {
    copy_tree_under(source, source, dest, depth, exclude_database)
}
fn workspace_link(path: &Path, workspaces: &Path) -> bool {
    path.strip_prefix(workspaces)
        .is_ok_and(|relative| relative.components().count() >= 2)
}
// Codex sandbox runs create bootstrap links under their private temporary home.
// Preserve only link bytes, as with workspaces; never follow or copy the target.
fn preserved_link(path: &Path, workspaces: &Path) -> bool {
    if workspace_link(path, workspaces) {
        return true;
    }
    let Some(state) = workspaces.parent() else {
        return false;
    };
    [
        state.join("runs"),
        state.join("recovery-history/original-runs"),
    ]
    .iter()
    .any(|runs| {
        path.strip_prefix(runs).is_ok_and(|relative| {
            let parts: Vec<_> = relative.components().collect();
            parts.len() >= 4
                && matches!(parts[0], std::path::Component::Normal(_))
                && parts[1].as_os_str() == "codex-home"
                && parts[2].as_os_str() == "tmp"
        })
    })
}

fn copy_tree_under(
    root: &Path,
    source: &Path,
    dest: &Path,
    depth: usize,
    exclude_database: bool,
) -> Result<()> {
    ensure!(depth < 64, "backup directory nesting exceeds limit");
    for item in std::fs::read_dir(source)? {
        let item = item?;
        let name = item.file_name();
        if exclude_database
            && depth == 0
            && [
                "daemon.lock",
                "service.lock",
                "telos.db",
                "telos.db-wal",
                "telos.db-shm",
            ]
            .iter()
            .any(|n| name == *n)
        {
            continue;
        }
        let kind = item.file_type()?;
        let to = dest.join(&name);
        if kind.is_symlink() {
            ensure!(
                preserved_link(&item.path(), &root.join("workspaces")),
                "symlink outside preserved workspace/runtime namespaces: {}",
                item.path().display()
            );
            std::os::unix::fs::symlink(std::fs::read_link(item.path())?, &to)?;
        } else if kind.is_dir() {
            std::fs::create_dir(&to)?;
            std::fs::set_permissions(&to, std::fs::Permissions::from_mode(0o700))?;
            copy_tree_under(root, &item.path(), &to, depth + 1, exclude_database)?;
            File::open(&to)?.sync_all()?;
        } else {
            let mut input = regular(&item.path())
                .with_context(|| format!("copy source file {}", item.path().display()))?;
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&to)?;
            std::io::copy(&mut input, &mut output)?;
            if input.metadata()?.permissions().mode() & 0o111 != 0 {
                output.set_permissions(std::fs::Permissions::from_mode(0o700))?;
            }
            output.sync_all()?;
        }
    }
    Ok(())
}
async fn inspect(db: &Path) -> Result<Value> {
    regular(db)?;
    let mut connection = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(db)
            .read_only(true)
            .immutable(true),
    )
    .await?;
    let result: Result<Value> = async {
        let rows: Vec<String> = sqlx::query_scalar("PRAGMA integrity_check")
            .fetch_all(&mut connection)
            .await?;
        ensure!(rows == ["ok"], "backup database integrity check failed");
        let foreign_keys: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pragma_foreign_key_check")
            .fetch_one(&mut connection)
            .await?;
        ensure!(
            foreign_keys == 0,
            "backup database foreign key check failed"
        );
        let version: i64 = sqlx::query_scalar("SELECT MAX(version) FROM schema_migrations")
            .fetch_one(&mut connection)
            .await?;
        ensure!((1..=19).contains(&version), "unsupported backup schema");
        let projects: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM projects")
            .fetch_one(&mut connection)
            .await?;
        let goals: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM loops")
            .fetch_one(&mut connection)
            .await?;
        Ok(json!({"schema":version,"projects":projects,"goals":goals}))
    }
    .await;
    connection.close().await?;
    result
}
pub async fn create(state: &Path, destination: &Path) -> Result<Value> {
    let state = state.canonicalize()?;
    ensure!(
        destination.is_absolute(),
        "backup destination must be absolute"
    );
    let parent = destination
        .parent()
        .context("backup parent missing")?
        .canonicalize()?;
    let destination = parent.join(destination.file_name().context("backup name missing")?);
    ensure!(
        !destination.starts_with(&state),
        "backup must be outside state directory"
    );
    ensure!(
        !destination.try_exists()?,
        "backup destination already exists"
    );
    let _operation = crate::service::operation_lock(&state)?;
    let _daemon = telos_runtime::daemon::maintenance_lock(&state)
        .context("stop/remove background service before backup")?;
    regular(&state.join("telos.db"))?;
    // A partial directory is intentionally retained on failure. Only a manifest marks completion.
    std::fs::create_dir(&destination)?;
    std::fs::set_permissions(&destination, std::fs::Permissions::from_mode(0o700))?;
    let payload = destination.join("state");
    std::fs::create_dir(&payload)?;
    std::fs::set_permissions(&payload, std::fs::Permissions::from_mode(0o700))?;
    let mut connection = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(state.join("telos.db"))
            .read_only(true),
    )
    .await?;
    let result = sqlx::query("VACUUM INTO ?")
        .bind(
            payload
                .join("telos.db")
                .to_str()
                .context("non-UTF8 backup path")?,
        )
        .execute(&mut connection)
        .await;
    connection.close().await?;
    result?;
    std::fs::set_permissions(
        payload.join("telos.db"),
        std::fs::Permissions::from_mode(0o600),
    )?;
    copy_tree(&state, &payload, 0, true)?;
    let database = inspect(&payload.join("telos.db")).await?;
    let mut files = BTreeMap::new();
    inventory(&payload, &payload, &mut files, 0)?;
    let manifest = json!({"format":2,"source_state":state,"created_at":telos_store::now(),"database":database,"files":files,"scope":"Telos state only; external projects/worktrees and credentials excluded"});
    File::open(payload.join("telos.db"))?.sync_all()?;
    File::open(&payload)?.sync_all()?;
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
    ensure!(
        manifest_bytes.len() <= 32 * 1024 * 1024,
        "backup manifest too large"
    );
    let mut f = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(destination.join("manifest.json"))?;
    f.write_all(&manifest_bytes)?;
    f.sync_all()?;
    File::open(&destination)?.sync_all()?;
    File::open(parent)?.sync_all()?;
    Ok(
        json!({"created":true,"backup":destination,"database":database,"files":files.len(),"restore_supported":false}),
    )
}
pub async fn verify(backup: &Path) -> Result<Value> {
    let backup = backup.canonicalize()?;
    let mut bytes = Vec::new();
    regular(&backup.join("manifest.json"))?
        .take(32 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 32 * 1024 * 1024, "backup manifest too large");
    let manifest: Value = serde_json::from_slice(&bytes)?;
    ensure!(
        matches!(manifest["format"].as_i64(), Some(1 | 2)),
        "unsupported backup format"
    );
    let payload = backup.join("state");
    ensure!(
        std::fs::symlink_metadata(&payload)?.is_dir(),
        "backup payload must be a directory"
    );
    let mut files = BTreeMap::new();
    inventory(&payload, &payload, &mut files, 0)?;
    ensure!(
        serde_json::to_value(&files)? == manifest["files"],
        "backup files differ from manifest"
    );
    let database = inspect(&payload.join("telos.db")).await?;
    ensure!(
        database == manifest["database"],
        "backup database differs from manifest"
    );
    Ok(
        json!({"verified":true,"backup":backup,"source_state":manifest["source_state"],"database":database,"files":files.len(),"restore_supported":false}),
    )
}

/// Reconstruct a verified recovery image without authorizing execution or replacing state.
pub async fn stage_restore(backup: &Path, destination: &Path) -> Result<Value> {
    let verified = verify(backup).await?;
    let backup = backup.canonicalize()?;
    ensure!(
        destination.is_absolute(),
        "restore destination must be absolute"
    );
    let parent = destination
        .parent()
        .context("restore parent missing")?
        .canonicalize()?;
    let destination = parent.join(destination.file_name().context("restore name missing")?);
    ensure!(
        !destination.starts_with(&backup),
        "restore must be outside backup"
    );
    ensure!(
        !destination.try_exists()?,
        "restore destination already exists"
    );
    let mut bytes = Vec::new();
    regular(&backup.join("manifest.json"))?
        .take(32 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 32 * 1024 * 1024, "backup manifest too large");
    let manifest: Value = serde_json::from_slice(&bytes)?;
    ensure!(
        manifest["files"].get("restore-required.json").is_none(),
        "backup already contains a recovery gate"
    );
    let stage = parent.join(format!(
        ".telos-restore-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
    ));
    std::fs::create_dir(&stage)?;
    std::fs::set_permissions(&stage, std::fs::Permissions::from_mode(0o700))?;
    // Keep failures inspectable, including incomplete copies. Never publish unverified data.
    let payload = stage.join("state");
    std::fs::create_dir(&payload)?;
    std::fs::set_permissions(&payload, std::fs::Permissions::from_mode(0o700))?;
    copy_tree(&backup.join("state"), &payload, 0, false)?;
    let mut files = BTreeMap::new();
    inventory(&payload, &payload, &mut files, 0)?;
    ensure!(
        serde_json::to_value(&files)? == manifest["files"],
        "copied restore files differ from manifest"
    );
    ensure!(
        inspect(&payload.join("telos.db")).await? == verified["database"],
        "restored database differs"
    );
    let mut saved_manifest = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(stage.join("backup-manifest.json"))?;
    saved_manifest.write_all(&bytes)?;
    saved_manifest.sync_all()?;
    let gate = json!({"format":1,"state":"reconciliation_required","backup":backup,"manifest_sha256":format!("{:x}",Sha256::digest(&bytes)),"original_state":manifest["source_state"],"staged_at":telos_store::now(),"reason":"Snapshot rollback does not establish external outcomes. Reconcile execution, notifications, workspaces and absolute artifact paths before activation."});
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(stage.join("restore-required.json"))?;
    f.write_all(&serde_json::to_vec_pretty(&gate)?)?;
    f.sync_all()?;
    let mut inner = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(payload.join("restore-required.json"))?;
    inner.write_all(&serde_json::to_vec_pretty(&gate)?)?;
    inner.sync_all()?;
    File::open(&payload)?.sync_all()?;
    File::open(&stage)?.sync_all()?;
    publish_exclusive(&stage, &destination)?;
    File::open(&parent)?.sync_all()?;
    Ok(
        json!({"staged":true,"destination":destination,"database":verified["database"],"execution_enabled":false,"activation_supported":true,"original_state":manifest["source_state"]}),
    )
}
fn read_json_bounded(path: &Path, limit: u64) -> Result<(Vec<u8>, Value)> {
    let mut bytes = Vec::new();
    regular(path)?.take(limit + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= limit, "recovery metadata too large");
    let value = serde_json::from_slice(&bytes)?;
    Ok((bytes, value))
}
/// Read the copied ledger only. Never probe/kill recorded processes or inspect external paths.
pub async fn inspect_restore(recovery: &Path) -> Result<Value> {
    let recovery = recovery.canonicalize()?;
    let payload = recovery.join("state");
    ensure!(
        std::fs::symlink_metadata(&payload)?.is_dir(),
        "recovery payload must be a directory"
    );
    let (_, gate) = read_json_bounded(&recovery.join("restore-required.json"), 65536)?;
    let (_, inner) = read_json_bounded(&payload.join("restore-required.json"), 65536)?;
    ensure!(
        gate == inner && gate["format"] == 1 && gate["state"] == "reconciliation_required",
        "recovery markers differ or unsupported state"
    );
    let (bytes, manifest) =
        read_json_bounded(&recovery.join("backup-manifest.json"), 32 * 1024 * 1024)?;
    ensure!(
        format!("{:x}", Sha256::digest(&bytes)) == gate["manifest_sha256"],
        "recovery manifest changed"
    );
    ensure!(
        matches!(manifest["format"].as_i64(), Some(1 | 2))
            && manifest["source_state"] == gate["original_state"],
        "recovery manifest identity differs"
    );
    let mut files = BTreeMap::new();
    inventory(&payload, &payload, &mut files, 0)?;
    for local in ["restore-required.json", "daemon.lock", "service.lock"] {
        files.remove(local);
    }
    ensure!(
        serde_json::to_value(&files)? == manifest["files"],
        "recovery payload changed; restage or reconcile the change explicitly"
    );
    let database = inspect(&payload.join("telos.db")).await?;
    ensure!(
        database == manifest["database"] && matches!(database["schema"].as_i64(), Some(16..=19)),
        "recovery review currently requires unchanged schema 16 through 19"
    );
    let original = Path::new(
        manifest["source_state"]
            .as_str()
            .context("original state missing")?,
    );
    ensure!(original.is_absolute(), "original state must be absolute");
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(payload.join("telos.db"))
            .read_only(true)
            .immutable(true),
    )
    .await?;
    let result: Result<Value> = async {
        let mut sections = serde_json::Map::new();
        // SQL is static; output is bounded independently for each category.
        for (name, count_sql, rows_sql) in [
            ("goals", "SELECT COUNT(*) FROM loops", "SELECT id, state AS detail FROM loops ORDER BY id LIMIT 100"),
            ("steps", "SELECT COUNT(*) FROM step_runs", "SELECT id, state AS detail FROM step_runs ORDER BY id LIMIT 100"),
            ("verification_leases", "SELECT COUNT(*) FROM verification_leases", "SELECT id, state AS detail FROM verification_leases ORDER BY id LIMIT 100"),
            ("notifications", "SELECT COUNT(*) FROM outbox", "SELECT CAST(id AS TEXT) AS id, state AS detail FROM outbox ORDER BY id LIMIT 100"),
            ("schedules", "SELECT COUNT(*) FROM loop_schedules", "SELECT loop_id AS id, CASE enabled WHEN 1 THEN 'enabled' ELSE 'disabled' END AS detail FROM loop_schedules ORDER BY loop_id LIMIT 100"),
            ("projects", "SELECT COUNT(*) FROM projects", "SELECT id, root AS detail FROM projects ORDER BY id LIMIT 100"),
            ("workspaces", "SELECT COUNT(*) FROM delivery_workspaces", "SELECT id, path AS detail FROM delivery_workspaces ORDER BY id LIMIT 100"),
            ("execution_policies", "SELECT COUNT(*) FROM project_execution_policies", "SELECT project_id AS id, CAST(revision AS TEXT) AS detail FROM project_execution_policies ORDER BY project_id LIMIT 100"),
        ] {
            let count: i64 = sqlx::query_scalar(count_sql).fetch_one(&mut db).await?;
            let rows = sqlx::query(rows_sql).fetch_all(&mut db).await?;
            let records = rows.iter().map(|r| json!({"id":r.get::<String,_>("id"),"detail":r.get::<String,_>("detail")})).collect::<Vec<_>>();
            sections.insert(name.into(), json!({"total":count,"truncated":count>100,"records":records}));
        }
        let evidence = review_evidence(&mut db, &files, &payload).await?;
        sections.insert("evidence".into(), evidence.clone());
        let queued: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM step_runs WHERE state IN ('pending','ready','running','outcome_unknown')").fetch_one(&mut db).await?;
        let leases: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM verification_leases WHERE state IN ('running','outcome_unknown')").fetch_one(&mut db).await?;
        let notices: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM outbox WHERE state NOT IN ('sent','superseded','failed')").fetch_one(&mut db).await?;
        Ok(json!({"verified_image":true,"execution_enabled":false,"activation_supported":true,"manifest_sha256":gate["manifest_sha256"],"database_sha256":manifest["files"]["telos.db"]["sha256"],"original_state":original,"recovery":recovery,"database":database,"inventory":sections,"evidence_integrity_complete":evidence["integrity_complete"],"snapshot_risks":{"queued_running_or_unknown_steps":queued,"unresolved_verification_leases":leases,"potential_notification_replay":notices},"required_reconciliation":["Establish current external outcomes; snapshot status cannot prove what happened after backup.","Review every project execution grant and external workspace before reactivation.","Reconcile absolute workspace and run-report paths; evidence paths remain relative to state.","Settle queued/running work and notification replay without resetting attempt budgets.","Preserve unknown outcomes until independent evidence establishes their disposition."],"scope":"Copied ledger only. No external processes, project paths, services or credentials were accessed. Counts include all records; each inventory list is capped at 100."}))
    }.await;
    db.close().await?;
    result
}

/// Check every evidence binding against the already hashed local inventory, without external reads.
async fn review_evidence(
    db: &mut SqliteConnection,
    files: &BTreeMap<String, Value>,
    payload: &Path,
) -> Result<Value> {
    let mut after: Option<String> = None;
    let mut records = Vec::new();
    let mut checked = 0u64;
    let mut invalid = 0u64;
    loop {
        let rows = sqlx::query(
            "SELECT id,path,sha256,byte_length FROM evidence WHERE (? IS NULL OR id>?) ORDER BY id LIMIT 1000",
        )
        .bind(&after)
        .bind(&after)
        .fetch_all(&mut *db)
        .await?;
        if rows.is_empty() {
            break;
        }
        for row in rows {
            let id: String = row.get("id");
            let path: String = row.get("path");
            let hash: String = row.get("sha256");
            let length: i64 = row.get("byte_length");
            let valid_path = !id.contains(['/', '\\']) && path == format!("evidence/{id}.bin");
            let status = if !valid_path {
                "unsupported_stored_path"
            } else if let Some(file) = files.get(&path) {
                if (1..=8 * 1024 * 1024).contains(&length)
                    && file["bytes"].as_u64() == Some(length as u64)
                    && file["sha256"] == hash
                {
                    "verified"
                } else {
                    "binding_mismatch"
                }
            } else {
                "missing_file"
            };
            checked += 1;
            if status != "verified" {
                invalid += 1;
            }
            if records.len() < 100 {
                records.push(json!({"id":id,"stored_path":path,"integrity":status,"mapping":if valid_path { json!({"kind":"state_relative","copied_candidate":payload.join(&path),"external_state_checked":false}) } else {json!({"kind":"unsupported_stored_path","external_state_checked":false})}}));
            }
            after = Some(id);
        }
    }
    Ok(
        json!({"total":checked,"checked":checked,"invalid":invalid,"integrity_complete":invalid==0,"truncated":checked>100,"records":records,"scope":"Local evidence bindings only, not factual correctness or external outcome confirmation."}),
    )
}

/// Explicit operator check: touches only the requested candidate and its Git identity metadata.
pub async fn check_workspace(
    recovery: &Path,
    workspace_id: &str,
    candidate: &Path,
    expected_manifest: &str,
    expected_database: &str,
) -> Result<Value> {
    let review = inspect_restore(recovery).await?;
    ensure!(
        review["manifest_sha256"] == expected_manifest
            && review["database_sha256"] == expected_database,
        "reviewed recovery fingerprints changed"
    );
    ensure!(
        candidate.is_absolute(),
        "workspace candidate must be absolute"
    );
    let payload = recovery.canonicalize()?.join("state");
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(payload.join("telos.db"))
            .read_only(true)
            .immutable(true),
    )
    .await?;
    let result = sqlx::query("SELECT * FROM delivery_workspaces WHERE id=?")
        .bind(workspace_id)
        .fetch_optional(&mut db)
        .await;
    db.close().await?;
    let row = result?.context("workspace not recorded in recovery image")?;
    let recorded_path: String = row.get("path");
    let workspace = telos_core::DeliveryWorkspace {
        id: row.get("id"),
        cycle_id: row.get("cycle_id"),
        contract_revision: row.get("contract_revision"),
        policy_revision: row.get("policy_revision"),
        path: candidate
            .to_str()
            .context("non-UTF8 candidate path")?
            .to_owned(),
        repository_git_dir: row.get("repository_git_dir"),
        base_commit: row.get("base_commit"),
        state: row.get("state"),
        last_error: row.get("last_error"),
    };
    telos_runtime::workspace::validate_identity(&workspace).await?;
    Ok(
        json!({"identity_verified":true,"workspace_id":workspace_id,"recorded_path":recorded_path,"candidate":candidate,"repository_git_dir":workspace.repository_git_dir,"base_commit":workspace.base_commit,"manifest_sha256":expected_manifest,"database_sha256":expected_database,"observed_at":telos_store::now(),"execution_enabled":false,"scope":"Read-only Git identity/backlink/base check. Does not establish artifact equality, external execution outcomes, current test acceptance or activation permission."}),
    )
}

fn preparation_inventory(root: &Path) -> Result<BTreeMap<String, Value>> {
    let mut files = BTreeMap::new();
    inventory_under(root, root, &mut files, 0, &root.join("state/workspaces"))?;
    for path in [
        "receipt.json",
        "daemon.lock",
        "service.lock",
        "state/daemon.lock",
        "state/service.lock",
    ] {
        files.remove(path);
    }
    Ok(files)
}
async fn prepared_receipt(root: &Path, manifest: &str, database: &str) -> Result<Value> {
    ensure!(
        std::fs::symlink_metadata(root)?.is_dir(),
        "prepared image must be a directory"
    );
    let (_, receipt) = read_json_bounded(&root.join("receipt.json"), 32 * 1024 * 1024)?;
    ensure!(
        receipt["format"] == 1
            && receipt["manifest_sha256"] == manifest
            && receipt["database_sha256"] == database,
        "prepared receipt fingerprint mismatch"
    );
    ensure!(
        serde_json::to_value(preparation_inventory(root)?)? == receipt["files"],
        "prepared image changed"
    );
    inspect(&root.join("state/telos.db")).await?;
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(root.join("state/telos.db"))
            .read_only(true)
            .immutable(true),
    )
    .await?;
    let result: Result<()> = async {
        let row=sqlx::query("SELECT manifest_sha256,database_sha256,state,report FROM recovery_preparations WHERE singleton=1").fetch_one(&mut db).await?;
        ensure!(row.get::<String,_>("manifest_sha256")==manifest && row.get::<String,_>("database_sha256")==database && row.get::<String,_>("state")=="prepared", "prepared ledger identity mismatch");
        ensure!(serde_json::from_str::<Value>(&row.get::<String,_>("report"))? == receipt["report"], "prepared report differs from ledger");
        if let Some(proof) = receipt.get("operator_review") {
            let review: telos_core::RecoveryProjectReview = serde_json::from_value(proof.clone())?;
            review.validate()?;
            ensure!(review.manifest_sha256 == manifest, "review manifest mismatch");
            let hold = sqlx::query("SELECT state,proof FROM recovery_project_holds WHERE project_id=?")
                .bind(&review.project_id).fetch_one(&mut db).await?;
            ensure!(hold.get::<String,_>("state") == "cleared"
                && serde_json::from_str::<Value>(&hold.get::<String,_>("proof"))? == *proof,
                "review receipt differs from project ledger");
            ensure!(receipt["review_result"] == json!({"project_id":review.project_id,"state":"cleared","decision_id":review.decision_id,"execution_started":false,"service_activated":false}), "review result mismatch");
        }
        let environment_decisions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM events WHERE kind='environment_operation_reconciled' AND sequence>(SELECT MAX(sequence) FROM events WHERE kind='recovery_prepared') AND json_extract(body,'$.recovery_review.manifest_sha256')=?")
            .bind(manifest).fetch_one(&mut db).await?;
        let claimed_environment_decisions = match receipt.get("environment_dispositions") {
            Some(records) => records.as_array().context("environment dispositions must be an array")?.len(),
            None => 0,
        };
        ensure!(environment_decisions == claimed_environment_decisions as i64,
            "environment disposition receipt is incomplete");
        let mut environment_ids = std::collections::BTreeSet::new();
        if let Some(records) = receipt.get("environment_dispositions") {
            for record in records.as_array().context("environment dispositions must be an array")? {
                let disposition: telos_core::RecoveryEnvironmentDisposition = serde_json::from_value(record["disposition"].clone())?;
                disposition.validate()?;
                ensure!(environment_ids.insert(disposition.session.decision_id.clone()), "duplicate environment disposition receipt");
                ensure!(disposition.review.manifest_sha256 == manifest, "environment disposition snapshot mismatch");
                let rows = sqlx::query("SELECT e.body,l.project_id FROM events e JOIN loops l ON l.id=e.loop_id WHERE e.kind='environment_operation_reconciled' AND e.sequence>(SELECT MAX(sequence) FROM events WHERE kind='recovery_prepared') AND json_extract(e.body,'$.review.decision_id')=? LIMIT 2")
                    .bind(&disposition.session.decision_id).fetch_all(&mut db).await?;
                ensure!(rows.len() == 1, "environment disposition audit missing or ambiguous");
                let audit: Value = serde_json::from_str(rows[0].get::<&str,_>("body"))?;
                ensure!(rows[0].get::<&str,_>("project_id") == disposition.review.project_id
                    && audit["recovery_review"] == record["disposition"]["review"]
                    && audit["review"] == record["disposition"]["session"]
                    && audit == record["result"] && audit["phase"] == "retired",
                    "environment disposition receipt differs from audit ledger");
            }
        }
        if let Some(record) = receipt.get("execution_disposition") {
            let disposition: telos_core::RecoveryExecutionDisposition = serde_json::from_value(record["disposition"].clone())?;
            disposition.validate()?;
            let review = &disposition.review;
            ensure!(review.manifest_sha256 == manifest, "disposition snapshot mismatch");
            let rows = sqlx::query("SELECT body FROM events WHERE kind='recovery_executions_discarded' AND json_extract(body,'$.project_id')=? AND json_extract(body,'$.decision_id')=? LIMIT 2")
                .bind(&review.project_id).bind(&review.decision_id).fetch_all(&mut db).await?;
            ensure!(rows.len() == 1 && serde_json::from_str::<Value>(&rows[0].get::<String,_>("body"))?["proof"] == record["disposition"], "disposition receipt differs from audit ledger");
            ensure!(record["result"] == json!({"project_id":review.project_id,"decision_id":review.decision_id,"step_ids":disposition.step_ids,"disposition":"discarded","automatic_retry":false,"task_success_asserted":false,"service_activated":false}), "disposition result mismatch");
            for step in &disposition.step_ids {
                let state: String = sqlx::query_scalar("SELECT s.state FROM step_runs s JOIN work_items w ON w.id=s.work_item_id JOIN cycles c ON c.id=w.cycle_id JOIN loops l ON l.id=c.loop_id WHERE s.id=? AND l.project_id=?")
                    .bind(step).bind(&review.project_id).fetch_one(&mut db).await?;
                ensure!(state == "cancelled", "disposed step state mismatch");
            }
        }
        if let Some(record) = receipt.get("verification_disposition") {
            let disposition: telos_core::RecoveryVerificationDisposition = serde_json::from_value(record["disposition"].clone())?;
            disposition.validate()?;
            let review = &disposition.review;
            ensure!(review.manifest_sha256 == manifest, "disposition snapshot mismatch");
            let rows = sqlx::query("SELECT body FROM events WHERE kind='recovery_verifications_discarded' AND json_extract(body,'$.project_id')=? AND json_extract(body,'$.decision_id')=? LIMIT 2")
                .bind(&review.project_id).bind(&review.decision_id).fetch_all(&mut db).await?;
            ensure!(rows.len() == 1 && serde_json::from_str::<Value>(&rows[0].get::<String,_>("body"))?["proof"] == record["disposition"], "disposition receipt differs from audit ledger");
            ensure!(record["result"] == json!({"project_id":review.project_id,"decision_id":review.decision_id,"lease_ids":disposition.lease_ids,"disposition":"discarded","automatic_retry":false,"task_success_asserted":false,"service_activated":false}), "disposition result mismatch");
            for lease in &disposition.lease_ids {
                let state: String = sqlx::query_scalar("SELECT state FROM verification_leases WHERE id=? AND project_id=?")
                    .bind(lease).bind(&review.project_id).fetch_one(&mut db).await?;
                ensure!(state == "finished", "disposed verification state mismatch");
            }
        }
        if let Some(record) = receipt.get("workspace_adoption") {
            let adoption: telos_core::RecoveryWorkspaceAdoption = serde_json::from_value(record["adoption"].clone())?;
            adoption.validate()?;
            ensure!(adoption.review.manifest_sha256 == manifest, "workspace adoption snapshot mismatch");
            let rows = sqlx::query("SELECT body FROM events WHERE kind='recovery_workspace_adopted' AND json_extract(body,'$.project_id')=? AND json_extract(body,'$.decision_id')=? LIMIT 2")
                .bind(&adoption.review.project_id).bind(&adoption.review.decision_id).fetch_all(&mut db).await?;
            ensure!(rows.len() == 1, "workspace adoption audit missing or ambiguous");
            let audit: Value = serde_json::from_str(&rows[0].get::<String,_>("body"))?;
            ensure!(audit["adoption"] == record["adoption"] && format!("{:x}", Sha256::digest(serde_json::to_vec(&audit["snapshot"])?)) == adoption.expected_snapshot_sha256, "workspace adoption proof mismatch");
            let row = sqlx::query("SELECT d.*,l.project_id,p.version AS plan_version FROM delivery_workspaces d JOIN cycles c ON c.id=d.cycle_id JOIN loops l ON l.id=c.loop_id LEFT JOIN verification_plans p ON p.loop_id=l.id AND p.contract_revision=d.contract_revision WHERE d.id=?")
                .bind(&adoption.workspace_id).fetch_one(&mut db).await?;
            ensure!(row.get::<String,_>("project_id") == adoption.review.project_id && row.get::<String,_>("path") == adoption.candidate_path && row.get::<String,_>("state") == "ready"
                && json!(row.get::<Option<i64>,_>("plan_version")) == audit["next_plan_version"], "adopted workspace binding or plan changed");
            for column in ["id", "cycle_id", "repository_git_dir", "base_commit"] {
                ensure!(json!(row.get::<String,_>(column)) == audit["previous_binding"][column], "adopted workspace identity changed");
            }
            for column in ["contract_revision", "policy_revision"] {
                ensure!(json!(row.get::<i64,_>(column)) == audit["previous_binding"][column], "adopted workspace authority changed");
            }
            ensure!(record["result"] == json!({"workspace_id":adoption.workspace_id,"path":adoption.candidate_path,"state":"ready","execution_started":false,"task_success_asserted":false,"service_activated":false}), "workspace adoption result mismatch");
        }
        if let Some(proof) = receipt.get("run_archive") {
            let rows = sqlx::query("SELECT body FROM events WHERE kind='recovery_runs_archived' LIMIT 2").fetch_all(&mut db).await?;
            ensure!(rows.len() == 1 && serde_json::from_str::<Value>(&rows[0].get::<String,_>("body"))? == *proof, "run archive differs from audit ledger");
            let archived = namespace_inventory(&receipt["files"], "state/recovery-history/original-runs/")?;
            ensure!(proof["manifest_sha256"] == manifest && proof["source"] == "runs" && proof["destination"] == "recovery-history/original-runs" && proof["process_actions"] == false && proof["execution_enabled"] == false
                && proof["file_count"] == archived.len() && proof["tree_sha256"] == format!("{:x}", Sha256::digest(serde_json::to_vec(&archived)?)), "run archive inventory mismatch");
            ensure!(std::fs::symlink_metadata(root.join("state/runs")).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound), "historical live-run namespace still present");
            ensure!(std::fs::symlink_metadata(root.join("state/recovery-history/original-runs"))?.is_dir(), "historical run archive missing");
        }
        Ok(())
    }.await;
    db.close().await?;
    result?;
    if let Some(expected) = receipt.get("activation") {
        let store = telos_store::Store::open_recovery_review(&root.join("state")).await?;
        let verified = telos_runtime::recovery_preparation::verify_activation_publication(
            &store,
            &root.join("state"),
            false,
        )
        .await;
        store.close().await;
        ensure!(
            verified? == *expected,
            "activation receipt differs from published authorization"
        );
    }
    Ok(
        json!({"prepared":true,"directory":root,"execution_enabled":false,"activation_supported":true,"report":receipt["report"]}),
    )
}
/// Preserve the original verified image; publish only a fully audited, separately hashed copy.
pub async fn prepare_restore(
    recovery: &Path,
    expected_manifest: &str,
    expected_database: &str,
) -> Result<Value> {
    let recovery = recovery.canonicalize()?;
    let _operation = crate::service::operation_lock(&recovery)?;
    let _owner = telos_runtime::daemon::maintenance_lock(&recovery)?;
    let review = inspect_restore(&recovery).await?;
    ensure!(
        review["manifest_sha256"] == expected_manifest
            && review["database_sha256"] == expected_database,
        "reviewed recovery fingerprints changed"
    );
    ensure!(
        review["evidence_integrity_complete"] == true,
        "recovery evidence bindings require reconciliation before preparation"
    );
    let completed = recovery.join("prepared-state");
    if completed.try_exists()? {
        return prepared_receipt(&completed, expected_manifest, expected_database).await;
    }
    let candidate = recovery.join(format!(
        ".prepare-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
    ));
    std::fs::create_dir(&candidate)?;
    std::fs::set_permissions(&candidate, std::fs::Permissions::from_mode(0o700))?;
    let payload = candidate.join("state");
    std::fs::create_dir(&payload)?;
    std::fs::set_permissions(&payload, std::fs::Permissions::from_mode(0o700))?;
    copy_tree(&recovery.join("state"), &payload, 0, false)?;
    let (_, manifest) =
        read_json_bounded(&recovery.join("backup-manifest.json"), 32 * 1024 * 1024)?;
    let mut files = BTreeMap::new();
    inventory(&payload, &payload, &mut files, 0)?;
    for name in ["restore-required.json", "daemon.lock", "service.lock"] {
        files.remove(name);
    }
    ensure!(
        serde_json::to_value(files)? == manifest["files"],
        "candidate differs from reviewed image"
    );
    let (marker, _) = read_json_bounded(&payload.join("restore-required.json"), 65536)?;
    let mut marker_file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(candidate.join("restore-required.json"))?;
    marker_file.write_all(&marker)?;
    marker_file.sync_all()?;
    let original = Path::new(
        review["original_state"]
            .as_str()
            .context("original state missing")?,
    );
    let report = telos_runtime::recovery_preparation::prepare(
        &payload,
        expected_manifest,
        expected_database,
        original,
    )
    .await?;
    let receipt = json!({"format":1,"manifest_sha256":expected_manifest,"database_sha256":expected_database,"report":report,"files":preparation_inventory(&candidate)?});
    let bytes = serde_json::to_vec_pretty(&receipt)?;
    ensure!(
        bytes.len() <= 32 * 1024 * 1024,
        "preparation receipt too large"
    );
    let mut f = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(candidate.join("receipt.json"))?;
    f.write_all(&bytes)?;
    f.sync_all()?;
    File::open(payload.join("telos.db"))?.sync_all()?;
    File::open(&payload)?.sync_all()?;
    File::open(&candidate)?.sync_all()?;
    // Verify the complete result before publishing. Failed candidates remain gated and inspectable.
    prepared_receipt(&candidate, expected_manifest, expected_database).await?;
    publish_exclusive(&candidate, &completed)?;
    File::open(&recovery)?.sync_all()?;
    prepared_receipt(&completed, expected_manifest, expected_database).await
}

/// Each decision produces a new immutable-by-convention image. Inputs are explicitly pinned.
pub async fn review_project(
    image: &Path,
    expected_receipt: &str,
    decision: &Path,
    materials: &Path,
    destination: &Path,
) -> Result<Value> {
    operator_action(
        image,
        expected_receipt,
        Some(decision),
        Some(materials),
        destination,
        OperatorKind::Review,
    )
    .await
}

pub async fn reconcile_environment(
    image: &Path,
    expected_receipt: &str,
    decision: &Path,
    materials: &Path,
    destination: &Path,
) -> Result<Value> {
    operator_action(
        image,
        expected_receipt,
        Some(decision),
        Some(materials),
        destination,
        OperatorKind::ReconcileEnvironment,
    )
    .await
}

pub async fn discard_executions(
    image: &Path,
    expected_receipt: &str,
    decision: &Path,
    materials: &Path,
    destination: &Path,
) -> Result<Value> {
    operator_action(
        image,
        expected_receipt,
        Some(decision),
        Some(materials),
        destination,
        OperatorKind::DiscardExecution,
    )
    .await
}

pub async fn discard_verifications(
    image: &Path,
    expected_receipt: &str,
    decision: &Path,
    materials: &Path,
    destination: &Path,
) -> Result<Value> {
    operator_action(
        image,
        expected_receipt,
        Some(decision),
        Some(materials),
        destination,
        OperatorKind::DiscardVerification,
    )
    .await
}

pub async fn adopt_workspace(
    image: &Path,
    expected_receipt: &str,
    decision: &Path,
    materials: &Path,
    destination: &Path,
) -> Result<Value> {
    operator_action(
        image,
        expected_receipt,
        Some(decision),
        Some(materials),
        destination,
        OperatorKind::AdoptWorkspace,
    )
    .await
}

/// Report concrete activation blockers; never removes gates, starts work or cleans processes.
pub async fn inspect_activation(
    image: &Path,
    expected_receipt: &str,
    check_workspaces: bool,
) -> Result<Value> {
    let image = image.canonicalize()?;
    let _operation = crate::service::operation_lock(&image)?;
    let _owner = telos_runtime::daemon::maintenance_lock(&image.join("state"))?;
    let (bytes, receipt) = read_json_bounded(&image.join("receipt.json"), 32 * 1024 * 1024)?;
    ensure!(
        format!("{:x}", Sha256::digest(bytes)) == expected_receipt,
        "source receipt changed"
    );
    prepared_receipt(
        &image,
        receipt["manifest_sha256"]
            .as_str()
            .context("manifest missing")?,
        receipt["database_sha256"]
            .as_str()
            .context("database missing")?,
    )
    .await?;
    let store = telos_store::Store::open_recovery_review(&image.join("state")).await?;
    let result =
        activation_checks(&store, &image, &receipt, expected_receipt, check_workspaces).await;
    store.close().await;
    result
}

async fn activation_checks(
    store: &telos_store::Store,
    image: &Path,
    receipt: &Value,
    expected_receipt: &str,
    check_workspaces: bool,
) -> Result<Value> {
    let inventory = store.recovery_activation_inventory().await?;
    let mut blockers = Vec::new();
    for key in [
        "held_projects",
        "unfinished_steps",
        "unfinished_verifications",
        "external_environment_sessions",
        "unpaused_goals",
        "armed_schedules",
        "pending_notifications",
    ] {
        if inventory["counts"][key].as_i64().unwrap_or(0) > 0 {
            blockers.push(json!({"kind":key,"count":inventory["counts"][key]}));
        }
    }
    if inventory["workspace_records_truncated"] == true {
        blockers.push(json!({"kind":"workspace_review_limit_exceeded","limit":100}));
    }
    // Historical process journals must never be reinterpreted as current cleanup authority.
    let legacy_runs = image.join("state/runs");
    match std::fs::symlink_metadata(&legacy_runs) {
            Ok(_) => blockers.push(json!({"kind":"legacy_run_namespace_requires_archival","path":"state/runs","note":"Activation must preserve historical run records outside the live cleanup namespace; do not delete evidence."})),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
    let mut checks = Vec::new();
    for record in inventory["workspace_records"]
        .as_array()
        .context("workspace inventory missing")?
    {
        let workspace: telos_core::DeliveryWorkspace =
            serde_json::from_value(record["workspace"].clone())?;
        let audit = &record["adoption_audit"];
        if workspace.state != "ready" || audit.is_null() {
            blockers
                .push(json!({"kind":"workspace_requires_adoption","workspace_id":workspace.id}));
            continue;
        }
        let adoption: telos_core::RecoveryWorkspaceAdoption =
            serde_json::from_value(audit["adoption"].clone())?;
        adoption.validate()?;
        ensure!(
            adoption.review.project_id == record["project_id"]
                && adoption.review.manifest_sha256 == receipt["manifest_sha256"]
                && adoption.candidate_path == workspace.path,
            "workspace adoption provenance mismatch"
        );
        if !check_workspaces {
            blockers
                .push(json!({"kind":"external_workspace_not_checked","workspace_id":workspace.id}));
            continue;
        }
        match telos_runtime::recovery_preparation::inspect_workspace(store, &workspace.id, Path::new(&workspace.path)).await {
                Ok(observation) => {
                    let matches = observation["snapshot_sha256"] == adoption.expected_snapshot_sha256;
                    if !matches { blockers.push(json!({"kind":"workspace_changed_since_adoption","workspace_id":workspace.id})); }
                    checks.push(json!({"workspace_id":workspace.id,"matches_adoption":matches,"observed_snapshot_sha256":observation["snapshot_sha256"]}));
                }
                Err(error) => blockers.push(json!({"kind":"workspace_inspection_failed","workspace_id":workspace.id,"error":error.to_string()})),
            }
    }
    Ok(
        json!({"source_receipt_sha256":expected_receipt,"inventory":inventory,"external_workspace_checks":checks,"checks_passed":blockers.is_empty(),"blockers":blockers,"activation_supported":true,"execution_enabled":false,"scope":"Point-in-time preflight only. Does not prove exclusive access or stop another service; activation and namespace migration are still required."}),
    )
}

/// Publish one authorized new state directory. This does not start a service or resume goals.
pub async fn activate_restore(
    image: &Path,
    expected_receipt: &str,
    decision_file: &Path,
    destination: &Path,
) -> Result<Value> {
    use std::os::fd::AsRawFd;
    let image = image.canonicalize()?;
    let _operation = crate::service::operation_lock(&image)?;
    let _owner = telos_runtime::daemon::maintenance_lock(&image.join("state"))?;
    let (receipt_bytes, mut receipt) =
        read_json_bounded(&image.join("receipt.json"), 32 * 1024 * 1024)?;
    ensure!(
        format!("{:x}", Sha256::digest(receipt_bytes)) == expected_receipt,
        "source receipt changed"
    );
    let manifest = receipt["manifest_sha256"]
        .as_str()
        .context("manifest missing")?
        .to_owned();
    let database = receipt["database_sha256"]
        .as_str()
        .context("database missing")?
        .to_owned();
    prepared_receipt(&image, &manifest, &database).await?;
    ensure!(
        receipt.get("activation").is_none(),
        "image is already activated"
    );
    let (_, value) = read_json_bounded(decision_file, 65536)?;
    let decision: telos_core::RecoveryActivation = serde_json::from_value(value)?;
    decision.validate()?;
    ensure!(
        decision.manifest_sha256 == manifest && decision.source_receipt_sha256 == expected_receipt,
        "activation decision source mismatch"
    );
    ensure!(
        destination.is_absolute(),
        "activation destination must be absolute"
    );
    let parent = destination
        .parent()
        .context("destination parent missing")?
        .canonicalize()?;
    let destination = parent.join(
        destination
            .file_name()
            .context("destination filename missing")?,
    );
    ensure!(
        !destination.starts_with(&image)
            && decision.target_state
                == destination
                    .join("state")
                    .to_str()
                    .context("non-UTF8 destination")?,
        "activation target mismatch or source overlap"
    );
    ensure!(
        std::fs::symlink_metadata(&destination)
            .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
        "activation destination exists or is inaccessible"
    );
    let (marker, gate) = read_json_bounded(&image.join("restore-required.json"), 65536)?;
    let original = Path::new(
        gate["original_state"]
            .as_str()
            .context("original state missing")?,
    );
    // Read existing ownership locks only. An absent/moved source still needs the operator's independent retirement confirmation.
    let mut source_guards = Vec::new();
    if original.try_exists()? {
        ensure!(
            original.canonicalize()? == original,
            "original source path changed"
        );
        for name in ["service.lock", "daemon.lock"] {
            let path = original.join(name);
            match std::fs::symlink_metadata(&path) {
                Ok(_) => {
                    let file = regular(&path)?;
                    ensure!(
                        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }
                            == 0,
                        "source instance or lifecycle operation is still active"
                    );
                    source_guards.push(file);
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                Err(e) => return Err(e.into()),
            }
        }
    }
    let candidate = parent.join(format!(
        ".activate-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
    ));
    std::fs::create_dir(&candidate)?;
    std::fs::set_permissions(&candidate, std::fs::Permissions::from_mode(0o700))?;
    let payload = candidate.join("state");
    std::fs::create_dir(&payload)?;
    std::fs::set_permissions(&payload, std::fs::Permissions::from_mode(0o700))?;
    copy_tree(&image.join("state"), &payload, 0, false)?;
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(candidate.join("restore-required.json"))?;
    f.write_all(&marker)?;
    f.sync_all()?;
    ensure!(
        serde_json::to_value(preparation_inventory(&candidate)?)? == receipt["files"],
        "activation candidate differs from reviewed image"
    );
    let store = telos_store::Store::open_recovery_review(&payload).await?;
    let checked = activation_checks(&store, &candidate, &receipt, expected_receipt, true).await;
    store.close().await;
    let checked = checked?;
    ensure!(
        checked["checks_passed"] == true,
        "activation preflight has unresolved blockers: {}",
        checked["blockers"]
    );
    let current_hash = receipt["files"]["state/telos.db"]["sha256"]
        .as_str()
        .context("database hash missing")?;
    let record = telos_runtime::recovery_preparation::authorize_activation(
        &payload,
        current_hash,
        original,
        &decision,
        &checked,
    )
    .await?;
    receipt["activation"] = record;
    receipt["source_receipt_sha256"] = json!(expected_receipt);
    receipt["files"] = serde_json::to_value(preparation_inventory(&candidate)?)?;
    let bytes = serde_json::to_vec_pretty(&receipt)?;
    ensure!(
        bytes.len() <= 32 * 1024 * 1024,
        "activation receipt too large"
    );
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(candidate.join("receipt.json"))?;
    f.write_all(&bytes)?;
    f.sync_all()?;
    File::open(payload.join("telos.db"))?.sync_all()?;
    File::open(&payload)?.sync_all()?;
    File::open(&candidate)?.sync_all()?;
    prepared_receipt(&candidate, &manifest, &database).await?;
    publish_exclusive(&candidate, &destination)?;
    File::open(&parent)?.sync_all()?;
    let store = telos_store::Store::open_recovery_review(&destination.join("state")).await?;
    let verified = telos_runtime::recovery_preparation::verify_activation_publication(
        &store,
        &destination.join("state"),
        true,
    )
    .await;
    store.close().await;
    verified?;
    drop(source_guards);
    Ok(
        json!({"directory":destination,"state_directory":decision.target_state,"activated":true,"service_started":false,"goals_resumed":false,"receipt_sha256":format!("{:x}",Sha256::digest(bytes))}),
    )
}

pub async fn inspect_workspace(
    image: &Path,
    expected_receipt: &str,
    workspace_id: &str,
    candidate: &Path,
) -> Result<Value> {
    let image = image.canonicalize()?;
    let _operation = crate::service::operation_lock(&image)?;
    let _owner = telos_runtime::daemon::maintenance_lock(&image.join("state"))?;
    let (bytes, receipt) = read_json_bounded(&image.join("receipt.json"), 32 * 1024 * 1024)?;
    ensure!(
        format!("{:x}", Sha256::digest(bytes)) == expected_receipt,
        "source receipt changed"
    );
    prepared_receipt(
        &image,
        receipt["manifest_sha256"]
            .as_str()
            .context("manifest missing")?,
        receipt["database_sha256"]
            .as_str()
            .context("database missing")?,
    )
    .await?;
    let store = telos_store::Store::open_recovery_review(&image.join("state")).await?;
    let result =
        telos_runtime::recovery_preparation::inspect_workspace(&store, workspace_id, candidate)
            .await;
    store.close().await;
    let mut result = result?;
    result["source_receipt_sha256"] = json!(expected_receipt);
    result["manifest_sha256"] = receipt["manifest_sha256"].clone();
    Ok(result)
}

pub async fn archive_runs(
    image: &Path,
    expected_receipt: &str,
    destination: &Path,
) -> Result<Value> {
    operator_action(
        image,
        expected_receipt,
        None,
        None,
        destination,
        OperatorKind::ArchiveRuns,
    )
    .await
}

fn namespace_inventory(files: &Value, prefix: &str) -> Result<BTreeMap<String, Value>> {
    Ok(files
        .as_object()
        .context("file inventory missing")?
        .iter()
        .filter_map(|(name, value)| {
            name.strip_prefix(prefix)
                .map(|suffix| (suffix.to_owned(), value.clone()))
        })
        .collect())
}

#[derive(Clone, Copy)]
enum OperatorKind {
    ReconcileEnvironment,
    Review,
    ArchiveRuns,
    AdoptWorkspace,
    DiscardExecution,
    DiscardVerification,
}

async fn operator_action(
    image: &Path,
    expected_receipt: &str,
    decision: Option<&Path>,
    materials: Option<&Path>,
    destination: &Path,
    kind: OperatorKind,
) -> Result<Value> {
    let image = image.canonicalize()?;
    let _operation = crate::service::operation_lock(&image)?;
    let _owner = telos_runtime::daemon::maintenance_lock(&image.join("state"))?;
    let (receipt_bytes, mut receipt) =
        read_json_bounded(&image.join("receipt.json"), 32 * 1024 * 1024)?;
    ensure!(
        format!("{:x}", Sha256::digest(&receipt_bytes)) == expected_receipt,
        "source receipt changed"
    );
    let manifest = receipt["manifest_sha256"]
        .as_str()
        .context("missing manifest")?
        .to_owned();
    let database = receipt["database_sha256"]
        .as_str()
        .context("missing database")?
        .to_owned();
    prepared_receipt(&image, &manifest, &database).await?;
    ensure!(
        receipt.get("activation").is_none(),
        "use the retained pre-activation image for recovery edits"
    );
    let (action, bytes) = if matches!(kind, OperatorKind::ArchiveRuns) {
        (None, Vec::new())
    } else {
        let (_, value) = read_json_bounded(decision.context("operator decision required")?, 65536)?;
        use telos_runtime::recovery_preparation::OperatorRecoveryAction;
        let action = match kind {
            OperatorKind::ArchiveRuns => unreachable!("archive has no operator material"),
            OperatorKind::ReconcileEnvironment => {
                OperatorRecoveryAction::ReconcileEnvironment(serde_json::from_value(value)?)
            }
            OperatorKind::Review => OperatorRecoveryAction::Review(serde_json::from_value(value)?),
            OperatorKind::AdoptWorkspace => {
                OperatorRecoveryAction::AdoptWorkspace(serde_json::from_value(value)?)
            }
            OperatorKind::DiscardExecution => {
                OperatorRecoveryAction::Discard(serde_json::from_value(value)?)
            }
            OperatorKind::DiscardVerification => {
                OperatorRecoveryAction::DiscardVerification(serde_json::from_value(value)?)
            }
        };
        action.validate()?;
        let review = action.review();
        ensure!(
            review.manifest_sha256 == manifest,
            "decision snapshot mismatch"
        );
        let materials = materials
            .context("operator materials required")?
            .canonicalize()?;
        let mut bytes = Vec::new();
        for reference in &review.evidence {
            // IDs are explicit file names, never traversals or links.
            ensure!(
                reference.id != "." && reference.id != "..",
                "invalid material filename"
            );
            let mut material = Vec::new();
            regular(&materials.join(&reference.id))?
                .take(8 * 1024 * 1024 + 1)
                .read_to_end(&mut material)?;
            ensure!(
                !material.is_empty() && material.len() <= 8 * 1024 * 1024,
                "material size invalid"
            );
            ensure!(
                format!("{:x}", Sha256::digest(&material)) == reference.sha256,
                "material hash mismatch"
            );
            bytes.push(material);
        }
        (Some(action), bytes)
    };
    ensure!(destination.is_absolute(), "destination must be absolute");
    let parent = destination
        .parent()
        .context("destination parent missing")?
        .canonicalize()?;
    let destination = parent.join(
        destination
            .file_name()
            .context("destination filename missing")?,
    );
    ensure!(
        !destination.starts_with(&image),
        "destination must be outside source image"
    );
    ensure!(
        std::fs::symlink_metadata(&destination)
            .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
        "destination already exists or is inaccessible"
    );
    let candidate = parent.join(format!(
        ".review-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
    ));
    std::fs::create_dir(&candidate)?;
    std::fs::set_permissions(&candidate, std::fs::Permissions::from_mode(0o700))?;
    let payload = candidate.join("state");
    std::fs::create_dir(&payload)?;
    std::fs::set_permissions(&payload, std::fs::Permissions::from_mode(0o700))?;
    copy_tree(&image.join("state"), &payload, 0, false)?;
    let (marker, gate) = read_json_bounded(&image.join("restore-required.json"), 65536)?;
    let mut f = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(candidate.join("restore-required.json"))?;
    f.write_all(&marker)?;
    f.sync_all()?;
    ensure!(
        serde_json::to_value(preparation_inventory(&candidate)?)? == receipt["files"],
        "review candidate differs from source"
    );
    let original = Path::new(
        gate["original_state"]
            .as_str()
            .context("original state missing")?,
    );
    let current_database = receipt["files"]["state/telos.db"]["sha256"]
        .as_str()
        .context("missing current database hash")?;
    let result = if matches!(kind, OperatorKind::ArchiveRuns) {
        let old_runs = namespace_inventory(&receipt["files"], "state/runs/")?;
        let hash = format!("{:x}", Sha256::digest(serde_json::to_vec(&old_runs)?));
        let result = telos_runtime::recovery_preparation::archive_runs(
            &payload,
            &manifest,
            current_database,
            original,
            &hash,
            old_runs.len(),
        )
        .await?;
        let actual = serde_json::to_value(preparation_inventory(&candidate)?)?;
        ensure!(
            namespace_inventory(&actual, "state/recovery-history/original-runs/")? == old_runs,
            "archived run bytes differ from source"
        );
        let mut expected_files: BTreeMap<String, Value> =
            serde_json::from_value(receipt["files"].clone())?;
        expected_files.retain(|key, _| !key.starts_with("state/runs/") && key != "state/telos.db");
        for (key, value) in old_runs {
            expected_files.insert(format!("state/recovery-history/original-runs/{key}"), value);
        }
        let mut actual_files: BTreeMap<String, Value> = serde_json::from_value(actual)?;
        actual_files.remove("state/telos.db");
        ensure!(
            expected_files == actual_files,
            "archive changed unrelated files"
        );
        result
    } else {
        telos_runtime::recovery_preparation::apply_operator_action(
            &payload,
            current_database,
            original,
            action.context("operator action missing")?,
            bytes,
        )
        .await?
    };
    receipt["source_receipt_sha256"] = json!(expected_receipt);
    match kind {
        OperatorKind::ReconcileEnvironment => {
            let records = receipt
                .as_object_mut()
                .context("receipt must be an object")?
                .entry("environment_dispositions")
                .or_insert_with(|| json!([]))
                .as_array_mut()
                .context("environment dispositions must be an array")?;
            records.push(result.clone());
        }
        OperatorKind::ArchiveRuns => receipt["run_archive"] = result.clone(),
        OperatorKind::AdoptWorkspace => receipt["workspace_adoption"] = result.clone(),
        OperatorKind::DiscardExecution => receipt["execution_disposition"] = result.clone(),
        OperatorKind::DiscardVerification => receipt["verification_disposition"] = result.clone(),
        OperatorKind::Review => {
            receipt["operator_review"] = result["review"].clone();
            receipt["review_result"] = result["result"].clone();
        }
    }
    receipt["files"] = serde_json::to_value(preparation_inventory(&candidate)?)?;
    let receipt_bytes = serde_json::to_vec_pretty(&receipt)?;
    ensure!(
        receipt_bytes.len() <= 32 * 1024 * 1024,
        "review receipt too large"
    );
    let mut f = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(candidate.join("receipt.json"))?;
    f.write_all(&receipt_bytes)?;
    f.sync_all()?;
    File::open(payload.join("telos.db"))?.sync_all()?;
    File::open(&payload)?.sync_all()?;
    File::open(&candidate)?.sync_all()?;
    prepared_receipt(&candidate, &manifest, &database).await?;
    publish_exclusive(&candidate, &destination)?;
    File::open(&parent)?.sync_all()?;
    Ok(
        json!({"directory":destination,"review":result,"receipt_sha256":format!("{:x}",Sha256::digest(&receipt_bytes)),"execution_enabled":false,"activation_supported":true}),
    )
}

pub(crate) fn publish_exclusive(source: &Path, destination: &Path) -> Result<()> {
    let source = std::ffi::CString::new(source.as_os_str().as_encoded_bytes())?;
    let destination = std::ffi::CString::new(destination.as_os_str().as_encoded_bytes())?;
    #[cfg(target_os = "macos")]
    // SAFETY: both paths are valid NUL-terminated strings; EXCL refuses replacement.
    let status = unsafe {
        libc::renameatx_np(
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            destination.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    #[cfg(target_os = "linux")]
    // SAFETY: both paths are valid NUL-terminated strings; NOREPLACE refuses replacement.
    let status = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    anyhow::bail!("exclusive restore publication unsupported on this platform");
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    ensure!(
        status == 0,
        "restore publication failed: {}",
        std::io::Error::last_os_error()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture(std::path::PathBuf);
    impl Fixture {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!(
                "telos-backup-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir(&p).unwrap();
            Self(p)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    #[tokio::test]
    async fn activation_preflight_keeps_gates_and_never_interprets_old_journals() -> Result<()> {
        let f = Fixture::new();
        let state = f.0.join("source");
        let store = telos_store::Store::open(&state).await?;
        store.close().await;
        for with_journal in [false, true] {
            if with_journal {
                std::fs::create_dir_all(state.join("runs/old"))?;
                std::fs::write(
                    state.join("runs/old/processes.json"),
                    b"not JSON; must never parse or execute",
                )?;
            }
            let archive = f.0.join(format!("archive-{with_journal}"));
            let recovery = f.0.join(format!("recovery-{with_journal}"));
            create(&state, &archive).await?;
            stage_restore(&archive, &recovery).await?;
            let inspection = inspect_restore(&recovery).await?;
            prepare_restore(
                &recovery,
                inspection["manifest_sha256"].as_str().unwrap(),
                inspection["database_sha256"].as_str().unwrap(),
            )
            .await?;
            let image = recovery.join("prepared-state");
            let before = preparation_inventory(&image)?;
            let digest = format!(
                "{:x}",
                Sha256::digest(std::fs::read(image.join("receipt.json"))?)
            );
            let result = inspect_activation(&image, &digest, false).await?;
            assert_eq!(result["checks_passed"], !with_journal);
            assert_eq!(result["activation_supported"], true);
            assert_eq!(result["execution_enabled"], false);
            if with_journal {
                assert_eq!(
                    result["blockers"][0]["kind"],
                    "legacy_run_namespace_requires_archival"
                );
            }
            if with_journal {
                let archived = f.0.join("archived-runs");
                let archive_result = archive_runs(&image, &digest, &archived).await?;
                let archive_digest = archive_result["receipt_sha256"].as_str().unwrap();
                let activated = f.0.canonicalize()?.join("activated");
                let decision = telos_core::RecoveryActivation {
                    manifest_sha256: inspection["manifest_sha256"].as_str().unwrap().into(),
                    source_receipt_sha256: archive_digest.into(),
                    target_state: activated.join("state").to_string_lossy().into(),
                    decision_id: "synthetic-activation".into(),
                    operator: "fixture".into(),
                    statement:
                        "Synthetic fixture only; no real retirement or exclusive-access assertion"
                            .into(),
                    source_instance_retired: true,
                    exclusive_access_confirmed: true,
                };
                let decision_file = f.0.join("activation.json");
                std::fs::write(&decision_file, serde_json::to_vec(&decision)?)?;
                let source_lock = telos_runtime::daemon::maintenance_lock(&state)?;
                assert!(
                    activate_restore(&archived, archive_digest, &decision_file, &activated)
                        .await
                        .unwrap_err()
                        .to_string()
                        .contains("still active")
                );
                drop(source_lock);
                let activation =
                    activate_restore(&archived, archive_digest, &decision_file, &activated).await?;
                assert_eq!(activation["activated"], true);
                assert_eq!(activation["service_started"], false);
                assert_eq!(activation["goals_resumed"], false);
                assert!(activated.join("state/restore-required.json").exists());
                assert!(
                    activate_restore(&archived, archive_digest, &decision_file, &activated)
                        .await
                        .is_err()
                );
                let active_state = activated.join("state");
                let (shutdown, rx) = tokio::sync::watch::channel(false);
                let service = tokio::spawn({
                    let root = active_state.clone();
                    let runner = telos_runtime::runner::CodexRunner {
                        program: f.0.join("must-not-run"),
                    };
                    async move { telos_runtime::daemon::serve(&root, runner, rx).await }
                });
                let mut responding = false;
                for _ in 0..100 {
                    if telos_runtime::daemon::call(
                        &active_state,
                        &telos_runtime::daemon::Request::Ping,
                    )
                    .await
                    .is_ok()
                    {
                        responding = true;
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
                shutdown.send(true)?;
                service.await??;
                assert!(responding, "activated daemon did not respond");
                assert_eq!(
                    std::fs::read(
                        active_state.join("recovery-history/original-runs/old/processes.json")
                    )?,
                    b"not JSON; must never parse or execute"
                );
                // Copying the published state to another location does not transfer authority.
                let copied = f.0.join("copied-active-state");
                std::fs::create_dir(&copied)?;
                // A stopped SQLite worker can still be removing WAL sidecars.
                // Use the supported locked snapshot API, not a raw database-tree
                // copy; the activation marker still binds the original target.
                let snapshot = f.0.join("active-state-snapshot");
                create(&active_state, &snapshot).await?;
                verify(&snapshot).await?;
                copy_tree(&snapshot.join("state"), &copied, 0, false)?;
                let (_tx, rx) = tokio::sync::watch::channel(false);
                let error = telos_runtime::daemon::serve(
                    &copied,
                    telos_runtime::runner::CodexRunner {
                        program: f.0.join("must-not-run"),
                    },
                    rx,
                )
                .await
                .unwrap_err();
                assert!(format!("{error:#}").contains("different state directory"));
                std::fs::write(active_state.join("recovery-activated.json"), b"{}")?;
                let (_tx, rx) = tokio::sync::watch::channel(false);
                assert!(telos_runtime::daemon::serve(
                    &active_state,
                    telos_runtime::runner::CodexRunner {
                        program: f.0.join("must-not-run")
                    },
                    rx
                )
                .await
                .is_err());

                assert_eq!(
                    inspect_activation(&archived, archive_digest, false).await?["checks_passed"],
                    true
                );
                assert!(!archived.join("state/runs").exists());
                assert_eq!(
                    std::fs::read(
                        archived.join("state/recovery-history/original-runs/old/processes.json")
                    )?,
                    b"not JSON; must never parse or execute"
                );
                assert!(archived.join("state/restore-required.json").exists());
                assert!(archive_runs(&image, &digest, &archived).await.is_err());
                let repeated = f.0.join("archive-again");
                assert!(archive_runs(&archived, archive_digest, &repeated)
                    .await
                    .is_err());
                assert!(!repeated.exists());
                // Receipt proof and archive bytes are independently checked.
                let receipt_path = archived.join("receipt.json");
                let (_, mut receipt) = read_json_bounded(&receipt_path, 32 * 1024 * 1024)?;
                receipt["run_archive"]["file_count"] = json!(999);
                std::fs::write(&receipt_path, serde_json::to_vec(&receipt)?)?;
                assert!(prepared_receipt(
                    &archived,
                    inspection["manifest_sha256"].as_str().unwrap(),
                    inspection["database_sha256"].as_str().unwrap()
                )
                .await
                .is_err());
            }
            assert_eq!(preparation_inventory(&image)?, before);
            assert!(image.join("state/restore-required.json").exists());
            assert!(inspect_activation(&image, &"0".repeat(64), true)
                .await
                .is_err());
        }
        Ok(())
    }

    #[tokio::test]
    async fn offline_environment_retirement_preserves_images_materials_and_review_chain(
    ) -> Result<()> {
        let f = Fixture::new();
        let state = f.0.join("source");
        let project = f.0.join("project");
        std::fs::create_dir(&project)?;
        let store = telos_store::Store::open(&state).await?;
        store.register_project("p", &project).await?;
        store
            .configure_project(
                "p",
                0,
                &telos_core::ProjectExecutionPolicy {
                    enabled: true,
                    sandbox: telos_core::Sandbox::WorkspaceWrite,
                    timeout_seconds: 30,
                    max_output_bytes: 65536,
                    max_steps_per_cycle: 1,
                },
            )
            .await?;
        let contract = serde_json::from_value(
            json!({"goal":"synthetic recovery","delivery_scope":"fixture only",
            "criteria":[{"id":"one","description":"fixture"}],"max_attempts":1,"interval_seconds":null}),
        )?;
        let goal = store
            .create_loop("p", "fixture", telos_core::LoopKind::Delivery, &contract)
            .await?;
        // Synthetic journal only: never invoke a provider, remote host or agent.
        let mut heads = Vec::new();
        for session in ["fixture-one", "fixture-two"] {
            store
                .configure_environment_adapter(
                    "p",
                    session,
                    0,
                    &telos_core::EnvironmentAdapterGrant {
                        enabled: true,
                        environment_sha256: format!("{:x}", Sha256::digest(session.as_bytes())),
                        adapter_sha256: "d".repeat(64),
                    },
                )
                .await?;
        }
        for session in ["fixture-one", "fixture-two"] {
            let grant = store.environment_adapter("p", session).await?.unwrap();
            for (version, action, phase) in
                [(0, "register", "registered"), (1, "acquire", "acquired")]
            {
                let intent = store
                    .begin_environment_operation(&telos_store::EnvironmentIntent {
                        loop_id: goal.id.clone(),
                        revision: 1,
                        adapter_key: session.into(),
                        adapter_version: 1,
                        verification_lease_id: None,
                        environment_sha256: grant["environment_sha256"].as_str().unwrap().into(),
                        adapter_sha256: grant["adapter_sha256"].as_str().unwrap().into(),
                        session_id: session.into(),
                        request_key: action.into(),
                        action: action.into(),
                        expected_version: version,
                    })
                    .await?;
                store
                    .finish_environment_operation(
                        intent["intent_sequence"].as_i64().unwrap(),
                        phase,
                    )
                    .await?;
            }
            heads.push(
                store
                    .events(&goal.id)
                    .await?
                    .into_iter()
                    .rev()
                    .find(|e| e["kind"] == "environment_operation_outcome")
                    .unwrap(),
            );
        }
        store.close().await;
        let archive = f.0.join("archive");
        let recovery = f.0.join("recovery");
        create(&state, &archive).await?;
        stage_restore(&archive, &recovery).await?;
        let inspection = inspect_restore(&recovery).await?;
        let manifest = inspection["manifest_sha256"].as_str().unwrap();
        let database = inspection["database_sha256"].as_str().unwrap();
        prepare_restore(&recovery, manifest, database).await?;
        let image = recovery.join("prepared-state");
        let original = preparation_inventory(&image)?;
        let hash = format!(
            "{:x}",
            Sha256::digest(std::fs::read(image.join("receipt.json"))?)
        );
        let materials = f.0.join("materials");
        std::fs::create_dir(&materials)?;
        let material = b"Synthetic provider and process attestation; no external action took place";
        std::fs::write(materials.join("report.txt"), material)?;
        let review = json!({"project_id":"p","manifest_sha256":manifest,"decision_id":"environment-one",
            "operator":"synthetic-test","statement":"Fixture testimony only; not a live recovery",
            "reviewed_external_state":true,"evidence":[{"id":"report.txt","sha256":format!("{:x}",Sha256::digest(material))}]});
        let make_decision = |index: usize, decision: &str| {
            let body = &heads[index]["body"];
            let mut review = review.clone();
            review["decision_id"] = json!(decision);
            json!({"review":review,"session":{
                "loop_id":goal.id,"revision":1,"session_id":body["session_id"],
                "environment_sha256":body["environment_sha256"],"adapter_sha256":body["adapter_sha256"],
                "expected_sequence":heads[index]["sequence"],"expected_version":2,"decision_id":decision,
                "provider_observed_at":telos_store::now(),"provider_lease_owner":null,"provider_operation_active":false,
                "remote_operations_stopped":true,"local_owners_retired":true,"provider_protocol_reconciled":true,
                "operator_note":"Fixture attestation; no real provider was used"}})
        };
        let file = f.0.join("decision.json");
        let output = f.0.join("first-retired");
        let decision = make_decision(0, "environment-one");
        // Bound receipt, materials, project, snapshot, decision identity and session version.
        for failure in 0..7 {
            let mut invalid = decision.clone();
            match failure {
                0 => invalid["review"]["manifest_sha256"] = json!("d".repeat(64)),
                1 => invalid["review"]["project_id"] = json!("other"),
                2 => invalid["session"]["expected_version"] = json!(3),
                3 => invalid["session"]["decision_id"] = json!("mismatch"),
                4 => invalid["session"]["provider_observed_at"] = json!(telos_store::now() - 301),
                5 => invalid["review"]["evidence"][0]["sha256"] = json!("e".repeat(64)),
                _ => (),
            }
            std::fs::write(&file, serde_json::to_vec(&invalid)?)?;
            let expected = if failure == 6 { "wrong-receipt" } else { &hash };
            assert!(
                reconcile_environment(&image, expected, &file, &materials, &output)
                    .await
                    .is_err()
            );
            assert!(!output.exists());
            assert_eq!(preparation_inventory(&image)?, original);
        }
        std::fs::write(&file, serde_json::to_vec(&decision)?)?;
        let first = reconcile_environment(&image, &hash, &file, &materials, &output).await?;
        assert_eq!(preparation_inventory(&image)?, original);
        assert_eq!(first["execution_enabled"], false);
        let readonly = telos_store::Store::open_recovery_review(&output.join("state")).await?;
        assert_eq!(
            readonly.recovery_activation_inventory().await?["counts"]
                ["external_environment_sessions"],
            1
        );
        assert_eq!(
            readonly.recovery_project_status("p").await?.unwrap()["state"],
            "held"
        );
        assert_eq!(
            readonly.get_loop(&goal.id).await?.state,
            telos_core::LoopState::Paused
        );
        let audit = readonly.events(&goal.id).await?;
        let record = audit
            .iter()
            .find(|e| e["kind"] == "environment_operation_reconciled")
            .unwrap();
        let reference: telos_core::RecoveryReviewEvidence =
            serde_json::from_value(record["body"]["recovery_review"]["evidence"][0].clone())?;
        assert_eq!(
            readonly.get_evidence(&reference.id, 0, 1024).await?["text"],
            std::str::from_utf8(material)?
        );
        readonly.close().await;
        let first_inventory = preparation_inventory(&output)?;
        let second_output = f.0.join("second-retired");
        std::fs::write(
            &file,
            serde_json::to_vec(&make_decision(1, "environment-two"))?,
        )?;
        let second = reconcile_environment(
            &output,
            first["receipt_sha256"].as_str().unwrap(),
            &file,
            &materials,
            &second_output,
        )
        .await?;
        assert_eq!(preparation_inventory(&output)?, first_inventory);
        let (_, receipt) =
            read_json_bounded(&second_output.join("receipt.json"), 32 * 1024 * 1024)?;
        assert_eq!(
            receipt["environment_dispositions"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        // Project review consumes both dispositions while leaving the goal paused.
        let mut project_review = review;
        project_review["decision_id"] = json!("project-review");
        std::fs::write(&file, serde_json::to_vec(&project_review)?)?;
        let released = f.0.join("released");
        review_project(
            &second_output,
            second["receipt_sha256"].as_str().unwrap(),
            &file,
            &materials,
            &released,
        )
        .await?;
        prepared_receipt(&released, manifest, database).await?;
        let readonly = telos_store::Store::open_recovery_review(&released.join("state")).await?;
        let inventory = readonly.recovery_activation_inventory().await?;
        assert_eq!(inventory["counts"]["external_environment_sessions"], 0);
        assert_eq!(inventory["counts"]["held_projects"], 0);
        assert_eq!(
            readonly.get_loop(&goal.id).await?.state,
            telos_core::LoopState::Paused
        );
        readonly.close().await;
        // Even a self-consistent file inventory cannot justify a fabricated receipt claim.
        let receipt_path = released.join("receipt.json");
        let (_, valid_receipt) = read_json_bounded(&receipt_path, 32 * 1024 * 1024)?;
        for tamper in 0..5 {
            let mut receipt = valid_receipt.clone();
            match tamper {
                0 => {
                    receipt["environment_dispositions"][0]["disposition"]["review"]["statement"] =
                        json!("forged")
                }
                1 => receipt["environment_dispositions"][0]["result"]["scope"] = json!("forged"),
                2 => {
                    receipt["environment_dispositions"]
                        .as_array_mut()
                        .unwrap()
                        .pop();
                }
                3 => {
                    receipt["environment_dispositions"][1] =
                        receipt["environment_dispositions"][0].clone();
                }
                _ => {
                    receipt
                        .as_object_mut()
                        .unwrap()
                        .remove("environment_dispositions");
                }
            }
            std::fs::write(&receipt_path, serde_json::to_vec(&receipt)?)?;
            assert!(prepared_receipt(&released, manifest, database)
                .await
                .is_err());
        }
        Ok(())
    }

    #[tokio::test]
    async fn offline_disposition_preserves_input_and_chains_into_project_review() -> Result<()> {
        let fixture = Fixture::new();
        let state = fixture.0.join("source");
        let store = telos_store::Store::open(&state).await?;
        let project = fixture.0.join("project");
        std::fs::create_dir(&project)?;
        store.register_project("p", &project).await?;
        let contract: telos_core::Contract = serde_json::from_value(
            json!({"goal":"synthetic recovery","delivery_scope":"fixture","criteria":[{"id":"one","description":"fixture"}],"max_attempts":1,"interval_seconds":null}),
        )?;
        let goal = store
            .create_loop("p", "fixture", telos_core::LoopKind::Delivery, &contract)
            .await?;
        let cycle = store.start_cycle(&goal.id, 1, "fixture").await?;
        let step = store
            .enqueue_step(
                &cycle,
                1,
                "fixture",
                &telos_core::ExecutionSpec {
                    task: "Never run this synthetic fixture".into(),
                    cwd: project.to_string_lossy().into(),
                    sandbox: telos_core::Sandbox::ReadOnly,
                    timeout_seconds: 10,
                    max_output_bytes: 4096,
                },
                telos_store::now(),
            )
            .await?;
        store.close().await;
        let archive = fixture.0.join("archive");
        create(&state, &archive).await?;
        let recovery = fixture.0.join("recovery");
        stage_restore(&archive, &recovery).await?;
        let inspection = inspect_restore(&recovery).await?;
        let manifest = inspection["manifest_sha256"].as_str().unwrap();
        let database = inspection["database_sha256"].as_str().unwrap();
        prepare_restore(&recovery, manifest, database).await?;
        let image = recovery.join("prepared-state");
        let original = preparation_inventory(&image)?;
        let receipt_hash = format!(
            "{:x}",
            Sha256::digest(std::fs::read(image.join("receipt.json"))?)
        );
        let materials = fixture.0.join("materials");
        std::fs::create_dir(&materials)?;
        let material = b"Synthetic fixture only: no process launched or external effects";
        std::fs::write(materials.join("report.txt"), material)?;
        let review = json!({"project_id":"p","manifest_sha256":manifest,"decision_id":"synthetic-discard","operator":"synthetic-test","statement":"Synthetic fixture only; no real operator review","reviewed_external_state":true,"evidence":[{"id":"report.txt","sha256":format!("{:x}",Sha256::digest(material))}]});
        let mut decision = json!({"review":review,"step_ids":[step],"processes_confirmed_stopped":true,"external_effects_reconciled":false});
        let file = fixture.0.join("decision.json");
        let output = fixture.0.join("disposed");
        std::fs::write(&file, serde_json::to_vec(&decision)?)?;
        assert!(
            discard_executions(&image, &receipt_hash, &file, &materials, &output)
                .await
                .is_err()
        );
        assert!(!output.exists());
        decision["external_effects_reconciled"] = json!(true);
        decision["step_ids"] = json!([step, "missing"]);
        std::fs::write(&file, serde_json::to_vec(&decision)?)?;
        assert!(
            discard_executions(&image, &receipt_hash, &file, &materials, &output)
                .await
                .is_err()
        );
        assert!(!output.exists());
        assert_eq!(preparation_inventory(&image)?, original);
        decision["step_ids"] = json!([step]);
        std::fs::write(&file, serde_json::to_vec(&decision)?)?;
        let result = discard_executions(&image, &receipt_hash, &file, &materials, &output).await?;
        assert_eq!(result["review"]["result"]["automatic_retry"], false);
        assert_eq!(preparation_inventory(&image)?, original);
        prepared_receipt(&output, manifest, database).await?;
        let readonly = telos_store::Store::open_recovery_review(&output.join("state")).await?;
        assert_eq!(readonly.step_status(&step).await?["state"], "cancelled");
        assert_eq!(
            readonly.recovery_project_status("p").await?.unwrap()["state"],
            "held"
        );
        assert_eq!(
            readonly.get_loop(&goal.id).await?.state,
            telos_core::LoopState::Paused
        );
        readonly.close().await;
        // A distinct project review can consume the output without dropping disposition proof.
        let mut review = review;
        review["decision_id"] = json!("synthetic-release");
        std::fs::write(&file, serde_json::to_vec(&review)?)?;
        let released = fixture.0.join("released");
        review_project(
            &output,
            result["receipt_sha256"].as_str().unwrap(),
            &file,
            &materials,
            &released,
        )
        .await?;
        prepared_receipt(&released, manifest, database).await?;
        // Receipt claims must still match the committed audit, even if file hashes agree.
        let receipt_path = released.join("receipt.json");
        let (_, mut receipt) = read_json_bounded(&receipt_path, 32 * 1024 * 1024)?;
        receipt["execution_disposition"]["disposition"]["review"]["statement"] = json!("altered");
        std::fs::write(&receipt_path, serde_json::to_vec(&receipt)?)?;
        assert!(prepared_receipt(&released, manifest, database)
            .await
            .is_err());
        Ok(())
    }

    #[tokio::test]
    async fn offline_verification_disposition_preserves_input_and_review_chain() -> Result<()> {
        let fixture = Fixture::new();
        let state = fixture.0.join("source");
        let store = telos_store::Store::open(&state).await?;
        let project = fixture.0.join("project");
        std::fs::create_dir(&project)?;
        store.register_project("p", &project).await?;
        let contract: telos_core::Contract = serde_json::from_value(
            json!({"goal":"synthetic recovery","delivery_scope":"fixture","criteria":[{"id":"one","description":"fixture"}],"max_attempts":1,"interval_seconds":null}),
        )?;
        let goal = store
            .create_loop("p", "fixture", telos_core::LoopKind::Delivery, &contract)
            .await?;
        store.start_cycle(&goal.id, 1, "fixture").await?;
        let plan = telos_core::VerificationPlan {
            checks: vec![telos_core::FileHashCheck {
                criterion_id: "one".into(),
                relative_path: "result.txt".into(),
                expected_sha256: "a".repeat(64),
            }],
            test_checks: vec![],
            log_checks: vec![],
        };
        store.configure_verification(&goal.id, 1, 0, &plan).await?;
        let step = store.acquire_verification(&goal.id, 1, 1).await?;
        store.close().await;
        let archive = fixture.0.join("archive");
        create(&state, &archive).await?;
        let recovery = fixture.0.join("recovery");
        stage_restore(&archive, &recovery).await?;
        let inspection = inspect_restore(&recovery).await?;
        let manifest = inspection["manifest_sha256"].as_str().unwrap();
        let database = inspection["database_sha256"].as_str().unwrap();
        prepare_restore(&recovery, manifest, database).await?;
        let image = recovery.join("prepared-state");
        let original = preparation_inventory(&image)?;
        let receipt_hash = format!(
            "{:x}",
            Sha256::digest(std::fs::read(image.join("receipt.json"))?)
        );
        let materials = fixture.0.join("materials");
        std::fs::create_dir(&materials)?;
        let material = b"Synthetic fixture only: no process launched or external effects";
        std::fs::write(materials.join("report.txt"), material)?;
        let review = json!({"project_id":"p","manifest_sha256":manifest,"decision_id":"synthetic-discard","operator":"synthetic-test","statement":"Synthetic fixture only; no real operator review","reviewed_external_state":true,"evidence":[{"id":"report.txt","sha256":format!("{:x}",Sha256::digest(material))}]});
        let mut decision = json!({"review":review,"lease_ids":[step],"processes_confirmed_stopped":true,"external_effects_reconciled":false});
        let file = fixture.0.join("decision.json");
        let output = fixture.0.join("disposed");
        std::fs::write(&file, serde_json::to_vec(&decision)?)?;
        assert!(
            discard_verifications(&image, &receipt_hash, &file, &materials, &output)
                .await
                .is_err()
        );
        assert!(!output.exists());
        decision["external_effects_reconciled"] = json!(true);
        decision["lease_ids"] = json!([step, "missing"]);
        std::fs::write(&file, serde_json::to_vec(&decision)?)?;
        assert!(
            discard_verifications(&image, &receipt_hash, &file, &materials, &output)
                .await
                .is_err()
        );
        assert!(!output.exists());
        assert_eq!(preparation_inventory(&image)?, original);
        decision["lease_ids"] = json!([step]);
        std::fs::write(&file, serde_json::to_vec(&decision)?)?;
        let result =
            discard_verifications(&image, &receipt_hash, &file, &materials, &output).await?;
        assert_eq!(result["review"]["result"]["automatic_retry"], false);
        assert_eq!(preparation_inventory(&image)?, original);
        prepared_receipt(&output, manifest, database).await?;
        let readonly = telos_store::Store::open_recovery_review(&output.join("state")).await?;
        assert!(readonly.verification_status(&goal.id).await?["active_verification"].is_null());
        assert_eq!(
            readonly.recovery_project_status("p").await?.unwrap()["state"],
            "held"
        );
        assert_eq!(
            readonly.get_loop(&goal.id).await?.state,
            telos_core::LoopState::Paused
        );
        readonly.close().await;
        // A distinct project review can consume the output without dropping disposition proof.
        let mut review = review;
        review["decision_id"] = json!("synthetic-release");
        std::fs::write(&file, serde_json::to_vec(&review)?)?;
        let released = fixture.0.join("released");
        review_project(
            &output,
            result["receipt_sha256"].as_str().unwrap(),
            &file,
            &materials,
            &released,
        )
        .await?;
        prepared_receipt(&released, manifest, database).await?;
        // Receipt claims must still match the committed audit, even if file hashes agree.
        let receipt_path = released.join("receipt.json");
        let (_, mut receipt) = read_json_bounded(&receipt_path, 32 * 1024 * 1024)?;
        receipt["verification_disposition"]["disposition"]["review"]["statement"] =
            json!("altered");
        std::fs::write(&receipt_path, serde_json::to_vec(&receipt)?)?;
        assert!(prepared_receipt(&released, manifest, database)
            .await
            .is_err());
        Ok(())
    }

    #[tokio::test]
    async fn operator_review_publishes_new_chain_and_preserves_source_on_failure() -> Result<()> {
        let fixture = Fixture::new();
        let state = fixture.0.join("source");
        let store = telos_store::Store::open(&state).await?;
        let project = fixture.0.join("project");
        std::fs::create_dir(&project)?;
        store.register_project("one", &project).await?;
        let project_two = fixture.0.join("project-two");
        std::fs::create_dir(&project_two)?;
        store.register_project("two", &project_two).await?;
        store.close().await;
        let archive = fixture.0.join("archive");
        create(&state, &archive).await?;
        let recovery = fixture.0.join("recovery");
        stage_restore(&archive, &recovery).await?;
        let inspection = inspect_restore(&recovery).await?;
        let manifest = inspection["manifest_sha256"].as_str().unwrap();
        let database = inspection["database_sha256"].as_str().unwrap();
        prepare_restore(&recovery, manifest, database).await?;
        let image = recovery.join("prepared-state");
        let original = preparation_inventory(&image)?;
        let receipt_hash = format!(
            "{:x}",
            Sha256::digest(std::fs::read(image.join("receipt.json"))?)
        );
        let material_dir = fixture.0.join("materials");
        std::fs::create_dir(&material_dir)?;
        let material = b"SYNTHETIC operator review: isolated empty fixture, no external execution";
        std::fs::write(material_dir.join("report.txt"), material)?;
        let decision = fixture.0.join("decision.json");
        let mut request = json!({"project_id":"one","manifest_sha256":manifest,"decision_id":"synthetic-1","operator":"synthetic-test","statement":"Synthetic external review fixture only","reviewed_external_state":true,"evidence":[{"id":"report.txt","sha256":format!("{:x}",Sha256::digest(material))}]});
        std::fs::write(&decision, serde_json::to_vec(&request)?)?;
        let output = fixture.0.join("reviewed");
        // An explicit material mismatch never changes or publishes an image.
        std::fs::write(material_dir.join("report.txt"), b"tampered")?;
        assert!(
            review_project(&image, &receipt_hash, &decision, &material_dir, &output)
                .await
                .is_err()
        );
        assert!(!output.exists());
        std::fs::write(material_dir.join("report.txt"), material)?;
        let result =
            review_project(&image, &receipt_hash, &decision, &material_dir, &output).await?;
        assert_eq!(result["execution_enabled"], false);
        assert_eq!(preparation_inventory(&image)?, original);
        prepared_receipt(&output, manifest, database).await?;
        assert!(
            review_project(&image, &receipt_hash, &decision, &material_dir, &output)
                .await
                .is_err()
        );
        // A second project can be reviewed from the first verified output.
        request["project_id"] = json!("two");
        request["decision_id"] = json!("synthetic-2");
        std::fs::write(&decision, serde_json::to_vec(&request)?)?;
        let second = fixture.0.join("reviewed-two");
        let result2 = review_project(
            &output,
            result["receipt_sha256"].as_str().unwrap(),
            &decision,
            &material_dir,
            &second,
        )
        .await?;
        assert_eq!(result2["review"]["result"]["state"], "cleared");
        let store = telos_store::Store::open_recovery_review(&second.join("state")).await?;
        for project in ["one", "two"] {
            assert_eq!(
                store.recovery_project_status(project).await?.unwrap()["state"],
                "cleared"
            );
        }
        assert_eq!(
            store.recovery_preparation().await?.unwrap()["state"],
            "prepared"
        );
        store.close().await;
        // A runtime rejection after candidate creation retains the gated candidate,
        // while the original input remains valid and untouched.
        request["project_id"] = json!("missing");
        std::fs::write(&decision, serde_json::to_vec(&request)?)?;
        let failed = fixture.0.join("failed");
        assert!(
            review_project(&image, &receipt_hash, &decision, &material_dir, &failed)
                .await
                .is_err()
        );
        assert!(!failed.exists());
        assert_eq!(preparation_inventory(&image)?, original);
        assert!(std::fs::read_dir(&fixture.0)?
            .filter_map(Result::ok)
            .any(|e| e.file_name().to_string_lossy().starts_with(".review-")
                && e.path().join("state/restore-required.json").exists()));
        Ok(())
    }

    #[tokio::test]
    async fn committed_wal_goals_and_evidence_survive_verified_snapshot() -> Result<()> {
        let fixture = Fixture::new();
        let state = fixture.0.join("state");
        let store = telos_store::Store::open(&state).await?;
        let project = fixture.0.join("project");
        std::fs::create_dir(&project)?;
        store.register_project("fixture", &project).await?;
        let contract: telos_core::Contract = serde_json::from_value(
            json!({"goal":"Preserve this goal","delivery_scope":"fixture","criteria":[{"id":"one","description":"verify"}],"max_attempts":1,"interval_seconds":null}),
        )?;
        let goal = store
            .create_loop(
                "fixture",
                "Backup fixture",
                telos_core::LoopKind::Delivery,
                &contract,
            )
            .await?;
        let cycle = store.start_cycle(&goal.id, 1, "backup-check").await?;
        store
            .enqueue_step(
                &cycle,
                1,
                "queued",
                &telos_core::ExecutionSpec {
                    task: "Never execute this snapshot fixture".into(),
                    cwd: project.to_string_lossy().into(),
                    sandbox: telos_core::Sandbox::ReadOnly,
                    timeout_seconds: 10,
                    max_output_bytes: 4096,
                },
                telos_store::now(),
            )
            .await?;
        let evidence_id = store
            .add_evidence(
                &cycle,
                1,
                "fixture-artifact",
                "fixture-environment",
                "恢复后的证据".as_bytes(),
            )
            .await?;
        // Keep the fixture connection open so committed pages remain in the WAL.
        assert!(state.join("telos.db-wal").metadata()?.len() > 0);
        std::fs::write(state.join("evidence/proof.txt"), "持久证据")?;
        let dest = fixture.0.join("backup");
        let result = create(&state, &dest).await?;
        assert_eq!(result["database"]["goals"], 1);
        assert_eq!(verify(&dest).await?["verified"], true);
        assert!(!dest.join("state/telos.db-wal").exists());
        assert!(!dest.join("state/daemon.lock").exists());
        assert_eq!(
            std::fs::read_to_string(dest.join("state/evidence/proof.txt"))?,
            "持久证据"
        );
        let restored = fixture.0.join("restored");
        assert_eq!(
            stage_restore(&dest, &restored).await?["execution_enabled"],
            false
        );
        assert_eq!(inspect(&restored.join("state/telos.db")).await?["goals"], 1);
        assert_eq!(
            std::fs::read_to_string(restored.join("state/evidence/proof.txt"))?,
            "持久证据"
        );
        let before = std::fs::read(restored.join("state/telos.db"))?;
        let review = inspect_restore(&restored).await?;
        assert_eq!(review["inventory"]["goals"]["total"], 1);
        assert_eq!(
            review["snapshot_risks"]["queued_running_or_unknown_steps"],
            1
        );
        assert_eq!(review["execution_enabled"], false);
        assert_eq!(review["evidence_integrity_complete"], true);
        assert_eq!(review["inventory"]["evidence"]["checked"], 1);
        assert_eq!(
            review["inventory"]["evidence"]["records"][0]["id"],
            evidence_id
        );
        assert_eq!(
            review["inventory"]["evidence"]["records"][0]["mapping"]["kind"],
            "state_relative"
        );
        assert_eq!(
            review["inventory"]["evidence"]["records"][0]["integrity"],
            "verified"
        );

        let mh = review["manifest_sha256"].as_str().unwrap();
        let dh = review["database_sha256"].as_str().unwrap();
        assert!(prepare_restore(&restored, &"0".repeat(64), dh)
            .await
            .is_err());
        assert!(!restored.join("prepared-state").exists());
        let operation = crate::service::operation_lock(&restored)?;
        assert!(prepare_restore(&restored, mh, dh).await.is_err());
        drop(operation);
        // A previous incomplete candidate must not be mistaken for a committed result.
        std::fs::create_dir(restored.join(".prepare-interrupted"))?;
        std::fs::write(restored.join(".prepare-interrupted/partial"), "preserve me")?;
        let prepared = prepare_restore(&restored, mh, dh).await?;
        assert_eq!(prepared["report"]["counts"]["steps_unknown"], 1);
        assert_eq!(prepared["execution_enabled"], false);
        assert_eq!(prepare_restore(&restored, mh, dh).await?, prepared);
        assert_eq!(
            inspect_restore(&restored).await?["database_sha256"],
            review["database_sha256"]
        );
        assert!(restored.join(".prepare-interrupted/partial").exists());
        let (_tx, rx) = tokio::sync::watch::channel(false);
        assert!(telos_runtime::daemon::serve(
            &restored.join("prepared-state/state"),
            telos_runtime::runner::CodexRunner {
                program: fixture.0.join("must-not-run")
            },
            rx
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("requires reconciliation"));
        assert_eq!(prepare_restore(&restored, mh, dh).await?, prepared);
        std::fs::write(
            restored.join("prepared-state/state/evidence/proof.txt"),
            "changed",
        )?;
        assert!(prepare_restore(&restored, mh, dh).await.is_err());

        assert_eq!(std::fs::read(restored.join("state/telos.db"))?, before);
        // A staged image remains reviewable after the original archive is moved away.
        let moved = fixture.0.join("moved-backup");
        std::fs::rename(&dest, &moved)?;
        assert_eq!(inspect_restore(&restored).await?["verified_image"], true);
        std::fs::rename(&moved, &dest)?;

        let (_tx, rx) = tokio::sync::watch::channel(false);
        let error = telos_runtime::daemon::serve(
            &restored,
            telos_runtime::runner::CodexRunner {
                program: fixture.0.join("must-not-run"),
            },
            rx,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("requires reconciliation"));
        assert_eq!(std::fs::read(restored.join("state/telos.db"))?, before);
        assert!(stage_restore(&dest, &restored).await.is_err());
        std::fs::write(
            restored.join("state/evidence/proof.txt"),
            "changed after staging",
        )?;
        assert!(inspect_restore(&restored).await.is_err());

        assert!(create(&state, &dest).await.is_err());
        std::fs::write(dest.join("state/evidence/proof.txt"), "tampered")?;
        assert!(verify(&dest).await.is_err());
        store.close().await;
        Ok(())
    }
    #[tokio::test]
    async fn ownership_and_destination_guards_do_not_create_backups() -> Result<()> {
        let fixture = Fixture::new();
        let state = fixture.0.join("state");
        let store = telos_store::Store::open(&state).await?;
        store.close().await;
        let dest = fixture.0.join("backup");
        let lock = telos_runtime::daemon::maintenance_lock(&state)?;
        assert!(create(&state, &dest).await.is_err());
        assert!(!dest.exists());
        drop(lock);
        let operation = crate::service::operation_lock(&state)?;
        assert!(create(&state, &dest).await.is_err());
        assert!(!dest.exists());
        drop(operation);
        assert!(create(&state, &state.join("nested")).await.is_err());
        Ok(())
    }
    #[tokio::test]
    async fn real_test_runtime_links_survive_backup_restore_and_run_archival_without_target_copy(
    ) -> Result<()> {
        let f = Fixture::new();
        let state = f.0.join("state");
        let store = telos_store::Store::open(&state).await?;
        store.close().await;
        let outside = f.0.join("external-runtime");
        std::fs::create_dir(&outside)?;
        std::fs::write(
            outside.join("must-not-copy"),
            b"synthetic external runtime bytes",
        )?;
        let relative = "verification-lease-fixture-0/codex-home/tmp/arg0/codex-fixture";
        let bootstrap = state.join("runs").join(relative);
        std::fs::create_dir_all(&bootstrap)?;
        std::os::unix::fs::symlink(outside.join("must-not-copy"), bootstrap.join("applypatch"))?;
        std::os::unix::fs::symlink(&outside, bootstrap.join("runtime-directory"))?;
        let backup = f.0.join("backup");
        create(&state, &backup).await?;
        verify(&backup).await?;
        let (_, manifest) = read_json_bounded(&backup.join("manifest.json"), 32 * 1024 * 1024)?;
        let entries = manifest["files"].as_object().unwrap();
        assert_eq!(
            entries[&format!("runs/{relative}/applypatch")]["type"],
            "symlink"
        );
        assert!(!entries.keys().any(|p| p.contains("runtime-directory/")));
        let recovery = f.0.join("recovery");
        stage_restore(&backup, &recovery).await?;
        let inspection = inspect_restore(&recovery).await?;
        let manifest_hash = inspection["manifest_sha256"].as_str().unwrap();
        let database_hash = inspection["database_sha256"].as_str().unwrap();
        prepare_restore(&recovery, manifest_hash, database_hash).await?;
        let image = recovery.join("prepared-state");
        let receipt = format!(
            "{:x}",
            Sha256::digest(std::fs::read(image.join("receipt.json"))?)
        );
        let archived = f.0.join("archived");
        archive_runs(&image, &receipt, &archived).await?;
        prepared_receipt(&archived, manifest_hash, database_hash).await?;
        let historical = archived
            .join("state/recovery-history/original-runs")
            .join(relative);
        assert_eq!(
            std::fs::read_link(historical.join("applypatch"))?,
            outside.join("must-not-copy")
        );
        assert_eq!(
            std::fs::read_link(historical.join("runtime-directory"))?,
            outside
        );
        assert!(
            std::fs::symlink_metadata(historical.join("runtime-directory"))?
                .file_type()
                .is_symlink()
        );
        Ok(())
    }

    #[tokio::test]
    async fn symlinks_and_incomplete_or_extra_payloads_are_rejected() -> Result<()> {
        let fixture = Fixture::new();
        let state = fixture.0.join("state");
        let store = telos_store::Store::open(&state).await?;
        store.close().await;
        let secret = fixture.0.join("not-state");
        std::fs::write(&secret, "outside")?;
        std::os::unix::fs::symlink(&secret, state.join("link"))?;
        let partial = fixture.0.join("partial");
        assert!(create(&state, &partial).await.is_err());
        assert!(!partial.join("manifest.json").exists());
        assert!(verify(&partial).await.is_err());
        assert!(!partial.join("state/link").exists());
        std::fs::remove_file(state.join("link"))?;
        let dest = fixture.0.join("backup");
        create(&state, &dest).await?;
        std::fs::write(dest.join("state/untracked"), "extra")?;
        assert!(verify(&dest).await.is_err());
        std::fs::remove_file(dest.join("state/untracked"))?;
        std::os::unix::fs::symlink(&secret, dest.join("state/link"))?;
        assert!(verify(&dest).await.is_err());
        Ok(())
    }
    #[test]
    fn fifo_input_is_rejected_without_opening_it() -> Result<()> {
        let fixture = Fixture::new();
        let path = fixture.0.join("pipe");
        let cpath = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())?;
        // SAFETY: cpath is a valid NUL-terminated path in this isolated fixture.
        ensure!(
            unsafe { libc::mkfifo(cpath.as_ptr(), 0o600) } == 0,
            "mkfifo failed"
        );
        assert!(regular(&path).is_err());
        Ok(())
    }
    #[test]
    fn restore_publication_never_replaces_an_existing_directory() -> Result<()> {
        let fixture = Fixture::new();
        let source = fixture.0.join("source");
        let dest = fixture.0.join("dest");
        std::fs::create_dir(&source)?;
        std::fs::create_dir(&dest)?;
        std::fs::write(source.join("file"), "payload")?;
        assert!(publish_exclusive(&source, &dest).is_err());
        assert!(source.join("file").exists());
        assert!(std::fs::read_dir(&dest)?.next().is_none());
        Ok(())
    }
    #[tokio::test]
    async fn recovery_review_is_bounded_and_binds_both_markers_and_manifest() -> Result<()> {
        let fixture = Fixture::new();
        let state = fixture.0.join("state");
        let store = telos_store::Store::open(&state).await?;
        for i in 0..101 {
            let p = fixture.0.join(format!("project-{i}"));
            std::fs::create_dir(&p)?;
            store.register_project(&format!("project-{i}"), &p).await?;
        }
        store.close().await;
        let backup = fixture.0.join("backup");
        let recovery = fixture.0.join("recovery");
        create(&state, &backup).await?;
        stage_restore(&backup, &recovery).await?;
        let review = inspect_restore(&recovery).await?;
        assert_eq!(review["inventory"]["projects"]["total"], 101);
        assert_eq!(review["inventory"]["projects"]["truncated"], true);
        assert_eq!(
            review["inventory"]["projects"]["records"]
                .as_array()
                .unwrap()
                .len(),
            100
        );
        let marker = std::fs::read(recovery.join("restore-required.json"))?;
        std::fs::write(recovery.join("restore-required.json"), "{}")?;
        assert!(inspect_restore(&recovery).await.is_err());
        std::fs::write(recovery.join("restore-required.json"), marker)?;
        std::fs::write(recovery.join("backup-manifest.json"), "{}")?;
        assert!(inspect_restore(&recovery).await.is_err());
        Ok(())
    }
    #[tokio::test]
    async fn evidence_review_checks_beyond_display_and_query_pages() -> Result<()> {
        let mut db = SqliteConnection::connect("sqlite::memory:").await?;
        sqlx::query(
            "CREATE TABLE evidence(id TEXT PRIMARY KEY,path TEXT,sha256 TEXT,byte_length INTEGER)",
        )
        .execute(&mut db)
        .await?;
        let mut files = BTreeMap::new();
        let mut tx = db.begin().await?;
        // Include an empty ID to ensure review does not skip rows at its initial cursor.
        for i in 0..1002 {
            let id = if i == 0 {
                String::new()
            } else {
                format!("e-{i:04}")
            };
            let path = format!("evidence/{id}.bin");
            sqlx::query("INSERT INTO evidence VALUES(?,?,?,3)")
                .bind(&id)
                .bind(&path)
                .bind("hash")
                .execute(&mut *tx)
                .await?;
            files.insert(path, json!({"sha256":"hash","bytes":3}));
        }
        sqlx::query("UPDATE evidence SET sha256='wrong' WHERE id='e-1001'")
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        let review = review_evidence(&mut db, &files, Path::new("/not-accessed")).await?;
        assert_eq!(review["checked"], 1002);
        assert_eq!(review["invalid"], 1);
        assert_eq!(review["records"].as_array().unwrap().len(), 100);
        assert_eq!(review["truncated"], true);
        assert_eq!(review["integrity_complete"], false);
        db.close().await?;
        Ok(())
    }
    #[tokio::test]
    async fn archive_consistency_does_not_bypass_invalid_evidence_bindings() -> Result<()> {
        let f = Fixture::new();
        let state = f.0.join("state");
        let store = telos_store::Store::open(&state).await?;
        store.register_project("p", &f.0).await?;
        let contract: telos_core::Contract = serde_json::from_value(
            json!({"goal":"fixture","delivery_scope":"fixture","criteria":[{"id":"a","description":"fixture"}],"max_attempts":1,"interval_seconds":null}),
        )?;
        let goal = store
            .create_loop("p", "fixture", telos_core::LoopKind::Delivery, &contract)
            .await?;
        let cycle = store.start_cycle(&goal.id, 1, "fixture").await?;
        let evidence = store.add_evidence(&cycle, 1, "a", "e", b"original").await?;
        store.close().await;
        // Corrupt fixture content before making the archive: archive hashes remain self-consistent.
        std::fs::write(state.join(format!("evidence/{evidence}.bin")), b"changed")?;
        let backup = f.0.join("backup");
        let recovery = f.0.join("recovery");
        create(&state, &backup).await?;
        stage_restore(&backup, &recovery).await?;
        let review = inspect_restore(&recovery).await?;
        assert_eq!(review["verified_image"], true);
        assert_eq!(review["evidence_integrity_complete"], false);
        assert_eq!(
            review["inventory"]["evidence"]["records"][0]["integrity"],
            "binding_mismatch"
        );
        assert!(prepare_restore(
            &recovery,
            review["manifest_sha256"].as_str().unwrap(),
            review["database_sha256"].as_str().unwrap()
        )
        .await
        .is_err());
        assert!(!recovery.join("prepared-state").exists());
        Ok(())
    }
    #[tokio::test]
    async fn workspace_links_and_executable_bits_roundtrip_without_target_reads() -> Result<()> {
        use std::os::unix::ffi::OsStringExt;
        let f = Fixture::new();
        let state = f.0.join("state");
        let store = telos_store::Store::open(&state).await?;
        store.close().await;
        let workspace = state.join("workspaces/workspace-fixture");
        std::fs::create_dir_all(&workspace)?;
        let outside = f.0.join("outside");
        std::fs::create_dir(&outside)?;
        std::fs::write(outside.join("must-not-copy"), "synthetic external content")?;
        let targets = [
            ("absolute", outside.into_os_string()),
            ("relative", std::ffi::OsString::from("missing-file")),
            ("cycle", std::ffi::OsString::from("cycle")),
            ("raw", std::ffi::OsString::from_vec(vec![b'x', 0xff])),
        ];
        for (name, target) in &targets {
            std::os::unix::fs::symlink(target, workspace.join(name))?;
        }
        std::fs::write(workspace.join("run.sh"), "#!/bin/sh\nexit 0\n")?;
        std::fs::set_permissions(
            workspace.join("run.sh"),
            std::fs::Permissions::from_mode(0o751),
        )?;
        let backup = f.0.join("backup");
        let recovery = f.0.join("recovery");
        create(&state, &backup).await?;
        assert_eq!(verify(&backup).await?["verified"], true);
        let (_, manifest) = read_json_bounded(&backup.join("manifest.json"), 32 * 1024 * 1024)?;
        assert_eq!(manifest["format"], 2);
        assert_eq!(
            manifest["files"]["workspaces/workspace-fixture/run.sh"]["executable"],
            true
        );
        assert!(!manifest["files"]
            .as_object()
            .unwrap()
            .keys()
            .any(|p| p.contains("must-not-copy")));
        stage_restore(&backup, &recovery).await?;
        let review = inspect_restore(&recovery).await?;
        prepare_restore(
            &recovery,
            review["manifest_sha256"].as_str().unwrap(),
            review["database_sha256"].as_str().unwrap(),
        )
        .await?;
        for root in [
            backup.join("state"),
            recovery.join("state"),
            recovery.join("prepared-state/state"),
        ] {
            let ws = root.join("workspaces/workspace-fixture");
            for (name, target) in &targets {
                assert_eq!(std::fs::read_link(ws.join(name))?.as_os_str(), target);
            }
            assert_eq!(
                std::fs::metadata(ws.join("run.sh"))?.permissions().mode() & 0o777,
                0o700
            );
        }
        let link = backup.join("state/workspaces/workspace-fixture/relative");
        std::fs::remove_file(&link)?;
        std::os::unix::fs::symlink("changed", link)?;
        assert!(verify(&backup).await.is_err());
        Ok(())
    }
    #[tokio::test]
    async fn workspace_root_links_remain_rejected() -> Result<()> {
        let f = Fixture::new();
        let state = f.0.join("state");
        let store = telos_store::Store::open(&state).await?;
        store.close().await;
        std::fs::create_dir(state.join("workspaces"))?;
        std::os::unix::fs::symlink(&f.0, state.join("workspaces/workspace-pointer"))?;
        assert!(create(&state, &f.0.join("backup")).await.is_err());
        Ok(())
    }
    #[tokio::test]
    async fn recovery_workspace_check_is_bound_and_read_only() -> Result<()> {
        let f = Fixture::new();
        let project = f.0.join("project");
        std::fs::create_dir(&project)?;
        let git = |args: &[&str]| -> Result<()> {
            let out = std::process::Command::new("git")
                .env_clear()
                .env("PATH", std::env::var_os("PATH").unwrap_or_default())
                .env("HOME", &f.0)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .arg("-C")
                .arg(&project)
                .args([
                    "-c",
                    "user.name=Fixture",
                    "-c",
                    "user.email=fixture@example.invalid",
                    "-c",
                    "commit.gpgSign=false",
                    "-c",
                    "core.hooksPath=/dev/null",
                ])
                .args(args)
                .output()?;
            ensure!(out.status.success(), "fixture Git failed");
            Ok(())
        };
        git(&["init", "--quiet"])?;
        std::fs::write(project.join("value"), "base")?;
        git(&["add", "value"])?;
        git(&["commit", "-qm", "fixture"])?;
        let state = f.0.join("state");
        let store = telos_store::Store::open(&state).await?;
        store.register_project("p", &project).await?;
        store
            .configure_project(
                "p",
                0,
                &telos_core::ProjectExecutionPolicy {
                    enabled: true,
                    sandbox: telos_core::Sandbox::WorkspaceWrite,
                    timeout_seconds: 30,
                    max_output_bytes: 65536,
                    max_steps_per_cycle: 3,
                },
            )
            .await?;
        let contract: telos_core::Contract = serde_json::from_value(
            json!({"goal":"fixture","delivery_scope":"fixture","criteria":[{"id":"a","description":"fixture"}],"max_attempts":1,"interval_seconds":null}),
        )?;
        let goal = store
            .create_loop("p", "fixture", telos_core::LoopKind::Delivery, &contract)
            .await?;
        let workspace = telos_runtime::workspace::prepare(&store, &goal.id, 1, 1, "HEAD").await?;
        let plan = telos_core::VerificationPlan {
            checks: vec![telos_core::FileHashCheck {
                criterion_id: "a".into(),
                relative_path: "value".into(),
                expected_sha256: format!("{:x}", Sha256::digest(b"approved")),
            }],
            test_checks: vec![],
            log_checks: vec![],
        };
        store.configure_verification(&goal.id, 1, 0, &plan).await?;
        store.close().await;
        let backup = f.0.join("backup");
        let recovery = f.0.join("recovery");
        create(&state, &backup).await?;
        stage_restore(&backup, &recovery).await?;
        let review = inspect_restore(&recovery).await?;
        let mh = review["manifest_sha256"].as_str().unwrap();
        let dh = review["database_sha256"].as_str().unwrap();
        let checked =
            check_workspace(&recovery, &workspace.id, Path::new(&workspace.path), mh, dh).await?;
        assert_eq!(checked["identity_verified"], true);
        assert_eq!(checked["execution_enabled"], false);
        assert!(check_workspace(
            &recovery,
            &workspace.id,
            &f.0.join("not-accessed"),
            &"0".repeat(64),
            dh
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("fingerprints changed"));
        let copied = recovery.join("state/workspaces").join(&workspace.id);
        assert!(check_workspace(&recovery, &workspace.id, &copied, mh, dh)
            .await
            .unwrap_err()
            .to_string()
            .contains("registration points to a different checkout"));
        assert_eq!(
            inspect_restore(&recovery).await?["database_sha256"],
            review["database_sha256"]
        );
        prepare_restore(&recovery, mh, dh).await?;
        let image = recovery.join("prepared-state");
        let original = preparation_inventory(&image)?;
        let receipt_hash = format!(
            "{:x}",
            Sha256::digest(std::fs::read(image.join("receipt.json"))?)
        );
        assert!(inspect_workspace(
            &image,
            &"0".repeat(64),
            &workspace.id,
            Path::new("/not-accessed")
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("receipt changed"));
        let inspection = inspect_workspace(
            &image,
            &receipt_hash,
            &workspace.id,
            Path::new(&workspace.path),
        )
        .await?;
        assert_eq!(preparation_inventory(&image)?, original);
        assert_eq!(inspection["execution_enabled"], false);
        let materials = f.0.join("materials");
        std::fs::create_dir(&materials)?;
        let material = b"Synthetic fixture: exclusive worktree inspection; no real approval";
        std::fs::write(materials.join("report.txt"), material)?;
        let review = json!({"project_id":"p","manifest_sha256":mh,"decision_id":"synthetic-adopt","operator":"fixture","statement":"Synthetic fixture only","reviewed_external_state":true,"evidence":[{"id":"report.txt","sha256":format!("{:x}",Sha256::digest(material))}]});
        let decision = json!({"review":review,"workspace_id":workspace.id,"candidate_path":workspace.path,"expected_snapshot_sha256":inspection["snapshot_sha256"],"exclusive_access_confirmed":true});
        let decision_file = f.0.join("adoption.json");
        std::fs::write(&decision_file, serde_json::to_vec(&decision)?)?;
        let output = f.0.join("adopted");
        std::fs::write(
            Path::new(&workspace.path).join("value"),
            b"changed after inspection",
        )?;
        assert!(
            adopt_workspace(&image, &receipt_hash, &decision_file, &materials, &output)
                .await
                .is_err()
        );
        assert!(!output.exists());
        assert_eq!(preparation_inventory(&image)?, original);
        std::fs::write(Path::new(&workspace.path).join("value"), b"base")?;
        let result =
            adopt_workspace(&image, &receipt_hash, &decision_file, &materials, &output).await?;
        prepared_receipt(&output, mh, dh).await?;
        assert_eq!(preparation_inventory(&image)?, original);
        let readonly = telos_store::Store::open_recovery_review(&output.join("state")).await?;
        assert_eq!(
            readonly
                .workspace_for_loop(&goal.id, 1)
                .await?
                .unwrap()
                .state,
            "ready"
        );
        assert_eq!(
            readonly.verification_plan(&goal.id, 1).await?.unwrap()["version"],
            2
        );
        assert_eq!(
            readonly.recovery_project_status("p").await?.unwrap()["state"],
            "held"
        );
        readonly.close().await;
        let mut release_review = review;
        release_review["decision_id"] = json!("synthetic-release");
        std::fs::write(&decision_file, serde_json::to_vec(&release_review)?)?;
        let released = f.0.join("released");
        review_project(
            &output,
            result["receipt_sha256"].as_str().unwrap(),
            &decision_file,
            &materials,
            &released,
        )
        .await?;
        prepared_receipt(&released, mh, dh).await?;
        let released_hash = format!(
            "{:x}",
            Sha256::digest(std::fs::read(released.join("receipt.json"))?)
        );
        let before_preflight = preparation_inventory(&released)?;
        let not_checked = inspect_activation(&released, &released_hash, false).await?;
        assert_eq!(not_checked["checks_passed"], false);
        assert!(not_checked["blockers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["kind"] == "external_workspace_not_checked"));
        assert_eq!(
            inspect_activation(&released, &released_hash, true).await?["checks_passed"],
            true
        );
        std::fs::write(
            Path::new(&workspace.path).join("value"),
            b"external change after adoption",
        )?;
        let changed = inspect_activation(&released, &released_hash, true).await?;
        assert_eq!(changed["checks_passed"], false);
        assert!(changed["blockers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["kind"] == "workspace_changed_since_adoption"));
        assert_eq!(preparation_inventory(&released)?, before_preflight);
        let activated = f.0.canonicalize()?.join("activated-workspace");
        let activation_decision = telos_core::RecoveryActivation {
            manifest_sha256: mh.into(),
            source_receipt_sha256: released_hash.clone(),
            target_state: activated.join("state").to_string_lossy().into(),
            decision_id: "synthetic-activation".into(),
            operator: "fixture".into(),
            statement: "Synthetic project activation fixture only".into(),
            source_instance_retired: true,
            exclusive_access_confirmed: true,
        };
        let activation_file = f.0.join("activation.json");
        std::fs::write(&activation_file, serde_json::to_vec(&activation_decision)?)?;
        assert!(
            activate_restore(&released, &released_hash, &activation_file, &activated)
                .await
                .is_err()
        );
        assert!(!activated.exists());
        std::fs::write(Path::new(&workspace.path).join("value"), b"base")?;
        activate_restore(&released, &released_hash, &activation_file, &activated).await?;
        let readonly = telos_store::Store::open_recovery_review(&activated.join("state")).await?;
        assert_eq!(
            readonly.get_loop(&goal.id).await?.state,
            telos_core::LoopState::Paused
        );
        readonly.close().await;
        assert_eq!(preparation_inventory(&released)?, before_preflight);

        // Synthetic worker proves continuation through the real daemon and verifier.
        use std::time::Duration;
        use telos_runtime::{
            daemon::{self, Request},
            runner::CodexRunner,
        };
        let fake = f.0.join("recovered-worker");
        std::fs::write(
            &fake,
            r#"#!/usr/bin/env python3
import pathlib,sys,json
sys.stdin.read()
root=pathlib.Path(sys.argv[sys.argv.index('-C')+1])
(root/'value').write_text('approved')
pathlib.Path(sys.argv[sys.argv.index('-o')+1]).write_text(json.dumps({'status':'done','summary':'synthetic recovery edit','evidence_paths':[],'remaining_work':[]}))
print(json.dumps({'type':'turn.completed','usage':{'output_tokens':0}}))
"#,
        )?;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700))?;
        let active_state = activated.join("state");
        let (stop, shutdown) = tokio::sync::watch::channel(false);
        let service = tokio::spawn({
            let root = active_state.clone();
            async move { daemon::serve(&root, CodexRunner { program: fake }, shutdown).await }
        });
        let outcome: Result<()> = async {
            tokio::time::timeout(Duration::from_secs(10), async {
                while daemon::call(&active_state, &Request::Ping).await.is_err() {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await?;
            let current = daemon::call(
                &active_state,
                &Request::GetLoop {
                    id: goal.id.clone(),
                },
            )
            .await?;
            assert_eq!(current["state"], "paused");
            let start = Request::StartLoop {
                id: goal.id.clone(),
                revision: 1,
                request_key: "post-recovery-work".into(),
            };
            assert!(daemon::call(&active_state, &start).await.is_err());
            assert_eq!(
                std::fs::read(Path::new(&workspace.path).join("value"))?,
                b"base"
            );
            daemon::call(
                &active_state,
                &Request::Control {
                    id: goal.id.clone(),
                    expected_version: current["version"].as_i64().unwrap(),
                    action: "resume".into(),
                },
            )
            .await?;
            let first = daemon::call(&active_state, &start).await?;
            let repeated = daemon::call(&active_state, &start).await?;
            assert_eq!(repeated["step_id"], first["step_id"]);
            assert_eq!(repeated["cycle_id"], first["cycle_id"]);
            assert_eq!(repeated["reused"], true);
            tokio::time::timeout(Duration::from_secs(30), async {
                loop {
                    let current = daemon::call(
                        &active_state,
                        &Request::GetLoop {
                            id: goal.id.clone(),
                        },
                    )
                    .await?;
                    if current["state"] == "completed" {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Ok::<(), anyhow::Error>(())
            })
            .await??;
            let delivery = daemon::call(
                &active_state,
                &Request::DeliveryResult {
                    id: goal.id.clone(),
                    revision: 1,
                },
            )
            .await?;
            assert_eq!(delivery["verdict"], "passed");
            assert_eq!(
                std::fs::read(Path::new(&workspace.path).join("value"))?,
                b"approved"
            );
            assert_eq!(std::fs::read(project.join("value"))?, b"base");
            Ok(())
        }
        .await;
        let _ = stop.send(true);
        tokio::time::timeout(Duration::from_secs(10), service).await???;
        outcome?;

        let path = released.join("receipt.json");
        let (_, mut receipt) = read_json_bounded(&path, 32 * 1024 * 1024)?;
        receipt["workspace_adoption"]["adoption"]["candidate_path"] = json!("/forged");
        std::fs::write(&path, serde_json::to_vec(&receipt)?)?;
        assert!(prepared_receipt(&released, mh, dh).await.is_err());
        Ok(())
    }
}
