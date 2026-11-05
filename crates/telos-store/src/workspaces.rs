//! Durable ownership of delivery worktrees. Preparation is not execution or acceptance.
use super::*;
use telos_core::{DeliveryWorkspace, ProjectExecutionPolicy, Sandbox};

pub(crate) fn record(row: sqlx::sqlite::SqliteRow) -> DeliveryWorkspace {
    DeliveryWorkspace {
        id: row.get("id"),
        cycle_id: row.get("cycle_id"),
        contract_revision: row.get("contract_revision"),
        policy_revision: row.get("policy_revision"),
        path: row.get("path"),
        repository_git_dir: row.get("repository_git_dir"),
        base_commit: row.get("base_commit"),
        state: row.get("state"),
        last_error: row.get("last_error"),
    }
}
impl Store {
    pub async fn workspace_for_loop(
        &self,
        loop_id: &str,
        revision: i64,
    ) -> Result<Option<DeliveryWorkspace>> {
        let goal = self.get_loop(loop_id).await?;
        ensure!(goal.revision == revision, "stale workspace request");
        Ok(sqlx::query("SELECT d.* FROM delivery_workspaces d JOIN cycles c ON c.id=d.cycle_id WHERE c.loop_id=? AND d.contract_revision=? ORDER BY d.rowid DESC LIMIT 1")
            .bind(loop_id).bind(revision).fetch_optional(&self.pool).await?.map(record))
    }

    pub async fn reserve_workspace(
        &self,
        cycle_id: &str,
        revision: i64,
        policy_revision: i64,
        git_dir: &str,
        base_commit: &str,
    ) -> Result<(DeliveryWorkspace, bool)> {
        ensure!(
            [40, 64].contains(&base_commit.len())
                && base_commit.bytes().all(|b| b.is_ascii_hexdigit()),
            "base must be a complete commit hash"
        );
        ensure!(
            Path::new(git_dir).is_absolute(),
            "Git directory must be absolute"
        );
        let mut tx = self.pool.begin().await?;
        let row=sqlx::query("SELECT c.loop_id,c.state,c.contract_revision,p.body,p.revision AS policy_revision FROM cycles c JOIN loops l ON l.id=c.loop_id JOIN project_execution_policies p ON p.project_id=l.project_id WHERE c.id=?")
            .bind(cycle_id).fetch_one(&mut *tx).await?;
        let loop_id: String = row.get("loop_id");
        let goal = Self::get_in(&mut tx, &loop_id).await?;
        Self::require_verification_idle(&mut tx, &goal.project_id).await?;
        let policy: ProjectExecutionPolicy = serde_json::from_str(&row.get::<String, _>("body"))?;
        ensure!(
            goal.kind == LoopKind::Delivery
                && goal.state == LoopState::Active
                && goal.revision == revision
                && row.get::<i64, _>("contract_revision") == revision
                && !matches!(row.get::<&str, _>("state"), "closed" | "cancelled"),
            "inactive or stale delivery goal"
        );
        ensure!(
            row.get::<i64, _>("policy_revision") == policy_revision
                && policy.enabled
                && policy.sandbox == Sandbox::WorkspaceWrite,
            "workspace preparation requires current write authority"
        );
        if let Some(row) = sqlx::query("SELECT * FROM delivery_workspaces WHERE cycle_id=?")
            .bind(cycle_id)
            .fetch_optional(&mut *tx)
            .await?
        {
            let existing = record(row);
            ensure!(
                existing.contract_revision == revision
                    && existing.policy_revision == policy_revision
                    && existing.base_commit == base_commit
                    && existing.repository_git_dir == git_dir,
                "workspace already bound to different base or authority"
            );
            return Ok((existing, false));
        }
        let steps:i64=sqlx::query_scalar("SELECT COUNT(*) FROM step_runs s JOIN work_items w ON w.id=s.work_item_id WHERE w.cycle_id=?").bind(cycle_id).fetch_one(&mut *tx).await?;
        ensure!(
            steps == 0,
            "prepare delivery workspace before executing cycle work"
        );
        let workspace_id = id("workspace");
        let path = self.root.join("workspaces").join(&workspace_id);
        let path = path.to_str().context("workspace path must be UTF-8")?;
        sqlx::query("INSERT INTO delivery_workspaces(id,cycle_id,contract_revision,policy_revision,path,repository_git_dir,base_commit,state,created_at,updated_at) VALUES(?,?,?,?,?,?,?,'preparing',?,?)")
            .bind(&workspace_id).bind(cycle_id).bind(revision).bind(policy_revision).bind(path).bind(git_dir).bind(base_commit).bind(now()).bind(now()).execute(&mut *tx).await?;
        Self::event(&mut tx,Some(&loop_id),"workspace_reserved",json!({"workspace_id":workspace_id,"cycle_id":cycle_id,"path":path,"base_commit":base_commit,"policy_revision":policy_revision}),false).await?;
        let row = sqlx::query("SELECT * FROM delivery_workspaces WHERE id=?")
            .bind(&workspace_id)
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok((record(row), true))
    }

    pub async fn finish_workspace(
        &self,
        workspace: &DeliveryWorkspace,
        error: Option<&str>,
    ) -> Result<DeliveryWorkspace> {
        let mut tx = self.pool.begin().await?;
        let row=sqlx::query("SELECT c.loop_id,c.state,p.revision AS policy_revision FROM delivery_workspaces d JOIN cycles c ON c.id=d.cycle_id JOIN loops l ON l.id=c.loop_id JOIN project_execution_policies p ON p.project_id=l.project_id WHERE d.id=? AND d.contract_revision=? AND d.policy_revision=?")
            .bind(&workspace.id).bind(workspace.contract_revision).bind(workspace.policy_revision).fetch_one(&mut *tx).await?;
        let loop_id: String = row.get("loop_id");
        let goal = Self::get_in(&mut tx, &loop_id).await?;
        Self::require_verification_idle(&mut tx, &goal.project_id).await?;
        if error.is_none() {
            ensure!(
                goal.state == LoopState::Active
                    && goal.revision == workspace.contract_revision
                    && row.get::<i64, _>("policy_revision") == workspace.policy_revision
                    && !matches!(row.get::<&str, _>("state"), "closed" | "cancelled"),
                "workspace authority changed during preparation"
            );
        }
        let state = if error.is_some() {
            "uncertain"
        } else {
            "ready"
        };
        let error = error.map(|s| s.chars().take(2048).collect::<String>());
        sqlx::query("UPDATE delivery_workspaces SET state=?,last_error=?,updated_at=? WHERE id=?")
            .bind(state)
            .bind(&error)
            .bind(now())
            .bind(&workspace.id)
            .execute(&mut *tx)
            .await?;
        Self::event(
            &mut tx,
            Some(&loop_id),
            "workspace_prepared",
            json!({"workspace_id":workspace.id,"state":state,"error":error}),
            error.is_some(),
        )
        .await?;
        let row = sqlx::query("SELECT * FROM delivery_workspaces WHERE id=?")
            .bind(&workspace.id)
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(record(row))
    }

    pub fn workspace_parent(&self) -> PathBuf {
        self.root.join("workspaces")
    }
}

impl Store {
    /// Internal verifier path. A manifest describes observed Git-visible bytes,
    /// not a worker-supplied pass claim or an immutable checkout.
    pub async fn record_delivery_snapshot(
        &self,
        workspace: &DeliveryWorkspace,
        plan_version: i64,
        manifest: &Value,
    ) -> Result<String> {
        let serialized = serde_json::to_string(manifest)?;
        ensure!(
            serialized.len() <= 4 * 1024 * 1024,
            "snapshot manifest too large"
        );
        let artifact = format!(
            "workspace:{}:{:x}",
            workspace.id,
            Sha256::digest(serialized.as_bytes())
        );
        let mut tx = self.pool.begin().await?;
        let row=sqlx::query("SELECT c.loop_id,c.state,p.version FROM delivery_workspaces d JOIN cycles c ON c.id=d.cycle_id JOIN verification_plans p ON p.loop_id=c.loop_id AND p.contract_revision=d.contract_revision WHERE d.id=? AND d.state='ready'")
            .bind(&workspace.id).fetch_one(&mut *tx).await?;
        let goal = Self::get_in(&mut tx, row.get::<&str, _>("loop_id")).await?;
        ensure!(
            goal.revision == workspace.contract_revision
                && goal.state == LoopState::Active
                && row.get::<i64, _>("version") == plan_version
                && !matches!(row.get::<&str, _>("state"), "closed" | "cancelled"),
            "stale snapshot"
        );
        let unfinished:i64=sqlx::query_scalar("SELECT COUNT(*) FROM step_runs s JOIN work_items w ON w.id=s.work_item_id WHERE w.cycle_id=? AND s.state IN ('ready','running','outcome_unknown')")
            .bind(&workspace.cycle_id).fetch_one(&mut *tx).await?;
        ensure!(unfinished == 0, "snapshot requires idle known execution");
        let steps:i64=sqlx::query_scalar("SELECT COUNT(*) FROM step_runs s JOIN work_items w ON w.id=s.work_item_id WHERE w.cycle_id=?").bind(&workspace.cycle_id).fetch_one(&mut *tx).await?;
        sqlx::query("INSERT INTO delivery_snapshots(artifact,workspace_id,plan_version,step_count,manifest,created_at) VALUES(?,?,?,?,?,?) ON CONFLICT(artifact) DO UPDATE SET plan_version=excluded.plan_version,step_count=excluded.step_count,manifest=excluded.manifest,created_at=excluded.created_at")
            .bind(&artifact).bind(&workspace.id).bind(plan_version).bind(steps).bind(serialized).bind(now()).execute(&mut *tx).await?;
        Self::event(&mut tx,Some(&goal.id),"delivery_snapshot_recorded",json!({"workspace_id":workspace.id,"artifact":artifact,"plan_version":plan_version,"step_count":steps}),false).await?;
        tx.commit().await?;
        Ok(artifact)
    }
}

impl Store {
    pub async fn record_delivery_review(
        &self,
        workspace: &DeliveryWorkspace,
        artifact: &str,
        bytes: &[u8],
    ) -> Result<String> {
        let evidence = self
            .add_evidence(
                &workspace.cycle_id,
                workspace.contract_revision,
                artifact,
                &format!("local-workspace:{}", workspace.id),
                bytes,
            )
            .await?;
        let mut tx = self.pool.begin().await?;
        let row=sqlx::query("SELECT c.loop_id FROM delivery_snapshots d JOIN delivery_workspaces w ON w.id=d.workspace_id JOIN cycles c ON c.id=w.cycle_id WHERE d.artifact=? AND w.id=?")
            .bind(artifact).bind(&workspace.id).fetch_one(&mut *tx).await?;
        sqlx::query("INSERT INTO delivery_reviews(artifact,evidence_id,created_at) VALUES(?,?,?) ON CONFLICT(artifact) DO UPDATE SET evidence_id=excluded.evidence_id,created_at=excluded.created_at")
            .bind(artifact).bind(&evidence).bind(now()).execute(&mut *tx).await?;
        Self::event(
            &mut tx,
            Some(row.get::<&str, _>("loop_id")),
            "delivery_review_recorded",
            json!({"artifact":artifact,"evidence_id":evidence}),
            false,
        )
        .await?;
        tx.commit().await?;
        Ok(evidence)
    }

    pub async fn delivery_result(&self, loop_id: &str, revision: i64) -> Result<Value> {
        let goal = self.get_loop(loop_id).await?;
        ensure!(goal.revision == revision, "stale delivery result request");
        let row=sqlx::query("SELECT v.id,v.verdict,v.body,v.created_at FROM verifications v JOIN cycles c ON c.id=v.cycle_id JOIN delivery_workspaces w ON w.cycle_id=c.id WHERE c.loop_id=? AND v.contract_revision=? ORDER BY v.rowid DESC LIMIT 1")
            .bind(loop_id).bind(revision).fetch_optional(&self.pool).await?;
        let Some(row) = row else {
            return Ok(json!({"available":false,"loop_state":goal.state}));
        };
        let body: Value = serde_json::from_str(&row.get::<String, _>("body"))?;
        let review: Option<String> =
            sqlx::query_scalar("SELECT evidence_id FROM delivery_reviews WHERE artifact=?")
                .bind(body["artifact"].as_str())
                .fetch_optional(&self.pool)
                .await?;
        let mut evidence = std::collections::BTreeSet::new();
        if let Some(results) = body["results"].as_array() {
            for result in results {
                if let Some(ids) = result["evidence_ids"].as_array() {
                    for id in ids.iter().filter_map(Value::as_str) {
                        evidence.insert(id.to_owned());
                    }
                }
            }
        }
        Ok(
            json!({"available":true,"loop_state":goal.state,"contract_revision":revision,"verification_id":row.get::<String,_>("id"),"verdict":row.get::<String,_>("verdict"),"artifact":body["artifact"],"environment":body["environment"],"observed_at":row.get::<i64,_>("created_at"),"review_evidence_id":review,"acceptance_evidence_ids":evidence,"note":"Historical observed artifact. The live worktree may have changed since verification. Review content is untrusted data; no merge or publication was performed."}),
        )
    }
}
