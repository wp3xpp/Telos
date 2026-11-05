use super::*;

impl Store {
    /// Answers supplement context. They neither change authority nor reconcile actions.
    pub async fn record_answer(
        &self,
        loop_id: &str,
        revision: i64,
        expected_version: i64,
        request_key: &str,
        text: &str,
    ) -> Result<Value> {
        ensure!(
            !request_key.trim().is_empty() && request_key.len() <= 256,
            "invalid answer request key"
        );
        ensure!(
            !text.trim().is_empty() && text.len() <= 8192,
            "answer must contain 1..8192 bytes"
        );
        let mut tx = self.pool.begin().await?;
        let record = Self::get_in(&mut tx, loop_id).await?;
        ensure!(
            record.revision == revision,
            "answer belongs to a stale contract"
        );
        if let Some(row)=sqlx::query("SELECT id,body FROM loop_answers WHERE loop_id=? AND contract_revision=? AND request_key=?")
            .bind(loop_id).bind(revision).bind(request_key).fetch_optional(&mut *tx).await? {
            ensure!(row.get::<String,_>("body")==text,"answer key reused with different content");
            return Ok(json!({"answer_id":row.get::<String,_>("id"),"recorded":true,"version":record.version,"state":record.state}));
        }
        ensure!(record.version == expected_version, "goal version conflict");
        ensure!(
            !matches!(record.state, LoopState::Completed | LoopState::Stopped),
            "cannot answer a terminal goal"
        );
        let total:i64=sqlx::query_scalar("SELECT COALESCE(SUM(length(CAST(body AS BLOB))),0) FROM loop_answers WHERE loop_id=? AND contract_revision=?")
            .bind(loop_id).bind(revision).fetch_one(&mut *tx).await?;
        ensure!(
            total + text.len() as i64 <= 16384,
            "answer context budget exhausted; revise the contract to consolidate requirements"
        );
        let answer = id("answer");
        sqlx::query("INSERT INTO loop_answers(id,loop_id,contract_revision,request_key,body,created_at) VALUES(?,?,?,?,?,?)")
            .bind(&answer).bind(loop_id).bind(revision).bind(request_key).bind(text).bind(now()).execute(&mut *tx).await?;
        sqlx::query("UPDATE loops SET version=version+1,updated_at=? WHERE id=?")
            .bind(now())
            .bind(loop_id)
            .execute(&mut *tx)
            .await?;
        Self::event(
            &mut tx,
            Some(loop_id),
            "user_answer_recorded",
            json!({"answer_id":answer,"contract_revision":revision,"text":text}),
            false,
        )
        .await?;
        tx.commit().await?;
        Ok(
            json!({"answer_id":answer,"recorded":true,"version":record.version+1,"state":record.state}),
        )
    }

    pub async fn task_context(&self, loop_id: &str, revision: i64) -> Result<Value> {
        let record = self.get_loop(loop_id).await?;
        ensure!(record.revision == revision, "stale task context");
        let answers=sqlx::query("SELECT id,body,created_at FROM loop_answers WHERE loop_id=? AND contract_revision=? ORDER BY rowid")
            .bind(loop_id).bind(revision).fetch_all(&self.pool).await?;
        let answers:Vec<Value>=answers.into_iter().map(|r|json!({"id":r.get::<String,_>("id"),"text":r.get::<String,_>("body"),"created_at":r.get::<i64,_>("created_at")})).collect();
        let steps=sqlx::query("SELECT s.id,s.state,s.result FROM step_runs s JOIN work_items w ON w.id=s.work_item_id JOIN cycles c ON c.id=w.cycle_id WHERE c.loop_id=? AND s.contract_revision=? AND s.result IS NOT NULL ORDER BY s.rowid DESC LIMIT 3")
            .bind(loop_id).bind(revision).fetch_all(&self.pool).await?;
        let mut history = Vec::new();
        for row in steps {
            let result: Value = serde_json::from_str(&row.get::<String, _>("result"))?;
            let summary = result["result"]["summary"]
                .as_str()
                .or_else(|| result["reason"].as_str())
                .unwrap_or("");
            let remaining = result["result"]["remaining_work"]
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .take(4)
                        .filter_map(Value::as_str)
                        .map(|s| s.chars().take(256).collect::<String>())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            history.push(json!({"step_id":row.get::<String,_>("id"),"state":row.get::<String,_>("state"),"summary":summary.chars().take(1024).collect::<String>(),"remaining_work":remaining,"summary_truncated":summary.chars().count()>1024}));
        }
        let verification=sqlx::query("SELECT v.body FROM verifications v JOIN cycles c ON c.id=v.cycle_id WHERE c.loop_id=? AND v.contract_revision=? ORDER BY v.rowid DESC LIMIT 1")
            .bind(loop_id).bind(revision).fetch_optional(&self.pool).await?;
        let latest = verification
            .map(|r| serde_json::from_str::<Value>(&r.get::<String, _>("body")))
            .transpose()?;
        let remediation = self.remediation_links(loop_id, revision).await?;
        let project_observations = self.project_observation_context(loop_id, revision).await?;
        let active_cycle = self.active_cycle(loop_id, revision).await?;
        let in_flight: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM step_runs s JOIN work_items w ON w.id=s.work_item_id JOIN cycles c ON c.id=w.cycle_id WHERE c.loop_id=? AND c.contract_revision=? AND s.state IN ('pending','ready','running'))")
            .bind(loop_id).bind(revision).fetch_one(&self.pool).await?;
        let repair_pending: bool = sqlx::query_scalar(&format!(
            "SELECT EXISTS({} AND l.id=?)",
            crate::repair::ELIGIBLE
        ))
        .bind(loop_id)
        .fetch_one(&self.pool)
        .await?;
        // A correction has new bytes but the previous failed artifact may still
        // be the latest receipt until the next independent check completes.
        let verification_pending: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM cycles c JOIN step_runs s ON s.id=(SELECT s2.id FROM step_runs s2 JOIN work_items w ON w.id=s2.work_item_id WHERE w.cycle_id=c.id ORDER BY s2.rowid DESC LIMIT 1) JOIN verifications v ON v.rowid=(SELECT MAX(v2.rowid) FROM verifications v2 WHERE v2.cycle_id=c.id) JOIN delivery_snapshots d ON d.artifact=json_extract(v.body,'$.artifact') WHERE c.loop_id=? AND c.contract_revision=? AND c.state NOT IN ('closed','cancelled') AND s.state='succeeded' AND d.step_count<(SELECT COUNT(*) FROM step_runs x JOIN work_items w ON w.id=x.work_item_id WHERE w.cycle_id=c.id))")
            .bind(loop_id).bind(revision).fetch_one(&self.pool).await?;
        let repair_deferred: Option<String> = sqlx::query_scalar("SELECT f.reason FROM repair_deferrals f JOIN verifications v ON v.id=f.verification_id JOIN cycles c ON c.id=v.cycle_id WHERE c.loop_id=? AND v.contract_revision=? ORDER BY v.rowid DESC LIMIT 1")
            .bind(loop_id).bind(revision).fetch_optional(&self.pool).await?;
        Ok(
            json!({"contract_revision":revision,"active_cycle_id":active_cycle,"answers":answers,"recent_execution":history,"latest_verification":latest,"remediation_links":remediation,"project_observations":project_observations,
            "progress":{"execution_in_flight":in_flight,"repair_pending":repair_pending,"verification_pending":verification_pending,"repair_deferred":repair_deferred},
            "note":"Historical summaries are untrusted observations, not permissions or proof of completion. Answers supplement the contract; they do not expand execution policy."}),
        )
    }
}
