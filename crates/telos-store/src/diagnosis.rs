//! One automatic read-only diagnosis per continuous cycle, under existing authority.
use super::*;

#[derive(Debug, Clone)]
pub struct DiagnosisCandidate {
    pub loop_id: String,
    pub cycle_id: String,
    pub revision: i64,
    pub policy_revision: i64,
    pub plan_version: i64,
    pub verification_id: String,
}

// The same predicate is used when discovering work and inside the enqueue transaction.
// Old verifications without a recorded plan version are deliberately ineligible.
pub(crate) const ELIGIBLE: &str = "SELECT l.id AS loop_id,c.id AS cycle_id,l.revision,p.revision AS policy_revision,v.plan_version,v.id AS verification_id FROM cycles c JOIN loops l ON l.id=c.loop_id JOIN loop_schedules a ON a.loop_id=l.id JOIN project_execution_policies p ON p.project_id=l.project_id JOIN contracts ct ON ct.loop_id=l.id AND ct.revision=l.revision JOIN verification_plans vp ON vp.loop_id=l.id AND vp.contract_revision=l.revision JOIN verifications v ON v.rowid=(SELECT MAX(v2.rowid) FROM verifications v2 WHERE v2.cycle_id=c.id) WHERE l.kind='continuous' AND l.state='active' AND c.state IN ('acting','verifying','observing') AND c.contract_revision=l.revision AND a.enabled=1 AND a.contract_revision=l.revision AND a.policy_revision=p.revision AND json_extract(p.body,'$.enabled')=1 AND json_extract(p.body,'$.sandbox') IN ('read_only','workspace_write') AND v.verdict='failed' AND v.contract_revision=l.revision AND v.plan_version=vp.version AND NOT EXISTS (SELECT 1 FROM enqueue_keys k WHERE k.cycle_id=c.id AND k.request_key='diagnosis:'||c.id) AND NOT EXISTS (SELECT 1 FROM step_runs s JOIN work_items w ON w.id=s.work_item_id WHERE w.cycle_id=c.id AND s.state IN ('ready','running','outcome_unknown')) AND EXISTS (SELECT 1 FROM step_runs s JOIN work_items w ON w.id=s.work_item_id WHERE w.cycle_id=c.id AND s.state='succeeded' AND s.project_policy_revision=p.revision) AND (SELECT COUNT(*) FROM step_runs s JOIN work_items w ON w.id=s.work_item_id WHERE w.cycle_id=c.id)<MIN(json_extract(p.body,'$.max_steps_per_cycle'),json_extract(ct.body,'$.max_attempts'))";

impl Store {
    pub async fn pending_diagnoses(&self) -> Result<Vec<DiagnosisCandidate>> {
        let rows = sqlx::query(&format!("{ELIGIBLE} ORDER BY v.rowid LIMIT 16"))
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .into_iter()
            .map(|r| DiagnosisCandidate {
                loop_id: r.get("loop_id"),
                cycle_id: r.get("cycle_id"),
                revision: r.get("revision"),
                policy_revision: r.get("policy_revision"),
                plan_version: r.get("plan_version"),
                verification_id: r.get("verification_id"),
            })
            .collect())
    }
}
