//! Explicit diagnosis-to-delivery relationships, never execution or recovery grants.
use super::*;
impl Store {
    pub async fn associate_remediation(
        &self,
        parent: &str,
        revision: i64,
        step: &str,
        child: &str,
        child_revision: i64,
        key: &str,
    ) -> Result<Value> {
        ensure!(
            !key.is_empty() && key.len() <= 128 && !key.chars().any(char::is_control),
            "invalid remediation request key"
        );
        let mut tx = self.pool.begin().await?;
        let origin = Self::get_in(&mut tx, parent).await?;
        let target = Self::get_in(&mut tx, child).await?;
        ensure!(
            origin.revision == revision
                && origin.kind == LoopKind::Continuous
                && matches!(
                    origin.state,
                    LoopState::Active | LoopState::Paused | LoopState::Blocked
                ),
            "remediation origin must be current continuous goal"
        );
        Self::require_recovery_clear(&mut tx, &origin.project_id).await?;
        let existing:Option<String>=sqlx::query_scalar("SELECT body FROM events WHERE loop_id=? AND kind='remediation_associated' AND json_extract(body,'$.revision')=? AND json_extract(body,'$.request_key')=? ORDER BY sequence DESC LIMIT 1").bind(parent).bind(revision).bind(key).fetch_optional(&mut *tx).await?;
        if let Some(existing) = existing {
            let old: Value = serde_json::from_str(&existing)?;
            ensure!(
                old["diagnosis_step_id"] == step
                    && old["delivery_loop_id"] == child
                    && old["delivery_revision"].as_i64() == Some(child_revision),
                "remediation request key reused for another association"
            );
            tx.commit().await?;
            return Ok(old);
        }
        Self::require_verification_idle(&mut tx, &origin.project_id).await?;
        ensure!(
            target.revision == child_revision
                && target.kind == LoopKind::Delivery
                && target.project_id == origin.project_id,
            "remediation must reference current delivery goal in same project"
        );
        let diagnosis=sqlx::query("SELECT s.result,w.cycle_id FROM step_runs s JOIN work_items w ON w.id=s.work_item_id JOIN cycles c ON c.id=w.cycle_id JOIN enqueue_keys k ON k.step_id=s.id AND k.request_key='diagnosis:'||c.id WHERE s.id=? AND c.loop_id=? AND c.contract_revision=? AND s.contract_revision=? AND s.state='succeeded'").bind(step).bind(parent).bind(revision).bind(revision).fetch_optional(&mut *tx).await?.context("successful current-revision diagnosis required")?;
        let result: String = diagnosis
            .get::<Option<String>, _>("result")
            .context("diagnosis result missing")?;
        let body = json!({"revision":revision,"request_key":key,"diagnosis_step_id":step,"diagnosis_cycle_id":diagnosis.get::<String,_>("cycle_id"),"diagnosis_result_sha256":format!("{:x}",Sha256::digest(result.as_bytes())),"delivery_loop_id":child,"delivery_revision":child_revision,"parent_log_windows_reset":true,"scope":"Relationship only; no execution, approval, deployment or verified business recovery"});
        // Existing healthy-window accumulation predates the association. Retain
        // its audit history, but require fresh accumulation for the open parent cycle.
        sqlx::query("DELETE FROM log_cursors WHERE cycle_id IN (SELECT id FROM cycles WHERE loop_id=? AND contract_revision=? AND state NOT IN ('closed','cancelled'))").bind(parent).bind(revision).execute(&mut *tx).await?;
        Self::event(
            &mut tx,
            Some(parent),
            "remediation_associated",
            body.clone(),
            false,
        )
        .await?;
        tx.commit().await?;
        Ok(body)
    }
    pub async fn remediation_links(&self, parent: &str, revision: i64) -> Result<Value> {
        let mut tx = self.pool.begin().await?;
        let record = Self::get_in(&mut tx, parent).await?;
        ensure!(record.revision == revision, "stale remediation context");
        let rows=sqlx::query("SELECT e.sequence,e.body,l.state,l.revision AS child_revision FROM events e JOIN loops l ON l.id=json_extract(e.body,'$.delivery_loop_id') WHERE e.loop_id=? AND e.kind='remediation_associated' AND json_extract(e.body,'$.revision')=? ORDER BY e.sequence DESC LIMIT 17").bind(parent).bind(revision).fetch_all(&mut *tx).await?;
        let more = rows.len() > 16;
        let plan: Option<i64> = sqlx::query_scalar(
            "SELECT version FROM verification_plans WHERE loop_id=? AND contract_revision=?",
        )
        .bind(parent)
        .bind(revision)
        .fetch_optional(&mut *tx)
        .await?;
        let mut items = Vec::new();
        for r in rows.into_iter().take(16) {
            let link: Value = serde_json::from_str(r.get::<&str, _>("body"))?;
            let sequence = r.get::<i64, _>("sequence");
            let current =
                link["delivery_revision"].as_i64() == Some(r.get::<i64, _>("child_revision"));
            let check:Option<String>=sqlx::query_scalar("SELECT body FROM events WHERE loop_id=? AND kind='remediation_recovery_checked' AND json_extract(body,'$.association_sequence')=? AND json_extract(body,'$.revision')=? ORDER BY sequence DESC LIMIT 1").bind(parent).bind(sequence).bind(revision).fetch_optional(&mut *tx).await?;
            let check = check
                .map(|v| serde_json::from_str::<Value>(&v))
                .transpose()?;
            let check_current = current
                && check
                    .as_ref()
                    .is_some_and(|v| v["plan_version"].as_i64() == plan && plan.is_some());
            items.push(json!({"event_sequence":sequence,"diagnosis_step_id":link["diagnosis_step_id"],"diagnosis_result_sha256":link["diagnosis_result_sha256"],"delivery_loop_id":link["delivery_loop_id"],"delivery_revision":link["delivery_revision"],"delivery_current_revision":r.get::<i64,_>("child_revision"),"delivery_state":r.get::<String,_>("state"),"association_current":current,"business_recovery":"unverified","recovery_check":check,"recovery_check_current":check_current}));
        }
        tx.commit().await?;
        Ok(
            json!({"items":items,"more":more,"limit":16,"scope":"Delivery progress and explicit association only. A completed delivery goal does not prove recovery of the monitored service; the origin requires its own current independent verification."}),
        )
    }
}
