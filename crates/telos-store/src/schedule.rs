use super::*;

impl Store {
    /// Only explicitly started managed continuous goals are eligible. One overdue
    /// occurrence is returned; missed wall-clock intervals are never replayed.
    pub async fn due_schedules(&self, at: i64) -> Result<Vec<Value>> {
        let rows = sqlx::query("SELECT l.id,l.revision,l.next_due_at,s.policy_revision FROM loop_schedules s JOIN loops l ON l.id=s.loop_id JOIN project_execution_policies p ON p.project_id=l.project_id WHERE s.enabled=1 AND l.state='active' AND l.kind='continuous' AND l.revision=s.contract_revision AND p.revision=s.policy_revision AND l.next_due_at<=? AND NOT EXISTS (SELECT 1 FROM cycles c WHERE c.loop_id=l.id AND c.state NOT IN ('closed','cancelled')) ORDER BY l.next_due_at,l.id LIMIT 16")
            .bind(at).fetch_all(&self.pool).await?;
        Ok(rows.into_iter().map(|r|json!({"id":r.get::<String,_>("id"),"revision":r.get::<i64,_>("revision"),"due_at":r.get::<i64,_>("next_due_at"),"policy_revision":r.get::<i64,_>("policy_revision")})).collect())
    }

    pub async fn schedule_status(&self, loop_id: &str) -> Result<Value> {
        let record = self.get_loop(loop_id).await?;
        let row = sqlx::query("SELECT * FROM loop_schedules WHERE loop_id=?")
            .bind(loop_id)
            .fetch_optional(&self.pool)
            .await?;
        let mut status = match row {
            Some(row) => {
                json!({"armed":row.get::<i64,_>("enabled")==1,"contract_revision":row.get::<i64,_>("contract_revision"),"policy_revision":row.get::<i64,_>("policy_revision"),"suspended_reason":row.get::<Option<String>,_>("suspended_reason"),"loop_state":record.state,"next_due_at":record.next_due_at})
            }
            None => {
                json!({"armed":false,"loop_state":record.state,"next_due_at":record.next_due_at})
            }
        };
        status["armed_scope"] = json!("managed_execution_only");
        Ok(status)
    }

    pub async fn suspend_schedule(
        &self,
        loop_id: &str,
        revision: i64,
        policy_revision: i64,
        reason: &str,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let n=sqlx::query("UPDATE loop_schedules SET enabled=0,suspended_reason=?,updated_at=? WHERE loop_id=? AND contract_revision=? AND policy_revision=? AND enabled=1")
            .bind(reason).bind(now()).bind(loop_id).bind(revision).bind(policy_revision).execute(&mut *tx).await?.rows_affected();
        if n > 0 {
            Self::event(
                &mut tx,
                Some(loop_id),
                "schedule_suspended",
                json!({"revision":revision,"reason":reason}),
                true,
            )
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }
}
