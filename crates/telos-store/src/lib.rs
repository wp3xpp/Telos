//! Transactional Loop ledger. Worker reports alone never complete a Loop.
mod answers;
mod diagnosis;
mod observations;
mod project_stages;
mod recovery;
mod repair;
pub use repair::RepairCandidate;
mod workspaces;
pub use diagnosis::DiagnosisCandidate;
mod environment_operations;
mod events;
pub use environment_operations::EnvironmentIntent;
mod inspection;
mod logs;
mod notifications;
mod remediation_links;
mod remediation_recovery;
pub use notifications::NotificationLease;
mod policy;
mod queue;
mod schedule;
mod verification_lease;
mod verification_plan;
use anyhow::{bail, ensure, Context, Result};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::{
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
    Row, Sqlite, SqlitePool, Transaction,
};
use std::{
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use telos_core::{evaluate, Contract, LoopKind, LoopRecord, LoopState, Verdict, Verification};
use uuid::Uuid;

#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
    root: PathBuf,
}

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
fn id(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::new_v4())
}
fn tag<T: serde::Serialize>(v: T) -> Result<String> {
    Ok(serde_json::to_value(v)?
        .as_str()
        .context("enum must be a string")?
        .to_owned())
}
fn parse<T: DeserializeOwned>(s: String) -> Result<T> {
    Ok(serde_json::from_value(Value::String(s))?)
}

impl Store {
    pub async fn open(root: &Path) -> Result<Self> {
        tokio::fs::create_dir_all(root.join("evidence")).await?;
        let root = tokio::fs::canonicalize(root).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).await?;
        }
        let options = SqliteConnectOptions::new()
            .filename(root.join("telos.db"))
            .create_if_missing(true)
            .foreign_keys(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Full)
            .busy_timeout(Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await?;
        let mut tx = pool.begin().await?;
        sqlx::query("CREATE TABLE IF NOT EXISTS schema_migrations(version INTEGER PRIMARY KEY, applied_at INTEGER NOT NULL)")
            .execute(&mut *tx).await?;
        let version: i64 =
            sqlx::query_scalar("SELECT COALESCE(MAX(version),0) FROM schema_migrations")
                .fetch_one(&mut *tx)
                .await?;
        ensure!(
            version <= 19,
            "database schema is newer than this Telos binary"
        );
        if version < 1 {
            sqlx::Executor::execute(&mut *tx, include_str!("schema.sql")).await?;
        }
        if version < 2 {
            sqlx::Executor::execute(&mut *tx, include_str!("migration_002.sql")).await?;
        }
        if version < 3 {
            sqlx::Executor::execute(&mut *tx, include_str!("migration_003.sql")).await?;
        }
        if version < 4 {
            sqlx::Executor::execute(&mut *tx, include_str!("migration_004.sql")).await?;
        }
        if version < 5 {
            sqlx::Executor::execute(&mut *tx, include_str!("migration_005.sql")).await?;
        }
        if version < 6 {
            sqlx::Executor::execute(&mut *tx, include_str!("migration_006.sql")).await?;
        }
        if version < 7 {
            sqlx::Executor::execute(&mut *tx, include_str!("migration_007.sql")).await?;
        }
        if version < 8 {
            sqlx::Executor::execute(&mut *tx, include_str!("migration_008.sql")).await?;
        }
        if version < 9 {
            sqlx::Executor::execute(&mut *tx, include_str!("migration_009.sql")).await?;
        }
        if version < 10 {
            sqlx::Executor::execute(&mut *tx, include_str!("migration_010.sql")).await?;
        }
        if version < 11 {
            sqlx::Executor::execute(&mut *tx, include_str!("migration_011.sql")).await?;
        }
        if version < 12 {
            sqlx::Executor::execute(&mut *tx, include_str!("migration_012.sql")).await?;
        }
        if version < 13 {
            sqlx::Executor::execute(&mut *tx, include_str!("migration_013.sql")).await?;
        }
        if version < 14 {
            sqlx::Executor::execute(&mut *tx, include_str!("migration_014.sql")).await?;
        }
        if version < 15 {
            sqlx::Executor::execute(&mut *tx, include_str!("migration_015.sql")).await?;
        }
        if version < 16 {
            sqlx::Executor::execute(&mut *tx, include_str!("migration_016.sql")).await?;
        }
        if version < 17 {
            sqlx::Executor::execute(&mut *tx, include_str!("migration_017.sql")).await?;
        }
        if version < 18 {
            sqlx::Executor::execute(&mut *tx, include_str!("migration_018.sql")).await?;
        }
        if version < 19 {
            sqlx::Executor::execute(&mut *tx, include_str!("migration_019.sql")).await?;
        }
        tx.commit().await?;
        Ok(Self { pool, root })
    }

    /// Open a closed prepared snapshot without migrations, WAL recovery or database writes.
    pub async fn open_recovery_review(root: &Path) -> Result<Self> {
        let root = root.canonicalize()?;
        ensure!(
            std::fs::symlink_metadata(root.join("telos.db"))?.is_file(),
            "review database must be regular"
        );
        for name in ["telos.db-wal", "telos.db-shm"] {
            ensure!(
                !root.join(name).try_exists()?,
                "review requires a closed standalone database"
            );
        }
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(root.join("telos.db"))
                    .read_only(true)
                    .immutable(true),
            )
            .await?;
        let version: i64 =
            sqlx::query_scalar("SELECT COALESCE(MAX(version),0) FROM schema_migrations")
                .fetch_one(&pool)
                .await?;
        ensure!(
            (18..=19).contains(&version),
            "review requires schema 18 or 19; prepare a copy with the current build first"
        );
        let store = Self { pool, root };
        ensure!(
            store.recovery_preparation().await?.is_some(),
            "review requires a prepared recovery image"
        );
        Ok(store)
    }

    pub async fn close(&self) {
        self.pool.close().await;
    }

    async fn event(
        tx: &mut Transaction<'_, Sqlite>,
        loop_id: Option<&str>,
        kind: &str,
        body: Value,
        notify: bool,
    ) -> Result<()> {
        let r = sqlx::query("INSERT INTO events(loop_id,kind,body,created_at) VALUES(?,?,?,?)")
            .bind(loop_id)
            .bind(kind)
            .bind(body.to_string())
            .bind(now())
            .execute(&mut **tx)
            .await?;
        if notify {
            if kind == "verification_recorded" {
                sqlx::query("UPDATE outbox SET state='superseded' WHERE state='pending' AND event_sequence IN (SELECT sequence FROM events WHERE loop_id=? AND kind='verification_recorded')")
                    .bind(loop_id).execute(&mut **tx).await?;
            }
            sqlx::query("INSERT INTO outbox(event_sequence) VALUES(?)")
                .bind(r.last_insert_rowid())
                .execute(&mut **tx)
                .await?;
        }
        Ok(())
    }

    pub async fn register_project(&self, project_id: &str, root: &Path) -> Result<()> {
        ensure!(!project_id.trim().is_empty(), "project id required");
        let root = root.canonicalize()?;
        ensure!(root.is_dir(), "project root must be a directory");
        let root = root.to_str().context("project path must be UTF-8")?;
        let mut tx = self.pool.begin().await?;
        if let Some(old) = sqlx::query("SELECT root FROM projects WHERE id=?")
            .bind(project_id)
            .fetch_optional(&mut *tx)
            .await?
        {
            ensure!(
                old.get::<String, _>("root") == root,
                "project id already points to another directory"
            );
        } else {
            sqlx::query("INSERT INTO projects(id,root,created_at) VALUES(?,?,?)")
                .bind(project_id)
                .bind(root)
                .bind(now())
                .execute(&mut *tx)
                .await?;
            Self::event(
                &mut tx,
                None,
                "project_registered",
                json!({"project_id":project_id,"root":root}),
                false,
            )
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn create_loop(
        &self,
        project_id: &str,
        name: &str,
        kind: LoopKind,
        contract: &Contract,
    ) -> Result<LoopRecord> {
        contract.validate(kind)?;
        ensure!(!name.trim().is_empty(), "loop name required");
        let loop_id = id("loop");
        let mut tx = self.pool.begin().await?;
        Self::require_recovery_clear(&mut tx, project_id).await?;
        sqlx::query("INSERT INTO loops(id,project_id,name,kind,state,revision,version,next_due_at,created_at,updated_at) VALUES(?,?,?,?,'active',1,1,?,?,?)")
            .bind(&loop_id).bind(project_id).bind(name).bind(tag(kind)?).bind(now()).bind(now()).bind(now()).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO contracts(loop_id,revision,body,created_at) VALUES(?,1,?,?)")
            .bind(&loop_id)
            .bind(serde_json::to_string(contract)?)
            .bind(now())
            .execute(&mut *tx)
            .await?;
        Self::event(
            &mut tx,
            Some(&loop_id),
            "loop_created",
            json!({"revision":1}),
            false,
        )
        .await?;
        tx.commit().await?;
        self.get_loop(&loop_id).await
    }

    async fn get_in(tx: &mut Transaction<'_, Sqlite>, loop_id: &str) -> Result<LoopRecord> {
        let row = sqlx::query("SELECT l.*,c.body FROM loops l JOIN contracts c ON c.loop_id=l.id AND c.revision=l.revision WHERE l.id=?")
            .bind(loop_id).fetch_optional(&mut **tx).await?.context("loop not found")?;
        Ok(LoopRecord {
            id: row.get("id"),
            project_id: row.get("project_id"),
            name: row.get("name"),
            kind: parse(row.get("kind"))?,
            state: parse(row.get("state"))?,
            revision: row.get("revision"),
            version: row.get("version"),
            contract: serde_json::from_str(&row.get::<String, _>("body"))?,
            next_due_at: row.get("next_due_at"),
        })
    }

    pub async fn get_loop(&self, loop_id: &str) -> Result<LoopRecord> {
        let mut tx = self.pool.begin().await?;
        let record = Self::get_in(&mut tx, loop_id).await?;
        tx.commit().await?;
        Ok(record)
    }

    pub async fn list_loops(&self) -> Result<Vec<LoopRecord>> {
        let mut tx = self.pool.begin().await?;
        let ids = sqlx::query("SELECT id FROM loops ORDER BY created_at,id")
            .fetch_all(&mut *tx)
            .await?;
        let mut records = Vec::new();
        for row in ids {
            records.push(Self::get_in(&mut tx, &row.get::<String, _>("id")).await?);
        }
        tx.commit().await?;
        Ok(records)
    }

    pub async fn update_contract(
        &self,
        loop_id: &str,
        expected_version: i64,
        contract: &Contract,
    ) -> Result<LoopRecord> {
        let mut tx = self.pool.begin().await?;
        let old = Self::get_in(&mut tx, loop_id).await?;
        ensure!(old.version == expected_version, "version conflict");
        ensure!(
            !matches!(old.state, LoopState::Completed | LoopState::Stopped),
            "terminal loop cannot be revised"
        );
        contract.validate(old.kind)?;
        Self::require_verification_idle(&mut tx, &old.project_id).await?;
        sqlx::query("UPDATE loop_schedules SET enabled=0,suspended_reason='contract_revised',updated_at=? WHERE loop_id=?")
            .bind(now()).bind(loop_id).execute(&mut *tx).await?;
        let running: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM step_runs s JOIN work_items w ON w.id=s.work_item_id JOIN cycles c ON c.id=w.cycle_id WHERE c.loop_id=? AND s.state IN ('running','outcome_unknown')")
            .bind(loop_id).fetch_one(&mut *tx).await?;
        ensure!(
            running == 0,
            "reconcile in-flight execution before changing contract"
        );
        sqlx::query("UPDATE step_runs SET state='cancelled' WHERE state IN ('pending','ready') AND work_item_id IN (SELECT w.id FROM work_items w JOIN cycles c ON c.id=w.cycle_id WHERE c.loop_id=?)")
            .bind(loop_id).execute(&mut *tx).await?;
        sqlx::query("UPDATE cycles SET state='cancelled',conclusion='contract_revised',closed_at=? WHERE loop_id=? AND state NOT IN ('closed','cancelled')")
            .bind(now()).bind(loop_id).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO contracts(loop_id,revision,body,created_at) VALUES(?,?,?,?)")
            .bind(loop_id)
            .bind(old.revision + 1)
            .bind(serde_json::to_string(contract)?)
            .bind(now())
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE loops SET revision=revision+1,version=version+1,next_due_at=?,updated_at=? WHERE id=?")
            .bind(now()).bind(now()).bind(loop_id).execute(&mut *tx).await?;
        Self::event(
            &mut tx,
            Some(loop_id),
            "contract_revised",
            json!({"revision":old.revision+1}),
            false,
        )
        .await?;
        tx.commit().await?;
        self.get_loop(loop_id).await
    }

    pub async fn control(
        &self,
        loop_id: &str,
        expected_version: i64,
        action: &str,
    ) -> Result<LoopRecord> {
        let mut tx = self.pool.begin().await?;
        let old = Self::get_in(&mut tx, loop_id).await?;
        ensure!(old.version == expected_version, "version conflict");
        let state = match action {
            "pause" if matches!(old.state, LoopState::Active | LoopState::Blocked) => {
                LoopState::Paused
            }
            "resume" if matches!(old.state, LoopState::Paused | LoopState::Blocked) => {
                LoopState::Active
            }
            "stop" if !matches!(old.state, LoopState::Completed | LoopState::Stopped) => {
                LoopState::Stopped
            }
            _ => bail!("invalid state transition"),
        };
        if action == "resume" {
            Self::require_verification_idle(&mut tx, &old.project_id).await?;
            let uncertain: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM step_runs s JOIN work_items w ON w.id=s.work_item_id JOIN cycles c ON c.id=w.cycle_id WHERE c.loop_id=? AND s.state='outcome_unknown'")
                .bind(loop_id).fetch_one(&mut *tx).await?;
            ensure!(
                uncertain == 0,
                "uncertain execution must be reconciled before resume"
            );
        }
        sqlx::query("UPDATE loops SET state=?,version=version+1,updated_at=? WHERE id=?")
            .bind(tag(state)?)
            .bind(now())
            .bind(loop_id)
            .execute(&mut *tx)
            .await?;
        Self::event(
            &mut tx,
            Some(loop_id),
            "loop_controlled",
            json!({"action":action,"state":state}),
            false,
        )
        .await?;
        tx.commit().await?;
        self.get_loop(loop_id).await
    }

    pub async fn start_cycle(
        &self,
        loop_id: &str,
        revision: i64,
        trigger_key: &str,
    ) -> Result<String> {
        self.start_cycle_inner(loop_id, revision, trigger_key, false)
            .await
    }

    pub async fn start_scheduled_cycle(
        &self,
        loop_id: &str,
        revision: i64,
        trigger_key: &str,
    ) -> Result<String> {
        self.start_cycle_inner(loop_id, revision, trigger_key, true)
            .await
    }

    async fn start_cycle_inner(
        &self,
        loop_id: &str,
        revision: i64,
        trigger_key: &str,
        exclusive: bool,
    ) -> Result<String> {
        ensure!(!trigger_key.trim().is_empty(), "trigger key required");
        let mut tx = self.pool.begin().await?;
        let record = Self::get_in(&mut tx, loop_id).await?;
        Self::require_recovery_clear(&mut tx, &record.project_id).await?;
        Self::require_environment_certain(&mut tx, &record.project_id).await?;
        ensure!(record.state == LoopState::Active, "loop is not active");
        ensure!(record.revision == revision, "stale contract");
        if let Some(row) =
            sqlx::query("SELECT id,contract_revision FROM cycles WHERE loop_id=? AND trigger_key=?")
                .bind(loop_id)
                .bind(trigger_key)
                .fetch_optional(&mut *tx)
                .await?
        {
            ensure!(
                row.get::<i64, _>("contract_revision") == revision,
                "trigger key belongs to an old contract"
            );
            return Ok(row.get("id"));
        }
        // An observation received during an open cycle updates that cycle;
        // a different trigger cannot create concurrent remediation.
        if let Some(row) = sqlx::query(
            "SELECT id FROM cycles WHERE loop_id=? AND state NOT IN ('closed','cancelled')",
        )
        .bind(loop_id)
        .fetch_optional(&mut *tx)
        .await?
        {
            ensure!(!exclusive, "cycle already active");
            return Ok(row.get("id"));
        }
        let cycle_id = id("cycle");
        sqlx::query("INSERT INTO cycles(id,loop_id,contract_revision,state,trigger_key,created_at) VALUES(?,?,?,'observing',?,?)")
            .bind(&cycle_id).bind(loop_id).bind(revision).bind(trigger_key).bind(now()).execute(&mut *tx).await?;
        sqlx::query("UPDATE loops SET next_due_at=NULL,version=version+1,updated_at=? WHERE id=?")
            .bind(now())
            .bind(loop_id)
            .execute(&mut *tx)
            .await?;
        Self::event(
            &mut tx,
            Some(loop_id),
            "cycle_started",
            json!({"cycle_id":cycle_id,"trigger":trigger_key}),
            false,
        )
        .await?;
        tx.commit().await?;
        Ok(cycle_id)
    }

    /// Trusted ingestion API: copies bounded evidence into immutable-by-convention storage.
    /// MCP must never expose arbitrary source-file reads through this method.
    pub async fn add_evidence(
        &self,
        cycle_id: &str,
        revision: i64,
        artifact: &str,
        environment: &str,
        bytes: &[u8],
    ) -> Result<String> {
        ensure!(
            !bytes.is_empty() && bytes.len() <= 8 * 1024 * 1024,
            "evidence must contain 1..8MiB"
        );
        ensure!(
            !artifact.is_empty() && !environment.is_empty(),
            "evidence binding required"
        );
        let evidence_id = id("evidence");
        let relative = format!("evidence/{evidence_id}.bin");
        let path = self.root.join(&relative);
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query("SELECT c.loop_id,c.contract_revision,c.state,l.revision FROM cycles c JOIN loops l ON l.id=c.loop_id WHERE c.id=?")
            .bind(cycle_id).fetch_one(&mut *tx).await?;
        ensure!(
            row.get::<i64, _>("contract_revision") == revision
                && row.get::<i64, _>("revision") == revision,
            "stale evidence"
        );
        ensure!(
            !matches!(row.get::<&str, _>("state"), "closed" | "cancelled"),
            "cycle is terminal"
        );
        let temporary = path.with_extension("tmp");
        {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            file.write_all(bytes)?;
            file.sync_all()?;
        }
        std::fs::rename(&temporary, &path)?;
        let hash = format!("{:x}", Sha256::digest(bytes));
        sqlx::query("INSERT INTO evidence(id,cycle_id,contract_revision,artifact,environment,path,sha256,byte_length,created_at) VALUES(?,?,?,?,?,?,?,?,?)")
            .bind(&evidence_id).bind(cycle_id).bind(revision).bind(artifact).bind(environment)
            .bind(relative).bind(hash).bind(bytes.len() as i64).bind(now()).execute(&mut *tx).await?;
        Self::event(
            &mut tx,
            Some(row.get("loop_id")),
            "evidence_recorded",
            json!({"evidence_id":evidence_id,"cycle_id":cycle_id}),
            false,
        )
        .await?;
        tx.commit().await?;
        Ok(evidence_id)
    }

    pub async fn verify(&self, verification: &Verification) -> Result<Verdict> {
        self.verify_inner(verification, None, false, None).await
    }

    pub async fn verify_configured(
        &self,
        verification: &Verification,
        plan_version: i64,
    ) -> Result<Verdict> {
        self.verify_inner(verification, Some(plan_version), false, None)
            .await
    }

    pub async fn verify_observed(
        &self,
        verification: &Verification,
        plan_version: i64,
        quiet_pending: bool,
    ) -> Result<Verdict> {
        self.verify_inner(verification, Some(plan_version), quiet_pending, None)
            .await
    }

    pub async fn verify_leased(
        &self,
        verification: &Verification,
        plan_version: i64,
        quiet_pending: bool,
        lease: &str,
    ) -> Result<Verdict> {
        self.verify_inner(verification, Some(plan_version), quiet_pending, Some(lease))
            .await
    }

    async fn verify_inner(
        &self,
        verification: &Verification,
        plan_version: Option<i64>,
        quiet_pending: bool,
        lease: Option<&str>,
    ) -> Result<Verdict> {
        let mut tx = self.pool.begin().await?;
        let cycle = sqlx::query("SELECT * FROM cycles WHERE id=?")
            .bind(&verification.cycle_id)
            .fetch_optional(&mut *tx)
            .await?
            .context("cycle not found")?;
        ensure!(
            !matches!(cycle.get::<&str, _>("state"), "closed" | "cancelled"),
            "cycle is terminal"
        );
        let loop_id: String = cycle.get("loop_id");
        let record = Self::get_in(&mut tx, &loop_id).await?;
        if let Some(lease) = lease {
            let owned: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM verification_leases WHERE id=? AND cycle_id=? AND contract_revision=? AND plan_version=? AND state='running'")
                .bind(lease).bind(&verification.cycle_id).bind(verification.contract_revision).bind(plan_version).fetch_one(&mut *tx).await?;
            ensure!(owned == 1, "verification lease missing or stale");
        } else {
            Self::require_verification_idle(&mut tx, &record.project_id).await?;
        }
        if let Some(expected) = plan_version {
            let current: Option<i64> = sqlx::query_scalar(
                "SELECT version FROM verification_plans WHERE loop_id=? AND contract_revision=?",
            )
            .bind(&loop_id)
            .bind(verification.contract_revision)
            .fetch_optional(&mut *tx)
            .await?;
            ensure!(
                current == Some(expected),
                "verification plan changed during observation"
            );
        }
        ensure!(
            record.state == LoopState::Active,
            "loop must be active for verification"
        );
        ensure!(
            record.revision == verification.contract_revision
                && cycle.get::<i64, _>("contract_revision") == record.revision,
            "stale verification"
        );
        let delivery =
            sqlx::query("SELECT d.id,d.state,d.policy_revision,p.revision AS current_policy FROM delivery_workspaces d JOIN cycles c ON c.id=d.cycle_id JOIN loops l ON l.id=c.loop_id JOIN project_execution_policies p ON p.project_id=l.project_id WHERE d.cycle_id=?")
                .bind(&verification.cycle_id)
                .fetch_optional(&mut *tx)
                .await?;
        let has_delivery = delivery.is_some();
        if let Some(workspace) = delivery {
            ensure!(
                workspace.get::<String, _>("state") == "ready"
                    && workspace.get::<i64, _>("policy_revision")
                        == workspace.get::<i64, _>("current_policy"),
                "workspace state or authority changed"
            );
            let workspace_id: String = workspace.get("id");
            let snapshot=sqlx::query("SELECT plan_version,step_count,manifest FROM delivery_snapshots WHERE artifact=? AND workspace_id=?")
                .bind(&verification.artifact).bind(&workspace_id).fetch_optional(&mut *tx).await?.context("delivery workspace requires artifact-bound acceptance")?;
            let steps:i64=sqlx::query_scalar("SELECT COUNT(*) FROM step_runs s JOIN work_items w ON w.id=s.work_item_id WHERE w.cycle_id=?").bind(&verification.cycle_id).fetch_one(&mut *tx).await?;
            ensure!(
                Some(snapshot.get::<i64, _>("plan_version")) == plan_version
                    && snapshot.get::<i64, _>("step_count") == steps
                    && verification.environment == format!("local-workspace:{workspace_id}"),
                "stale or mismatched delivery snapshot"
            );
            let manifest: String = snapshot.get("manifest");
            ensure!(
                verification.artifact
                    == format!(
                        "workspace:{workspace_id}:{:x}",
                        Sha256::digest(manifest.as_bytes())
                    ),
                "snapshot integrity failure"
            );
        }
        let configured: Option<String> = sqlx::query_scalar(
            "SELECT body FROM verification_plans WHERE loop_id=? AND contract_revision=?",
        )
        .bind(&loop_id)
        .bind(record.revision)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(configured) = configured {
            let plan: telos_core::VerificationPlan = serde_json::from_str(&configured)?;
            if !plan.test_checks.is_empty() {
                let lease = lease.context("test acceptance requires verification ownership")?;
                let reserved: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM verification_test_attempts WHERE lease_id=? AND cycle_id=? AND plan_version=?")
                    .bind(lease).bind(&verification.cycle_id).bind(plan_version).fetch_one(&mut *tx).await?;
                ensure!(
                    reserved == 1
                        && (has_delivery
                            || (record.kind == LoopKind::Continuous
                                && plan
                                    .test_checks
                                    .iter()
                                    .all(|c| c.adapter == telos_core::observation::ADAPTER))),
                    "test attempt or delivery workspace missing"
                );
            }
        }
        let verdict = evaluate(&record.contract, verification)?;
        if has_delivery && verdict == Verdict::Passed {
            let review: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM delivery_reviews WHERE artifact=?")
                    .bind(&verification.artifact)
                    .fetch_one(&mut *tx)
                    .await?;
            ensure!(review == 1, "delivery review artifact missing");
        }
        if verdict == Verdict::Passed {
            let unfinished: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM step_runs s JOIN work_items w ON w.id=s.work_item_id WHERE w.cycle_id=? AND s.state IN ('pending','ready','running','outcome_unknown')")
                .bind(&verification.cycle_id).fetch_one(&mut *tx).await?;
            ensure!(
                unfinished == 0,
                "unfinished or uncertain execution prevents closing cycle"
            );
        }
        for result in &verification.results {
            for evidence_id in &result.evidence_ids {
                let evidence = sqlx::query("SELECT * FROM evidence WHERE id=?")
                    .bind(evidence_id)
                    .fetch_optional(&mut *tx)
                    .await?
                    .context("evidence not found")?;
                ensure!(
                    evidence.get::<String, _>("cycle_id") == verification.cycle_id,
                    "cross-cycle evidence rejected"
                );
                ensure!(
                    evidence.get::<i64, _>("contract_revision") == record.revision,
                    "stale evidence revision"
                );
                ensure!(
                    evidence.get::<String, _>("artifact") == verification.artifact
                        && evidence.get::<String, _>("environment") == verification.environment,
                    "evidence artifact/environment mismatch"
                );
                let path = self.root.join(evidence.get::<String, _>("path"));
                let bytes = std::fs::read(path).context("evidence unavailable")?;
                ensure!(
                    format!("{:x}", Sha256::digest(&bytes)) == evidence.get::<String, _>("sha256"),
                    "evidence integrity failure"
                );
            }
        }
        let previous: Option<String> = sqlx::query_scalar("SELECT json_extract(e.body,'$.verdict') FROM outbox o JOIN events e ON e.sequence=o.event_sequence JOIN cycles c ON c.id=json_extract(e.body,'$.cycle_id') WHERE c.loop_id=? AND c.contract_revision=? AND e.kind='verification_recorded' ORDER BY e.sequence DESC LIMIT 1")
            .bind(&loop_id).bind(record.revision).fetch_optional(&mut *tx).await?;
        let notify = if quiet_pending && verdict == Verdict::Inconclusive {
            false
        } else if record.kind == LoopKind::Delivery && verdict == Verdict::Passed {
            true
        } else {
            match previous {
                Some(previous) => previous != tag(verdict)?,
                None => verdict != Verdict::Passed,
            }
        };
        let verification_id = id("verification");
        sqlx::query("INSERT INTO verifications(id,cycle_id,contract_revision,verdict,body,created_at,plan_version) VALUES(?,?,?,?,?,?,?)")
            .bind(&verification_id).bind(&verification.cycle_id).bind(record.revision).bind(tag(verdict)?)
            .bind(serde_json::to_string(verification)?).bind(now()).bind(plan_version).execute(&mut *tx).await?;
        if verdict == Verdict::Passed {
            sqlx::query(
                "UPDATE cycles SET state='closed',conclusion='verified',closed_at=? WHERE id=?",
            )
            .bind(now())
            .bind(&verification.cycle_id)
            .execute(&mut *tx)
            .await?;
            let (state, due) = match record.kind {
                LoopKind::Delivery => (LoopState::Completed, None),
                LoopKind::Continuous => (
                    LoopState::Active,
                    Some(
                        now()
                            + record
                                .contract
                                .interval_seconds
                                .context("interval missing")? as i64,
                    ),
                ),
            };
            sqlx::query(
                "UPDATE loops SET state=?,next_due_at=?,version=version+1,updated_at=? WHERE id=?",
            )
            .bind(tag(state)?)
            .bind(due)
            .bind(now())
            .bind(&loop_id)
            .execute(&mut *tx)
            .await?;
        } else {
            sqlx::query("UPDATE cycles SET state=? WHERE id=?")
                .bind(if verdict == Verdict::Failed {
                    "acting"
                } else {
                    "verifying"
                })
                .bind(&verification.cycle_id)
                .execute(&mut *tx)
                .await?;
            sqlx::query("UPDATE loops SET version=version+1,updated_at=? WHERE id=?")
                .bind(now())
                .bind(&loop_id)
                .execute(&mut *tx)
                .await?;
        }
        Self::event(
            &mut tx,
            Some(&loop_id),
            "verification_recorded",
            json!({"cycle_id":verification.cycle_id,"verdict":verdict,"verification_id":verification_id,"plan_version":plan_version,"lease_id":lease}),
            notify,
        )
        .await?;
        Self::link_remediation_verification(
            &mut tx,
            &loop_id,
            verification,
            &verification_id,
            plan_version,
            lease,
            verdict,
        )
        .await?;
        tx.commit().await?;
        Ok(verdict)
    }

    pub async fn events(&self, loop_id: &str) -> Result<Vec<Value>> {
        let rows = sqlx::query(
            "SELECT sequence,kind,body,created_at FROM events WHERE loop_id=? ORDER BY sequence",
        )
        .bind(loop_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(|row| Ok(json!({"sequence":row.get::<i64,_>("sequence"),"kind":row.get::<String,_>("kind"),"body":serde_json::from_str::<Value>(&row.get::<String,_>("body"))?,"created_at":row.get::<i64,_>("created_at")}))).collect()
    }
}
