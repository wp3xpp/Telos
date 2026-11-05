//! Bounded delivery corrections after independent failure, never unknown execution.
use super::*;

#[derive(Debug, Clone)]
pub struct RepairCandidate {
    pub loop_id: String,
    pub cycle_id: String,
    pub revision: i64,
    pub policy_revision: i64,
    pub plan_version: i64,
    pub verification_id: String,
    pub artifact: String,
    pub manifest: Value,
}

pub(crate) const ELIGIBLE: &str = r#"
SELECT l.id AS loop_id,c.id AS cycle_id,l.revision,p.revision AS policy_revision,
       vp.version AS plan_version,v.id AS verification_id,d.artifact,d.manifest
FROM cycles c JOIN loops l ON l.id=c.loop_id
JOIN contracts ct ON ct.loop_id=l.id AND ct.revision=l.revision
JOIN project_execution_policies p ON p.project_id=l.project_id
JOIN verification_plans vp ON vp.loop_id=l.id AND vp.contract_revision=l.revision
JOIN verifications v ON v.rowid=(SELECT MAX(v2.rowid) FROM verifications v2 WHERE v2.cycle_id=c.id)
JOIN delivery_workspaces w ON w.cycle_id=c.id
JOIN delivery_snapshots d ON d.artifact=json_extract(v.body,'$.artifact') AND d.workspace_id=w.id
WHERE l.kind='delivery' AND l.state='active' AND c.contract_revision=l.revision
AND c.state NOT IN ('closed','cancelled') AND w.state='ready'
AND w.contract_revision=l.revision AND w.policy_revision=p.revision
AND json_extract(p.body,'$.enabled')=1 AND json_extract(p.body,'$.sandbox')='workspace_write'
AND v.verdict='failed' AND v.contract_revision=l.revision AND v.plan_version=vp.version AND d.plan_version=vp.version
AND d.step_count=(SELECT COUNT(*) FROM step_runs s JOIN work_items i ON i.id=s.work_item_id WHERE i.cycle_id=c.id)
AND NOT EXISTS (SELECT 1 FROM repair_deferrals f WHERE f.verification_id=v.id)
AND NOT EXISTS (SELECT 1 FROM enqueue_keys k WHERE k.cycle_id=c.id AND k.request_key='repair:'||v.id)
AND NOT EXISTS (SELECT 1 FROM verification_leases x WHERE x.project_id=l.project_id AND x.state IN ('running','outcome_unknown'))
AND NOT EXISTS (SELECT 1 FROM step_runs s JOIN work_items i ON i.id=s.work_item_id JOIN cycles c2 ON c2.id=i.cycle_id JOIN loops l2 ON l2.id=c2.loop_id WHERE l2.project_id=l.project_id AND s.state IN ('pending','ready','running','outcome_unknown'))
AND EXISTS (SELECT 1 FROM step_runs s JOIN work_items i ON i.id=s.work_item_id WHERE i.cycle_id=c.id AND s.state='succeeded' AND s.project_policy_revision=p.revision AND s.workspace_id=w.id)
AND d.step_count<MIN(json_extract(p.body,'$.max_steps_per_cycle'),json_extract(ct.body,'$.max_attempts'))
AND (COALESCE(json_array_length(vp.body,'$.test_checks'),0)=0 OR (SELECT COUNT(*) FROM verification_test_attempts a WHERE a.cycle_id=c.id)<json_extract(ct.body,'$.max_attempts'))
"#;

impl Store {
    pub async fn pending_repairs(&self) -> Result<Vec<RepairCandidate>> {
        let rows = sqlx::query(&format!("{ELIGIBLE} ORDER BY v.rowid LIMIT 16"))
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter()
            .map(|r| {
                Ok(RepairCandidate {
                    loop_id: r.get("loop_id"),
                    cycle_id: r.get("cycle_id"),
                    revision: r.get("revision"),
                    policy_revision: r.get("policy_revision"),
                    plan_version: r.get("plan_version"),
                    verification_id: r.get("verification_id"),
                    artifact: r.get("artifact"),
                    manifest: serde_json::from_str(r.get("manifest"))?,
                })
            })
            .collect()
    }

    pub async fn defer_repair(&self, candidate: &RepairCandidate, reason: &str) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let reason: String = reason.chars().take(2048).collect();
        let inserted = sqlx::query("INSERT INTO repair_deferrals(verification_id,reason,created_at) VALUES(?,?,?) ON CONFLICT DO NOTHING")
            .bind(&candidate.verification_id).bind(&reason).bind(now()).execute(&mut *tx).await?;
        if inserted.rows_affected() == 1 {
            Self::event(
                &mut tx,
                Some(&candidate.loop_id),
                "repair_deferred",
                json!({"verification_id":candidate.verification_id,"reason":reason}),
                true,
            )
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }
}

impl Store {
    /// Resolve before the runtime heartbeat/execution select; no DB await inside it.
    pub async fn repair_preflight(&self, step_id: &str) -> Result<Option<(Value, Vec<String>)>> {
        let row = sqlx::query("SELECT v.plan_version,vp.version,vp.body,d.manifest FROM enqueue_keys k JOIN verifications v ON k.request_key='repair:'||v.id JOIN cycles c ON c.id=v.cycle_id JOIN loops l ON l.id=c.loop_id JOIN verification_plans vp ON vp.loop_id=l.id AND vp.contract_revision=l.revision JOIN delivery_snapshots d ON d.artifact=json_extract(v.body,'$.artifact') WHERE k.step_id=?")
            .bind(step_id).fetch_optional(&self.pool).await?;
        row.map(|r| {
            ensure!(
                r.get::<Option<i64>, _>("plan_version") == Some(r.get::<i64, _>("version")),
                "repair verification plan changed before dispatch"
            );
            let plan: telos_core::VerificationPlan = serde_json::from_str(r.get("body"))?;
            Ok((
                serde_json::from_str(r.get("manifest"))?,
                plan.checks.into_iter().map(|c| c.relative_path).collect(),
            ))
        })
        .transpose()
    }
}
