//! Only leased parent observations begun after association can link recovery checks.
use super::*;
impl Store {
    pub(crate) async fn link_remediation_verification(
        tx: &mut Transaction<'_, Sqlite>,
        parent: &str,
        verification: &Verification,
        verification_id: &str,
        plan_version: Option<i64>,
        lease: Option<&str>,
        verdict: Verdict,
    ) -> Result<()> {
        let (Some(plan), Some(lease)) = (plan_version, lease) else {
            return Ok(());
        };
        let started:Option<i64>=sqlx::query_scalar("SELECT MAX(sequence) FROM events WHERE loop_id=? AND kind='verification_lease_acquired' AND json_extract(body,'$.lease_id')=?").bind(parent).bind(lease).fetch_one(&mut **tx).await?;
        let Some(started) = started else {
            return Ok(());
        };
        let links=sqlx::query("SELECT e.sequence,json_extract(e.body,'$.delivery_loop_id') AS child,json_extract(e.body,'$.delivery_revision') AS child_revision FROM events e JOIN loops l ON l.id=json_extract(e.body,'$.delivery_loop_id') WHERE e.loop_id=? AND e.kind='remediation_associated' AND json_extract(e.body,'$.revision')=? AND json_extract(e.body,'$.delivery_revision')=l.revision AND e.sequence<? ORDER BY e.sequence DESC LIMIT 16").bind(parent).bind(verification.contract_revision).bind(started).fetch_all(&mut **tx).await?;
        for link in links {
            Self::event(tx,Some(parent),"remediation_recovery_checked",json!({"revision":verification.contract_revision,"association_sequence":link.get::<i64,_>("sequence"),"delivery_loop_id":link.get::<String,_>("child"),"delivery_revision":link.get::<i64,_>("child_revision"),"verification_id":verification_id,"verification_lease_id":lease,"verification_started_sequence":started,"plan_version":plan,"verdict":verdict,"artifact":verification.artifact,"environment":verification.environment,"scope":"Historical independent parent checks begun after association, within configured criteria; no claim of remediation causality or health beyond observed scope"}),false).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn old_or_unleased_observation_cannot_link_recovery() -> Result<()> {
        let root = std::env::temp_dir().join(format!("telos-recovery-order-{}", Uuid::new_v4()));
        let store = Store::open(&root).await?;
        store.register_project("fixture", &root).await?;
        let mut contract = Contract {
            goal: "fixture".into(),
            delivery_scope: "fixture".into(),
            criteria: vec![telos_core::Criterion {
                id: "health".into(),
                description: "fixture only".into(),
            }],
            interval_seconds: Some(30),
            max_attempts: 1,
        };
        let parent = store
            .create_loop("fixture", "parent", LoopKind::Continuous, &contract)
            .await?;
        contract.interval_seconds = None;
        let child = store
            .create_loop("fixture", "child", LoopKind::Delivery, &contract)
            .await?;
        let mut tx = store.pool.begin().await?;
        Store::event(
            &mut tx,
            Some(&parent.id),
            "verification_lease_acquired",
            json!({"lease_id":"old"}),
            false,
        )
        .await?;
        Store::event(
            &mut tx,
            Some(&parent.id),
            "remediation_associated",
            json!({"revision":1,"delivery_loop_id":child.id,"delivery_revision":1}),
            false,
        )
        .await?;
        let verification = Verification {
            cycle_id: "fixture-not-engine-verification".into(),
            contract_revision: 1,
            artifact: "fixture".into(),
            environment: "fixture".into(),
            observation_complete: true,
            results: vec![],
        };
        Store::link_remediation_verification(
            &mut tx,
            &parent.id,
            &verification,
            "fixture",
            Some(1),
            None,
            Verdict::Passed,
        )
        .await?;
        Store::link_remediation_verification(
            &mut tx,
            &parent.id,
            &verification,
            "fixture",
            Some(1),
            Some("old"),
            Verdict::Passed,
        )
        .await?;
        let old: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM events WHERE kind='remediation_recovery_checked'",
        )
        .fetch_one(&mut *tx)
        .await?;
        assert_eq!(old, 0);
        Store::event(
            &mut tx,
            Some(&parent.id),
            "verification_lease_acquired",
            json!({"lease_id":"fresh"}),
            false,
        )
        .await?;
        Store::link_remediation_verification(
            &mut tx,
            &parent.id,
            &verification,
            "fixture",
            Some(1),
            Some("fresh"),
            Verdict::Inconclusive,
        )
        .await?;
        let fresh: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM events WHERE kind='remediation_recovery_checked'",
        )
        .fetch_one(&mut *tx)
        .await?;
        assert_eq!(fresh, 1);
        tx.rollback().await?;
        store.close().await;
        std::fs::remove_dir_all(root)?;
        Ok(())
    }
}
