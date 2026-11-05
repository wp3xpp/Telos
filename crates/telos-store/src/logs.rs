use super::*;

impl Store {
    pub async fn log_cursor(
        &self,
        cycle_id: &str,
        criterion_id: &str,
        plan_version: i64,
    ) -> Result<Option<Value>> {
        let row=sqlx::query("SELECT version,body FROM log_cursors WHERE cycle_id=? AND criterion_id=? AND plan_version=?")
            .bind(cycle_id).bind(criterion_id).bind(plan_version).fetch_optional(&self.pool).await?;
        row.map(|r|Ok(json!({"version":r.get::<i64,_>("version"),"body":serde_json::from_str::<Value>(&r.get::<String,_>("body"))?}))).transpose()
    }

    pub async fn save_log_cursor(
        &self,
        cycle_id: &str,
        criterion_id: &str,
        plan_version: i64,
        expected_version: i64,
        body: &Value,
    ) -> Result<()> {
        let serialized = serde_json::to_string(body)?;
        ensure!(serialized.len() <= 65536, "log cursor exceeds size limit");
        let mut tx = self.pool.begin().await?;
        let row=sqlx::query("SELECT c.loop_id,c.contract_revision,c.state,l.revision,l.state AS loop_state,p.version AS plan_version FROM cycles c JOIN loops l ON l.id=c.loop_id JOIN verification_plans p ON p.loop_id=l.id AND p.contract_revision=c.contract_revision WHERE c.id=?")
            .bind(cycle_id).fetch_one(&mut *tx).await?;
        ensure!(
            row.get::<i64, _>("contract_revision") == row.get::<i64, _>("revision")
                && row.get::<i64, _>("plan_version") == plan_version,
            "stale log observation"
        );
        ensure!(
            row.get::<String, _>("loop_state") == "active"
                && !matches!(row.get::<&str, _>("state"), "closed" | "cancelled"),
            "inactive log observation"
        );
        let old:Option<i64>=sqlx::query_scalar("SELECT version FROM log_cursors WHERE cycle_id=? AND criterion_id=? AND plan_version=?")
            .bind(cycle_id).bind(criterion_id).bind(plan_version).fetch_optional(&mut *tx).await?;
        ensure!(
            old.unwrap_or(0) == expected_version,
            "log cursor version conflict"
        );
        sqlx::query("INSERT INTO log_cursors(cycle_id,criterion_id,plan_version,version,body,updated_at) VALUES(?,?,?,?,?,?) ON CONFLICT(cycle_id,criterion_id,plan_version) DO UPDATE SET version=excluded.version,body=excluded.body,updated_at=excluded.updated_at")
            .bind(cycle_id).bind(criterion_id).bind(plan_version).bind(expected_version+1).bind(&serialized).bind(now()).execute(&mut *tx).await?;
        Self::event(&mut tx,Some(row.get::<&str,_>("loop_id")),"log_observation_recorded",json!({"cycle_id":cycle_id,"criterion_id":criterion_id,"plan_version":plan_version,"cursor_version":expected_version+1,"observation":body}),false).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn pending_log_observations(&self, at: i64) -> Result<Vec<Value>> {
        let rows=sqlx::query("SELECT l.id,l.revision FROM cycles c JOIN loops l ON l.id=c.loop_id JOIN verification_plans p ON p.loop_id=l.id AND p.contract_revision=l.revision WHERE l.state='active' AND c.contract_revision=l.revision AND c.state IN ('observing','verifying','acting') AND json_array_length(p.body,'$.log_checks')>0 AND NOT EXISTS (SELECT 1 FROM step_runs s JOIN work_items w ON w.id=s.work_item_id WHERE w.cycle_id=c.id AND s.state IN ('ready','running','outcome_unknown')) AND COALESCE((SELECT MAX(updated_at) FROM log_cursors lc WHERE lc.cycle_id=c.id AND lc.plan_version=p.version),0)<=? ORDER BY l.id LIMIT 16")
            .bind(at-5).fetch_all(&self.pool).await?;
        Ok(rows
            .into_iter()
            .map(|r| json!({"id":r.get::<String,_>("id"),"revision":r.get::<i64,_>("revision")}))
            .collect())
    }
}
