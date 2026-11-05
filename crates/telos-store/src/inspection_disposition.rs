use super::*;

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InspectionDisposition {
    pub loop_id: String,
    pub revision: i64,
    pub expected_version: i64,
    pub cycle_id: String,
    pub step_id: String,
    pub generation: i64,
    pub evidence_id: String,
    pub evidence_sha256: String,
    pub decision_id: String,
    pub external_effects_reconciled: bool,
}

impl Store {
    pub fn inspection_run_dir(&self, review: &InspectionDisposition) -> Result<PathBuf> {
        let suffix = review
            .step_id
            .strip_prefix("step-")
            .context("invalid step ID")?;
        Uuid::parse_str(suffix).context("invalid step ID")?;
        ensure!(review.generation > 0, "invalid step generation");
        Ok(self
            .root
            .join("runs")
            .join(format!("{}-{}", review.step_id, review.generation)))
    }

    /// Retire a confirmed read-only inspection without releasing other ownership.
    pub async fn discard_inspection(
        &self,
        review: &InspectionDisposition,
        journal_sha256: &str,
    ) -> Result<Value> {
        ensure!(
            review.external_effects_reconciled,
            "operator effect reconciliation required"
        );
        ensure!(
            !review.decision_id.trim().is_empty()
                && review.decision_id.len() <= 128
                && !review.decision_id.chars().any(char::is_control),
            "invalid decision ID"
        );
        let run_dir = self.inspection_run_dir(review)?;
        let proof = serde_json::to_value(review)?;
        let evidence = self.get_evidence(&review.evidence_id, 0, 32768).await?;
        ensure!(
            evidence["sha256"] == review.evidence_sha256
                && evidence["loop_id"] == review.loop_id
                && evidence["contract_revision"] == review.revision
                && evidence["cycle_id"] == review.cycle_id
                && evidence["artifact"] == review.step_id
                && evidence["environment"] == "local-codex"
                && evidence["next_offset"].is_null(),
            "execution evidence binding mismatch"
        );
        let report: Value = serde_json::from_str(
            evidence["text"]
                .as_str()
                .context("execution evidence must be JSON")?,
        )?;
        ensure!(
            report["outcome"] == "outcome_unknown"
                && report["cleanup_confirmed"] == true
                && report["exit_code"] == 0
                && report["reason"]
                    .as_str()
                    .is_some_and(|reason| reason.starts_with("output limit exceeded"))
                && report["run_dir"]
                    .as_str()
                    .is_some_and(|path| Path::new(path) == run_dir),
            "expected a cleaned inspection with an unknown result after output truncation"
        );

        let mut tx = self.pool.begin().await?;
        let existing: Option<String> = sqlx::query_scalar(
            "SELECT body FROM events WHERE loop_id=? AND kind='inspection_discarded' AND json_extract(body,'$.proof.decision_id')=?"
        ).bind(&review.loop_id).bind(&review.decision_id).fetch_optional(&mut *tx).await?;
        if let Some(body) = existing {
            let receipt: Value = serde_json::from_str(&body)?;
            ensure!(
                receipt["proof"] == proof,
                "decision ID reused with different inputs"
            );
            return Ok(receipt);
        }
        let goal = Self::get_in(&mut tx, &review.loop_id).await?;
        ensure!(
            goal.kind == LoopKind::Continuous
                && goal.state == LoopState::Blocked
                && goal.revision == review.revision
                && goal.version == review.expected_version,
            "current blocked continuous goal and matching versions required"
        );
        Self::require_verification_idle(&mut tx, &goal.project_id).await?;
        Self::require_environment_settled(&mut tx, &goal.project_id).await?;
        let row = sqlx::query(
            "SELECT s.work_item_id,s.spec,s.result FROM step_runs s
             JOIN work_items w ON w.id=s.work_item_id JOIN cycles c ON c.id=w.cycle_id
             WHERE s.id=? AND s.lease_generation=? AND s.state='outcome_unknown'
             AND s.finished_at IS NOT NULL AND s.lease_until IS NULL
             AND s.project_policy_revision IS NOT NULL AND s.workspace_id IS NULL
             AND s.contract_revision=? AND c.id=? AND c.loop_id=? AND c.contract_revision=? AND c.state='blocked'"
        ).bind(&review.step_id).bind(review.generation).bind(review.revision)
            .bind(&review.cycle_id).bind(&review.loop_id).bind(review.revision)
            .fetch_optional(&mut *tx).await?.context("current unknown inspection not found")?;
        let spec: telos_core::ExecutionSpec = serde_json::from_str(row.get("spec"))?;
        let result: Value = serde_json::from_str(row.get("result"))?;
        ensure!(
            spec.sandbox == telos_core::Sandbox::ReadOnly && result == report,
            "ledger inspection and verified execution evidence do not match"
        );
        sqlx::query("UPDATE step_runs SET state='cancelled' WHERE id=?")
            .bind(&review.step_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "UPDATE work_items SET state='cancelled' WHERE id=? AND NOT EXISTS (
             SELECT 1 FROM step_runs WHERE work_item_id=work_items.id AND state IN ('pending','ready','running','outcome_unknown'))"
        ).bind(row.get::<String,_>("work_item_id")).execute(&mut *tx).await?;
        let receipt = json!({
            "proof": proof,
            "process_journal_sha256": journal_sha256,
            "disposition": "discarded",
            "automatic_retry": false,
            "loop_state": "blocked"
        });
        Self::event(
            &mut tx,
            Some(&review.loop_id),
            "inspection_discarded",
            receipt.clone(),
            false,
        )
        .await?;
        tx.commit().await?;
        Ok(receipt)
    }
}
