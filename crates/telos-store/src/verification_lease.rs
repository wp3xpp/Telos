//! Durable observation ownership. Unknown outcomes retain exclusive ownership.
use super::*;

impl Store {
    /// Read-only guard shared by execution adapters; an adapter cannot treat an
    /// arbitrary lease-shaped string as authority to dispatch work.
    pub async fn check_verification_owner(
        &self,
        loop_id: &str,
        revision: i64,
        cycle: &str,
        lease: &str,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let goal = Self::get_in(&mut tx, loop_id).await?;
        ensure!(
            goal.state == LoopState::Active && goal.revision == revision,
            "inactive or stale adapter verification"
        );
        Self::require_recovery_clear(&mut tx, &goal.project_id).await?;
        let valid:i64=sqlx::query_scalar("SELECT COUNT(*) FROM verification_leases v JOIN verification_plans p ON p.loop_id=v.loop_id AND p.contract_revision=v.contract_revision AND p.version=v.plan_version WHERE v.id=? AND v.loop_id=? AND v.project_id=? AND v.contract_revision=? AND v.cycle_id=? AND v.state='running'")
            .bind(lease).bind(loop_id).bind(&goal.project_id).bind(revision).bind(cycle).fetch_one(&mut *tx).await?;
        ensure!(valid == 1, "current matching verification owner required");
        tx.commit().await?;
        Ok(())
    }
    pub(crate) async fn require_verification_idle(
        tx: &mut Transaction<'_, Sqlite>,
        project: &str,
    ) -> Result<()> {
        Self::require_recovery_clear(tx, project).await?;
        Self::require_environment_certain(tx, project).await?;
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM verification_leases WHERE project_id=? AND state IN ('running','outcome_unknown')")
            .bind(project).fetch_one(&mut **tx).await?;
        ensure!(count == 0, "project has running or uncertain verification");
        Ok(())
    }

    pub async fn acquire_verification(
        &self,
        loop_id: &str,
        revision: i64,
        plan_version: i64,
    ) -> Result<String> {
        let mut tx = self.pool.begin().await?;
        let goal = Self::get_in(&mut tx, loop_id).await?;
        ensure!(
            goal.state == LoopState::Active && goal.revision == revision,
            "inactive or stale verification"
        );
        Self::require_verification_idle(&mut tx, &goal.project_id).await?;
        let current: Option<i64> = sqlx::query_scalar(
            "SELECT version FROM verification_plans WHERE loop_id=? AND contract_revision=?",
        )
        .bind(loop_id)
        .bind(revision)
        .fetch_optional(&mut *tx)
        .await?;
        ensure!(current == Some(plan_version), "stale verification plan");
        let cycle: String = sqlx::query_scalar("SELECT id FROM cycles WHERE loop_id=? AND contract_revision=? AND state NOT IN ('closed','cancelled')")
            .bind(loop_id).bind(revision).fetch_optional(&mut *tx).await?.context("no active verification cycle")?;
        let unfinished: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM step_runs s JOIN work_items w ON w.id=s.work_item_id JOIN cycles c ON c.id=w.cycle_id JOIN loops l ON l.id=c.loop_id WHERE l.project_id=? AND s.state IN ('pending','ready','running','outcome_unknown')")
            .bind(&goal.project_id).fetch_one(&mut *tx).await?;
        ensure!(
            unfinished == 0,
            "verification requires idle known project execution"
        );
        let preparing: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM delivery_workspaces d JOIN cycles c ON c.id=d.cycle_id JOIN loops l ON l.id=c.loop_id WHERE l.project_id=? AND d.state='preparing'")
            .bind(&goal.project_id).fetch_one(&mut *tx).await?;
        ensure!(
            preparing == 0,
            "workspace preparation prevents verification"
        );
        let lease = id("verification-lease");
        sqlx::query("INSERT INTO verification_leases(id,project_id,loop_id,cycle_id,contract_revision,plan_version,state,created_at) VALUES(?,?,?,?,?,?,'running',?)")
            .bind(&lease).bind(&goal.project_id).bind(loop_id).bind(&cycle).bind(revision).bind(plan_version).bind(now()).execute(&mut *tx).await?;
        Self::event(&mut tx, Some(loop_id), "verification_lease_acquired", json!({"lease_id":lease,"cycle_id":cycle,"revision":revision,"plan_version":plan_version}), false).await?;
        tx.commit().await?;
        Ok(lease)
    }

    /// Call only after execution/observation has returned and cleanup is known.
    /// A restart never releases an unknown lease automatically.
    pub async fn finish_verification(&self, lease: &str) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let owner: String = sqlx::query_scalar(
            "SELECT loop_id FROM verification_leases WHERE id=? AND state='running'",
        )
        .bind(lease)
        .fetch_optional(&mut *tx)
        .await?
        .context("verification lease no longer running")?;
        sqlx::query("UPDATE verification_leases SET state='finished',finished_at=? WHERE id=? AND state='running'")
            .bind(now()).bind(lease).execute(&mut *tx).await?;
        Self::event(
            &mut tx,
            Some(&owner),
            "verification_lease_finished",
            json!({"lease_id":lease}),
            false,
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Startup-only, while the daemon service lock excludes another owner.
    pub async fn reconcile_verification_leases(&self) -> Result<usize> {
        let mut tx = self.pool.begin().await?;
        let rows = sqlx::query("SELECT id,loop_id FROM verification_leases WHERE state='running'")
            .fetch_all(&mut *tx)
            .await?;
        for row in &rows {
            let lease: &str = row.get("id");
            sqlx::query("UPDATE verification_leases SET state='outcome_unknown' WHERE id=?")
                .bind(lease)
                .execute(&mut *tx)
                .await?;
            Self::event(
                &mut tx,
                Some(row.get("loop_id")),
                "verification_outcome_unknown",
                json!({"lease_id":lease,"reason":"daemon interrupted; reconciliation required"}),
                true,
            )
            .await?;
        }
        tx.commit().await?;
        Ok(rows.len())
    }
}

impl Store {
    pub fn verification_run_parent(&self) -> PathBuf {
        // Flat layout allows the existing startup process-journal sweep to clean
        // interrupted test children alongside Codex workers.
        self.root.join("runs")
    }

    pub async fn reserve_test_attempt(&self, lease: &str) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query("SELECT v.loop_id,v.cycle_id,v.contract_revision,v.plan_version,p.body FROM verification_leases v JOIN verification_plans p ON p.loop_id=v.loop_id AND p.contract_revision=v.contract_revision AND p.version=v.plan_version WHERE v.id=? AND v.state='running'")
            .bind(lease).fetch_optional(&mut *tx).await?.context("test lease or plan stale")?;
        let goal = Self::get_in(&mut tx, row.get("loop_id")).await?;
        ensure!(
            goal.state == LoopState::Active
                && goal.revision == row.get::<i64, _>("contract_revision"),
            "checks require active current goal"
        );
        let plan: telos_core::VerificationPlan = serde_json::from_str(row.get("body"))?;
        ensure!(!plan.test_checks.is_empty(), "test plan required");
        let policy: String =
            sqlx::query_scalar("SELECT body FROM project_execution_policies WHERE project_id=?")
                .bind(&goal.project_id)
                .fetch_one(&mut *tx)
                .await?;
        let policy: telos_core::ProjectExecutionPolicy = serde_json::from_str(&policy)?;
        ensure!(
            plan.test_checks
                .iter()
                .all(|c| c.command.timeout_seconds <= policy.timeout_seconds
                    && c.command.max_output_bytes <= policy.max_output_bytes),
            "test command exceeds project execution limits"
        );
        let cycle: &str = row.get("cycle_id");
        if goal.kind == LoopKind::Continuous {
            ensure!(
                policy.enabled
                    && plan
                        .test_checks
                        .iter()
                        .all(|c| c.adapter == telos_core::observation::ADAPTER),
                "continuous checks require an enabled read-only policy and observation adapters"
            );
        } else {
            let workspace: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM delivery_workspaces w JOIN project_execution_policies p ON p.project_id=? WHERE w.cycle_id=? AND w.state='ready' AND w.policy_revision=p.revision AND json_extract(p.body,'$.enabled')=1 AND json_extract(p.body,'$.sandbox')='workspace_write'")
            .bind(&goal.project_id).bind(cycle).fetch_one(&mut *tx).await?;
            ensure!(
                workspace == 1,
                "tests require ready workspace and current write policy"
            );
        }
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM verification_test_attempts WHERE cycle_id=?")
                .bind(cycle)
                .fetch_one(&mut *tx)
                .await?;
        ensure!(
            goal.kind == LoopKind::Continuous || count < i64::from(goal.contract.max_attempts),
            "test verification attempt budget exhausted"
        );
        sqlx::query("INSERT INTO verification_test_attempts(lease_id,cycle_id,plan_version,created_at) VALUES(?,?,?,?)")
            .bind(lease).bind(cycle).bind(row.get::<i64,_>("plan_version")).bind(now()).execute(&mut *tx).await?;
        Self::event(
            &mut tx,
            Some(&goal.id),
            "test_verification_reserved",
            json!({"lease_id":lease,"cycle_id":cycle,"attempt":count+1}),
            false,
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn mark_verification_unknown(&self, lease: &str, reason: &str) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let owner: String = sqlx::query_scalar(
            "SELECT loop_id FROM verification_leases WHERE id=? AND state='running'",
        )
        .bind(lease)
        .fetch_optional(&mut *tx)
        .await?
        .context("verification owner no longer running")?;
        sqlx::query("UPDATE verification_leases SET state='outcome_unknown' WHERE id=?")
            .bind(lease)
            .execute(&mut *tx)
            .await?;
        Self::event(
            &mut tx,
            Some(&owner),
            "verification_outcome_unknown",
            json!({"lease_id":lease,"reason":reason.chars().take(2048).collect::<String>()}),
            true,
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }
}

impl Store {
    /// Read-only status includes a different goal's lease when it blocks this project.
    pub async fn verification_status(&self, loop_id: &str) -> Result<Value> {
        let goal = self.get_loop(loop_id).await?;
        let row = sqlx::query("SELECT id,loop_id,cycle_id,contract_revision,plan_version,state,created_at FROM verification_leases WHERE project_id=? AND state IN ('running','outcome_unknown')")
            .bind(&goal.project_id).fetch_optional(&self.pool).await?;
        let active = row.map(|r| json!({"lease_id":r.get::<String,_>("id"),"loop_id":r.get::<String,_>("loop_id"),"cycle_id":r.get::<String,_>("cycle_id"),"contract_revision":r.get::<i64,_>("contract_revision"),"plan_version":r.get::<i64,_>("plan_version"),"state":r.get::<String,_>("state"),"created_at":r.get::<i64,_>("created_at")}));
        Ok(
            json!({"project_id":goal.project_id,"active_verification":active,"external_environment":self.environment_status(loop_id).await?}),
        )
    }

    /// Internal operator reconciliation context; never expose command plans as grants.
    pub async fn verification_recovery_context(&self, lease: &str) -> Result<Value> {
        let r = sqlx::query("SELECT v.*,p.body AS plan,(SELECT COUNT(*) FROM verification_test_attempts a WHERE a.lease_id=v.id) AS test_attempt FROM verification_leases v LEFT JOIN verification_plans p ON p.loop_id=v.loop_id AND p.contract_revision=v.contract_revision AND p.version=v.plan_version WHERE v.id=?")
            .bind(lease).fetch_optional(&self.pool).await?.context("verification lease missing")?;
        ensure!(
            r.get::<String, _>("state") == "outcome_unknown",
            "only unknown verification can be reconciled"
        );
        let plan = r
            .get::<Option<String>, _>("plan")
            .map(|p| serde_json::from_str::<Value>(&p))
            .transpose()?;
        Ok(
            json!({"lease_id":lease,"loop_id":r.get::<String,_>("loop_id"),"cycle_id":r.get::<String,_>("cycle_id"),"contract_revision":r.get::<i64,_>("contract_revision"),"plan_version":r.get::<i64,_>("plan_version"),"test_attempt":r.get::<i64,_>("test_attempt") == 1,"plan":plan}),
        )
    }

    /// Trusted runtime path, after independent cleanup inspection. Discards the
    /// uncertain observation; never accepts results, resets budgets or enqueues work.
    pub async fn resolve_verification_unknown(&self, lease: &str, proof: &Value) -> Result<Value> {
        ensure!(
            serde_json::to_vec(proof)?.len() <= 65536,
            "reconciliation proof too large"
        );
        let mut tx = self.pool.begin().await?;
        let owner: String = sqlx::query_scalar(
            "SELECT loop_id FROM verification_leases WHERE id=? AND state='outcome_unknown'",
        )
        .bind(lease)
        .fetch_optional(&mut *tx)
        .await?
        .context("verification no longer unknown")?;
        sqlx::query("UPDATE verification_leases SET state='finished',finished_at=? WHERE id=? AND state='outcome_unknown'")
            .bind(now()).bind(lease).execute(&mut *tx).await?;
        Self::event(&mut tx, Some(&owner), "verification_reconciled", json!({"lease_id":lease,"disposition":"discarded","proof":proof,"automatic_retry":false}), true).await?;
        tx.commit().await?;
        Ok(
            json!({"lease_id":lease,"state":"finished","disposition":"discarded","automatic_retry":false}),
        )
    }
}
