//! Project-owned release/action evidence, with fresh independent observation gates.
use super::*;
use telos_core::observation::{StageOutcome, StageReceipt};
impl Store {
    pub async fn record_project_stage(
        &self,
        child: &str,
        revision: i64,
        receipt: &StageReceipt,
    ) -> Result<Value> {
        receipt.validate()?;
        let mut tx = self.pool.begin().await?;
        let goal = Self::get_in(&mut tx, child).await?;
        ensure!(
            goal.kind == LoopKind::Delivery
                && goal.revision == revision
                && goal.state == LoopState::Completed,
            "project action evidence requires an accepted current delivery"
        );
        let row=sqlx::query("SELECT v.body,v.verdict FROM verifications v JOIN cycles c ON c.id=v.cycle_id WHERE c.loop_id=? AND v.contract_revision=? ORDER BY v.rowid DESC LIMIT 1")
            .bind(child).bind(revision).fetch_one(&mut *tx).await?;
        let accepted: Value = serde_json::from_str(row.get("body"))?;
        ensure!(
            row.get::<String, _>("verdict") == "passed" && accepted["artifact"] == receipt.artifact,
            "project stage refers to an unaccepted or different artifact"
        );
        let existing:Option<String>=sqlx::query_scalar("SELECT body FROM events WHERE loop_id=? AND kind='project_stage_recorded' AND json_extract(body,'$.revision')=? AND json_extract(body,'$.receipt.request_key')=? ORDER BY sequence DESC LIMIT 1")
            .bind(child).bind(revision).bind(&receipt.request_key).fetch_optional(&mut *tx).await?;
        if let Some(body) = existing {
            let body: Value = serde_json::from_str(&body)?;
            ensure!(
                body["receipt"] == serde_json::to_value(receipt)?,
                "stage request key reused for a different outcome"
            );
            return Ok(body);
        }
        let body = json!({"revision":revision,"recorded_at":now(),"receipt":receipt,"recovery":"unverified",
            "note":"Project operator attestation only; no action executed or permission granted. Parent requires independent post-stage observations."});
        Self::event(
            &mut tx,
            Some(child),
            "project_stage_recorded",
            body.clone(),
            false,
        )
        .await?;
        tx.commit().await?;
        Ok(body)
    }
    pub async fn observation_stage_gate(
        &self,
        parent: &str,
        revision: i64,
        stage: &str,
    ) -> Result<Value> {
        let record = self.get_loop(parent).await?;
        ensure!(record.revision == revision, "stale stage gate");
        let link=sqlx::query("SELECT e.sequence,e.body,l.revision AS child_revision,l.state FROM events e JOIN loops l ON l.id=json_extract(e.body,'$.delivery_loop_id') WHERE e.loop_id=? AND e.kind='remediation_associated' AND json_extract(e.body,'$.revision')=? ORDER BY e.sequence DESC LIMIT 1")
            .bind(parent).bind(revision).fetch_optional(&self.pool).await?;
        let Some(link) = link else {
            return Ok(json!({"required":false}));
        };
        let body: Value = serde_json::from_str(link.get("body"))?;
        let waiting = json!({"required":true,"satisfied":false,"stage":stage,"association_sequence":link.get::<i64,_>("sequence")});
        if link.get::<String, _>("state") != "completed"
            || body["delivery_revision"].as_i64() != Some(link.get("child_revision"))
        {
            return Ok(waiting);
        }
        let child = body["delivery_loop_id"]
            .as_str()
            .context("association child missing")?;
        let stage_body:Option<String>=sqlx::query_scalar("SELECT body FROM events WHERE loop_id=? AND kind='project_stage_recorded' AND json_extract(body,'$.revision')=? AND json_extract(body,'$.receipt.stage')=? AND sequence>? ORDER BY sequence DESC LIMIT 1")
            .bind(child).bind(link.get::<i64,_>("child_revision")).bind(stage).bind(link.get::<i64,_>("sequence")).fetch_optional(&self.pool).await?;
        let Some(stage_body) = stage_body else {
            return Ok(waiting);
        };
        let stage_body: Value = serde_json::from_str(&stage_body)?;
        let receipt: StageReceipt = serde_json::from_value(stage_body["receipt"].clone())?;
        if receipt.outcome != StageOutcome::Succeeded {
            return Ok(waiting);
        }
        Ok(
            json!({"required":true,"satisfied":true,"stage":stage,"not_before":stage_body["recorded_at"],
            "subject_id":receipt.subject_id,"artifact":receipt.artifact,"association_sequence":link.get::<i64,_>("sequence")}),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use telos_core::{Contract, Criterion, CriterionResult};
    #[tokio::test]
    async fn accepted_delivery_and_stage_attestation_are_not_parent_recovery() -> Result<()> {
        let root = std::env::temp_dir().join(format!("telos-stage-{}", Uuid::new_v4()));
        let s = Store::open(&root).await?;
        s.register_project("fixture", &root).await?;
        let mut contract = Contract {
            goal: "fixture".into(),
            delivery_scope: "fixture".into(),
            criteria: vec![Criterion {
                id: "value".into(),
                description: "fixture".into(),
            }],
            interval_seconds: Some(30),
            max_attempts: 2,
        };
        let parent = s
            .create_loop("fixture", "parent", LoopKind::Continuous, &contract)
            .await?;
        contract.interval_seconds = None;
        let child = s
            .create_loop("fixture", "child", LoopKind::Delivery, &contract)
            .await?;
        let mut receipt = StageReceipt {
            request_key: "stage-1".into(),
            stage: "activate".into(),
            artifact: "fixture-artifact".into(),
            subject_id: "fixture-subject".into(),
            outcome: StageOutcome::OutcomeUnknown,
            evidence_refs: vec!["fixture-driver-output".into()],
        };
        assert!(s
            .record_project_stage(&child.id, 1, &receipt)
            .await
            .is_err());
        let cycle = s.start_cycle(&child.id, 1, "fixture").await?;
        let evidence = s
            .add_evidence(
                &cycle,
                1,
                "fixture-artifact",
                "fixture",
                b"synthetic unit-test observation",
            )
            .await?;
        s.verify(&Verification {
            cycle_id: cycle,
            contract_revision: 1,
            artifact: "fixture-artifact".into(),
            environment: "fixture".into(),
            observation_complete: true,
            results: vec![CriterionResult {
                criterion_id: "value".into(),
                verdict: Verdict::Passed,
                evidence_ids: vec![evidence],
                explanation: "fixture".into(),
            }],
        })
        .await?;
        let mut tx = s.pool.begin().await?;
        Store::event(
            &mut tx,
            Some(&parent.id),
            "remediation_associated",
            json!({"revision":1,"delivery_loop_id":child.id,"delivery_revision":1}),
            false,
        )
        .await?;
        tx.commit().await?;
        assert_eq!(
            s.observation_stage_gate(&parent.id, 1, "activate").await?["satisfied"],
            false
        );
        s.record_project_stage(&child.id, 1, &receipt).await?;
        assert_eq!(
            s.observation_stage_gate(&parent.id, 1, "activate").await?["satisfied"],
            false
        );
        receipt.outcome = StageOutcome::Succeeded;
        assert!(s
            .record_project_stage(&child.id, 1, &receipt)
            .await
            .is_err());
        receipt.request_key = "independently-reconciled-stage".into();
        let recorded = s.record_project_stage(&child.id, 1, &receipt).await?;
        let gate = s.observation_stage_gate(&parent.id, 1, "activate").await?;
        assert_eq!(gate["satisfied"], true);
        assert_eq!(gate["not_before"], recorded["recorded_at"]);
        assert_eq!(gate["subject_id"], "fixture-subject");
        assert_eq!(s.get_loop(&parent.id).await?.state, LoopState::Active);
        assert!(s.task_context(&parent.id, 1).await?["latest_verification"].is_null());
        receipt.request_key = "wrong-artifact".into();
        receipt.artifact = "other".into();
        assert!(s
            .record_project_stage(&child.id, 1, &receipt)
            .await
            .is_err());
        s.close().await;
        std::fs::remove_dir_all(root)?;
        Ok(())
    }
}
