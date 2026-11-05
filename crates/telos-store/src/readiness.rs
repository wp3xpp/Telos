use super::*;
use std::collections::BTreeSet;
use telos_core::VerificationPlan;

impl Store {
    /// Read one ledger snapshot without opening a cycle or reading project files.
    pub async fn readiness(&self, loop_id: &str) -> Result<Value> {
        let mut tx = self.pool.begin().await?;
        let goal = Self::get_in(&mut tx, loop_id).await?;
        let policy = sqlx::query("SELECT revision, json_extract(body,'$.enabled') AS enabled FROM project_execution_policies WHERE project_id=?")
            .bind(&goal.project_id).fetch_optional(&mut *tx).await?;
        let policy_revision = policy.as_ref().map(|row| row.get::<i64, _>("revision"));
        let policy_enabled = policy
            .as_ref()
            .is_some_and(|row| row.get::<bool, _>("enabled"));
        let plan = sqlx::query(
            "SELECT version,body FROM verification_plans WHERE loop_id=? AND contract_revision=?",
        )
        .bind(loop_id)
        .bind(goal.revision)
        .fetch_optional(&mut *tx)
        .await?;
        let plan_version = plan.as_ref().map(|row| row.get::<i64, _>("version"));
        let plan: Option<VerificationPlan> = plan
            .as_ref()
            .map(|row| serde_json::from_str(row.get::<&str, _>("body")))
            .transpose()?;
        let sources: BTreeSet<&str> = plan
            .iter()
            .flat_map(|plan| &plan.test_checks)
            .filter_map(|check| check.observation.as_ref().map(|p| p.source_id.as_str()))
            .collect();

        let rows = sqlx::query("SELECT sequence,created_at,body FROM events WHERE sequence IN (
            SELECT MAX(sequence) FROM events WHERE loop_id=? AND kind='project_observation_collected'
            AND json_extract(body,'$.revision')=? AND (? OR json_extract(body,'$.batch.source_id') IN (SELECT value FROM json_each(?)))
            GROUP BY json_extract(body,'$.batch.source_id')) ORDER BY sequence DESC LIMIT ?")
            .bind(loop_id).bind(goal.revision).bind(sources.is_empty()).bind(serde_json::to_string(&sources)?)
            .bind(if sources.is_empty() { 16 } else { sources.len() as i64 }).fetch_all(&mut *tx).await?;
        let mut receipts = Vec::new();
        for row in rows {
            let receipt: Value = serde_json::from_str(row.get("body"))?;
            receipts.push(json!({"source_id":receipt["batch"]["source_id"],"cursor_version":receipt["version"],
                "recorded_at":row.get::<i64,_>("created_at"),"event_sequence":row.get::<i64,_>("sequence"),
                "backlog_bytes":receipt["backlog_bytes"],"gap_reason":receipt["batch"]["gap_reason"]}));
        }
        let missing_sources: Vec<_> = sources
            .into_iter()
            .filter(|source| {
                !receipts
                    .iter()
                    .any(|receipt| receipt["source_id"].as_str() == Some(*source))
            })
            .collect();
        let collection_status = if receipts.is_empty() {
            "no_evidence"
        } else if missing_sources.is_empty() {
            "evidence_recorded"
        } else {
            "partial_evidence"
        };

        let schedule = sqlx::query("SELECT enabled,contract_revision,policy_revision,suspended_reason FROM loop_schedules WHERE loop_id=?")
            .bind(loop_id).fetch_optional(&mut *tx).await?;
        let armed = goal.kind == LoopKind::Continuous
            && goal.state == LoopState::Active
            && policy_enabled
            && schedule.as_ref().is_some_and(|row| {
                row.get::<bool, _>("enabled")
                    && row.get::<i64, _>("contract_revision") == goal.revision
                    && Some(row.get::<i64, _>("policy_revision")) == policy_revision
            });
        let cycle_open: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM cycles WHERE loop_id=? AND state NOT IN ('closed','cancelled'))")
            .bind(loop_id).fetch_one(&mut *tx).await?;
        let next_due_at = if armed && !cycle_open {
            goal.next_due_at
        } else {
            None
        };
        let first = sqlx::query("SELECT v.id,v.verdict,v.created_at FROM verifications v JOIN cycles c ON c.id=v.cycle_id WHERE c.loop_id=? AND v.contract_revision=? AND v.plan_version=? ORDER BY v.rowid LIMIT 1")
            .bind(loop_id).bind(goal.revision).bind(plan_version).fetch_optional(&mut *tx).await?;
        let first = first.map(|row| json!({"id":row.get::<String,_>("id"),"verdict":row.get::<String,_>("verdict"),"recorded_at":row.get::<i64,_>("created_at")}));
        tx.commit().await?;

        let mut missing = Vec::new();
        if policy_revision.is_none() {
            missing.push("execution_policy_missing");
        } else if !policy_enabled {
            missing.push("execution_policy_disabled");
        }
        if plan_version.is_none() {
            missing.push("verification_plan_missing");
        }
        Ok(
            json!({"loop_id":goal.id,"project_id":goal.project_id,"revision":goal.revision,"kind":goal.kind,"loop_state":goal.state,
            "configuration":{"policy_revision":policy_revision,"policy_enabled":policy_enabled,"plan_version":plan_version,"missing":missing},
            "collection":{"status":collection_status,"receipts":receipts,"missing_sources":missing_sources},
            "inspection":{"status":if armed { "armed" } else { "not_scheduled" },"armed":armed,"next_due_at":next_due_at,"cycle_open":cycle_open,
                "suspended_reason":schedule.and_then(|row| row.get::<Option<String>,_>("suspended_reason"))},
            "first_verification":first}),
        )
    }
}
