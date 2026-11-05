use super::*;
use telos_core::VerificationPlan;

impl Store {
    pub async fn record_verification_deferred(
        &self,
        loop_id: &str,
        cycle_id: &str,
        reason: &str,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        Self::event(
            &mut tx,
            Some(loop_id),
            "verification_deferred",
            json!({"cycle_id":cycle_id,"reason":reason}),
            true,
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }
    pub async fn configure_verification(
        &self,
        loop_id: &str,
        revision: i64,
        expected_version: i64,
        plan: &VerificationPlan,
    ) -> Result<Value> {
        let mut tx = self.pool.begin().await?;
        let record = Self::get_in(&mut tx, loop_id).await?;
        ensure!(
            record.revision == revision
                && !matches!(record.state, LoopState::Completed | LoopState::Stopped),
            "stale or terminal goal"
        );
        plan.validate(&record.contract)?;
        ensure!(
            plan.test_checks.is_empty()
                || record.kind == LoopKind::Delivery
                || plan
                    .test_checks
                    .iter()
                    .all(|c| c.adapter == telos_core::observation::ADAPTER),
            "continuous goals require read-only observation adapters"
        );
        Self::require_verification_idle(&mut tx, &record.project_id).await?;
        let old: Option<i64> = sqlx::query_scalar(
            "SELECT version FROM verification_plans WHERE loop_id=? AND contract_revision=?",
        )
        .bind(loop_id)
        .bind(revision)
        .fetch_optional(&mut *tx)
        .await?;
        ensure!(
            old.unwrap_or(0) == expected_version,
            "verification plan version conflict"
        );
        let version = expected_version
            .checked_add(1)
            .context("plan version overflow")?;
        sqlx::query("INSERT INTO verification_plans(loop_id,contract_revision,version,body,updated_at) VALUES(?,?,?,?,?) ON CONFLICT(loop_id,contract_revision) DO UPDATE SET version=excluded.version,body=excluded.body,updated_at=excluded.updated_at")
            .bind(loop_id).bind(revision).bind(version).bind(serde_json::to_string(plan)?).bind(now()).execute(&mut *tx).await?;
        Self::event(
            &mut tx,
            Some(loop_id),
            "verification_plan_configured",
            json!({"revision":revision,"version":version,"plan":plan}),
            false,
        )
        .await?;
        tx.commit().await?;
        Ok(json!({"version":version,"plan":plan}))
    }

    pub async fn verification_plan(&self, loop_id: &str, revision: i64) -> Result<Option<Value>> {
        let row = sqlx::query(
            "SELECT version,body FROM verification_plans WHERE loop_id=? AND contract_revision=?",
        )
        .bind(loop_id)
        .bind(revision)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row|Ok(json!({"version":row.get::<i64,_>("version"),"plan":serde_json::from_str::<Value>(&row.get::<String,_>("body"))?}))).transpose()
    }

    pub async fn active_cycle(&self, loop_id: &str, revision: i64) -> Result<Option<String>> {
        Ok(sqlx::query_scalar("SELECT id FROM cycles WHERE loop_id=? AND contract_revision=? AND state NOT IN ('closed','cancelled')")
            .bind(loop_id).bind(revision).fetch_optional(&self.pool).await?)
    }
}
