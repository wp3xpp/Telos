use super::*;
use telos_core::ProjectExecutionPolicy;

impl Store {
    pub async fn project_policy(&self, project_id: &str) -> Result<Option<Value>> {
        let row =
            sqlx::query("SELECT revision,body FROM project_execution_policies WHERE project_id=?")
                .bind(project_id)
                .fetch_optional(&self.pool)
                .await?;
        row.map(|row| Ok(json!({"revision":row.get::<i64,_>("revision"),"policy":serde_json::from_str::<Value>(&row.get::<String,_>("body"))?}))).transpose()
    }

    /// Trusted operator API. Never expose authority mutation through model tools.
    pub async fn configure_project(
        &self,
        project_id: &str,
        expected_revision: i64,
        policy: &ProjectExecutionPolicy,
    ) -> Result<Value> {
        policy.validate()?;
        let mut tx = self.pool.begin().await?;
        Self::require_verification_idle(&mut tx, project_id).await?;
        let exists: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM projects WHERE id=?")
            .bind(project_id)
            .fetch_one(&mut *tx)
            .await?;
        ensure!(exists == 1, "project not registered");
        let revision: Option<i64> = sqlx::query_scalar(
            "SELECT revision FROM project_execution_policies WHERE project_id=?",
        )
        .bind(project_id)
        .fetch_optional(&mut *tx)
        .await?;
        ensure!(
            revision.unwrap_or(0) == expected_revision,
            "project policy revision conflict"
        );
        // Running effects must be reconciled before changing their authority envelope.
        let inflight: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM step_runs s JOIN work_items w ON w.id=s.work_item_id JOIN cycles c ON c.id=w.cycle_id JOIN loops l ON l.id=c.loop_id WHERE l.project_id=? AND s.project_policy_revision IS NOT NULL AND s.state IN ('running','outcome_unknown')")
            .bind(project_id).fetch_one(&mut *tx).await?;
        ensure!(
            inflight == 0,
            "pause and reconcile managed execution before changing project policy"
        );
        let revision = expected_revision
            .checked_add(1)
            .context("policy revision overflow")?;
        sqlx::query("INSERT INTO project_execution_policies(project_id,revision,body,updated_at) VALUES(?,?,?,?) ON CONFLICT(project_id) DO UPDATE SET revision=excluded.revision,body=excluded.body,updated_at=excluded.updated_at")
            .bind(project_id).bind(revision).bind(serde_json::to_string(policy)?).bind(now()).execute(&mut *tx).await?;
        // Pending work tied to the previous grant must not execute under a new one.
        sqlx::query("UPDATE loop_schedules SET enabled=0,suspended_reason='project_policy_changed',updated_at=? WHERE loop_id IN (SELECT id FROM loops WHERE project_id=?)")
            .bind(now()).bind(project_id).execute(&mut *tx).await?;
        let stale = sqlx::query("SELECT s.id,s.work_item_id,c.loop_id FROM step_runs s JOIN work_items w ON w.id=s.work_item_id JOIN cycles c ON c.id=w.cycle_id JOIN loops l ON l.id=c.loop_id WHERE l.project_id=? AND s.project_policy_revision IS NOT NULL AND s.state='ready'")
            .bind(project_id).fetch_all(&mut *tx).await?;
        for row in stale {
            let step: String = row.get("id");
            let loop_id: String = row.get("loop_id");
            sqlx::query("UPDATE step_runs SET state='cancelled',finished_at=? WHERE id=?")
                .bind(now())
                .bind(&step)
                .execute(&mut *tx)
                .await?;
            sqlx::query("UPDATE work_items SET state='cancelled' WHERE id=?")
                .bind(row.get::<String, _>("work_item_id"))
                .execute(&mut *tx)
                .await?;
            sqlx::query("UPDATE loops SET state='blocked',version=version+1,updated_at=? WHERE id=? AND state='active'").bind(now()).bind(&loop_id).execute(&mut *tx).await?;
            Self::event(
                &mut tx,
                Some(&loop_id),
                "step_cancelled_policy_changed",
                json!({"step_id":step,"policy_revision":revision}),
                true,
            )
            .await?;
        }
        Self::event(
            &mut tx,
            None,
            "project_execution_configured",
            json!({"project_id":project_id,"revision":revision,"policy":policy}),
            false,
        )
        .await?;
        tx.commit().await?;
        Ok(json!({"revision":revision,"policy":policy}))
    }
}
