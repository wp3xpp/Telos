//! Operator ingestion of project-owned normalized collection. Not acceptance.
use super::*;
use telos_core::observation::{Batch, Cursor};
impl Store {
    pub async fn observation_checkpoint(
        &self,
        goal: &str,
        revision: i64,
        source: &str,
    ) -> Result<Value> {
        ensure!(
            self.get_loop(goal).await?.revision == revision,
            "stale observation checkpoint"
        );
        let row=sqlx::query("SELECT body FROM events WHERE loop_id=? AND kind='project_observation_collected' AND json_extract(body,'$.revision')=? AND json_extract(body,'$.batch.source_id')=? ORDER BY sequence DESC LIMIT 1")
            .bind(goal).bind(revision).bind(source).fetch_optional(&self.pool).await?;
        Ok(row
            .map(|r| serde_json::from_str(r.get("body")))
            .transpose()?
            .unwrap_or(json!({"version":0,"cursor":null,"business_health":"unverified"})))
    }
    pub async fn project_observation_context(&self, goal: &str, revision: i64) -> Result<Value> {
        ensure!(
            self.get_loop(goal).await?.revision == revision,
            "stale observation context"
        );
        let rows=sqlx::query("SELECT sequence,body FROM events WHERE sequence IN (SELECT MAX(sequence) FROM events WHERE loop_id=? AND kind='project_observation_collected' AND json_extract(body,'$.revision')=? GROUP BY json_extract(body,'$.batch.source_id')) ORDER BY sequence DESC LIMIT 17")
            .bind(goal).bind(revision).fetch_all(&self.pool).await?;
        let more = rows.len() > 16;
        let mut items = vec![];
        for row in rows.into_iter().take(16) {
            let v: Value = serde_json::from_str(row.get("body"))?;
            items.push(json!({"event_sequence":row.get::<i64,_>("sequence"),"source_id":v["batch"]["source_id"],"version":v["version"],"cursor":v["cursor"],"backlog_bytes":v["backlog_bytes"],"gap_reason":v["batch"]["gap_reason"],"new_signals":v["new_signals"]}));
        }
        Ok(json!({"items":items,"more":more,"business_health":"unverified"}))
    }

    pub async fn pending_project_observations(&self, at: i64) -> Result<Vec<Value>> {
        let rows=sqlx::query("SELECT l.id,l.revision FROM loops l JOIN cycles c ON c.loop_id=l.id AND c.contract_revision=l.revision JOIN loop_schedules s ON s.loop_id=l.id AND s.contract_revision=l.revision JOIN project_execution_policies p ON p.project_id=l.project_id AND p.revision=s.policy_revision JOIN verification_plans v ON v.loop_id=l.id AND v.contract_revision=l.revision WHERE l.kind='continuous' AND l.state='active' AND s.enabled=1 AND json_extract(p.body,'$.enabled')=1 AND c.state NOT IN ('closed','cancelled') AND EXISTS(SELECT 1 FROM json_each(v.body,'$.test_checks') WHERE json_extract(value,'$.adapter')='local.observation_json') AND NOT EXISTS(SELECT 1 FROM step_runs x JOIN work_items w ON w.id=x.work_item_id WHERE w.cycle_id=c.id AND x.state IN ('ready','pending','running','outcome_unknown')) AND NOT EXISTS(SELECT 1 FROM verification_leases z WHERE z.project_id=l.project_id AND z.state IN ('running','outcome_unknown')) AND COALESCE((SELECT MAX(z.created_at) FROM verification_leases z WHERE z.cycle_id=c.id),0)<=?-(SELECT COALESCE(MIN(json_extract(value,'$.observation.poll_interval_seconds')),30) FROM json_each(v.body,'$.test_checks') WHERE json_extract(value,'$.adapter')='local.observation_json') ORDER BY l.id LIMIT 16")
            .bind(at).fetch_all(&self.pool).await?;
        Ok(rows
            .into_iter()
            .map(|r| json!({"id":r.get::<String,_>("id"),"revision":r.get::<i64,_>("revision")}))
            .collect())
    }

    pub async fn record_observation_batch(
        &self,
        goal: &str,
        revision: i64,
        batch: &Batch,
    ) -> Result<Value> {
        batch.validate()?;
        ensure!(
            serde_json::to_vec(batch)?.len() <= 256 * 1024,
            "collection metadata exceeds bound"
        );
        let mut tx = self.pool.begin().await?;
        let record = Self::get_in(&mut tx, goal).await?;
        ensure!(
            record.revision == revision
                && matches!(record.state, LoopState::Active | LoopState::Blocked)
                && record.kind == LoopKind::Continuous,
            "inactive or stale collection"
        );
        let previous=sqlx::query("SELECT body FROM events WHERE loop_id=? AND kind='project_observation_collected' AND json_extract(body,'$.revision')=? AND json_extract(body,'$.batch.source_id')=? AND json_extract(body,'$.batch.request_key')=? ORDER BY sequence DESC LIMIT 1")
            .bind(goal).bind(revision).bind(&batch.source_id).bind(&batch.request_key).fetch_optional(&mut *tx).await?;
        if let Some(row) = previous {
            let body: Value = serde_json::from_str(row.get("body"))?;
            ensure!(
                body["batch"] == serde_json::to_value(batch)?,
                "collection request key reused with different content"
            );
            return Ok(body);
        }
        let last=sqlx::query("SELECT body FROM events WHERE loop_id=? AND kind='project_observation_collected' AND json_extract(body,'$.revision')=? AND json_extract(body,'$.batch.source_id')=? ORDER BY sequence DESC LIMIT 1")
            .bind(goal).bind(revision).bind(&batch.source_id).fetch_optional(&mut *tx).await?;
        let last = last
            .map(|r| serde_json::from_str::<Value>(r.get("body")))
            .transpose()?;
        let version = last
            .as_ref()
            .and_then(|v| v["version"].as_i64())
            .unwrap_or(0);
        ensure!(
            batch.expected_version == version,
            "collection cursor version conflict"
        );
        if let Some(last) = &last {
            let cursor: Cursor = serde_json::from_value(last["cursor"].clone())?;
            ensure!(
                (cursor.generation == batch.generation && cursor.offset == batch.start)
                    || batch.gap_reason.is_some(),
                "collection gap/rotation requires an explicit reason"
            );
        } else {
            ensure!(
                batch.start == 0 || batch.gap_reason.is_some(),
                "initial skipped history requires an explicit gap"
            );
        }
        let mut unique = std::collections::BTreeSet::new();
        let mut new_signals = 0;
        for signal in &batch.signals {
            if !unique.insert((&signal.resource_key, &signal.episode_key, &signal.kind)) {
                continue;
            }
            let exists:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM events WHERE loop_id=? AND kind='project_signal_observed' AND json_extract(body,'$.revision')=? AND json_extract(body,'$.source_id')=? AND json_extract(body,'$.signal.resource_key')=? AND json_extract(body,'$.signal.episode_key')=? AND json_extract(body,'$.signal.kind')=?)")
                .bind(goal).bind(revision).bind(&batch.source_id).bind(&signal.resource_key).bind(&signal.episode_key).bind(&signal.kind).fetch_one(&mut *tx).await?;
            if !exists {
                Self::event(
                    &mut tx,
                    Some(goal),
                    "project_signal_observed",
                    json!({"revision":revision,"source_id":batch.source_id,"signal":signal}),
                    false,
                )
                .await?;
                new_signals += 1;
            }
        }
        let body = json!({"revision":revision,"version":version+1,"batch":batch,
            "cursor":Cursor{generation:batch.generation.clone(),offset:batch.end},
            "backlog_bytes":batch.source_size-batch.end,"new_signals":new_signals,
            "business_health":"unverified","note":"Collection metadata only; acceptance requires independent project checks"});
        Self::event(
            &mut tx,
            Some(goal),
            "project_observation_collected",
            body.clone(),
            false,
        )
        .await?;
        tx.commit().await?;
        Ok(body)
    }
}

impl Store {
    pub async fn observation_collection_gate(
        &self,
        goal: &str,
        revision: i64,
        source: &str,
        window_start: i64,
    ) -> Result<Value> {
        let checkpoint = self.observation_checkpoint(goal, revision, source).await?;
        if checkpoint["version"] == 0 {
            return Ok(json!({"required":false,"satisfied":true}));
        }
        let gap:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM events WHERE loop_id=? AND kind='project_observation_collected' AND json_extract(body,'$.revision')=? AND json_extract(body,'$.batch.source_id')=? AND json_extract(body,'$.batch.gap_reason') IS NOT NULL AND created_at>=?)")
            .bind(goal).bind(revision).bind(source).bind(window_start).fetch_one(&self.pool).await?;
        let backlog = checkpoint["backlog_bytes"]
            .as_u64()
            .context("collection backlog missing")?;
        Ok(
            json!({"required":true,"satisfied":backlog==0 && !gap,"version":checkpoint["version"],"backlog_bytes":backlog,"gap_in_window":gap}),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use telos_core::{Contract, Criterion};
    #[tokio::test]
    async fn collection_is_atomic_resumable_deduplicated_and_never_accepts_health() -> Result<()> {
        let root =
            std::env::temp_dir().join(format!("telos-project-collection-{}", Uuid::new_v4()));
        let store = Store::open(&root).await?;
        store.register_project("project", &root).await?;
        let contract = Contract {
            goal: "observe fixture".into(),
            delivery_scope: "fixture only".into(),
            criteria: vec![Criterion {
                id: "health".into(),
                description: "independent check".into(),
            }],
            interval_seconds: Some(21600),
            max_attempts: 2,
        };
        let goal = store
            .create_loop("project", "fixture", LoopKind::Continuous, &contract)
            .await?;
        let signal = telos_core::observation::Signal {
            resource_key: "resource-A".into(),
            episode_key: "episode-1".into(),
            kind: "unavailable".into(),
        };
        let mut batch = Batch {
            source_id: "source".into(),
            request_key: "batch-1".into(),
            expected_version: 0,
            generation: "generation-1".into(),
            start: 0,
            end: 256 * 1024,
            source_size: 1024 * 1024,
            gap_reason: None,
            signals: vec![signal; 111],
            evidence: vec!["bounded-range-digest".into()],
        };
        let first = store.record_observation_batch(&goal.id, 1, &batch).await?;
        assert_eq!(first["new_signals"], 1);
        assert_eq!(first["backlog_bytes"], 768 * 1024);
        assert_eq!(
            store.record_observation_batch(&goal.id, 1, &batch).await?,
            first
        );
        batch.request_key = "stale".into();
        assert!(store
            .record_observation_batch(&goal.id, 1, &batch)
            .await
            .is_err());
        store.close().await;
        let store = Store::open(&root).await?;
        assert_eq!(
            store.observation_checkpoint(&goal.id, 1, "source").await?,
            first
        );
        batch.expected_version = 1;
        batch.request_key = "batch-2".into();
        batch.start = batch.end;
        batch.end += 256 * 1024;
        let second = store.record_observation_batch(&goal.id, 1, &batch).await?;
        assert_eq!(second["new_signals"], 0);
        assert_eq!(second["version"], 2);
        batch.expected_version = 2;
        batch.request_key = "rotated".into();
        batch.generation = "generation-2".into();
        batch.start = 0;
        batch.end = 10;
        batch.source_size = 10;
        assert!(store
            .record_observation_batch(&goal.id, 1, &batch)
            .await
            .is_err());
        batch.gap_reason = Some("explicit rotation with unobserved history".into());
        store.record_observation_batch(&goal.id, 1, &batch).await?;
        assert_eq!(store.get_loop(&goal.id).await?.state, LoopState::Active);
        assert!(store.task_context(&goal.id, 1).await?["latest_verification"].is_null());
        assert_eq!(
            store.task_context(&goal.id, 1).await?["project_observations"]["items"][0]["version"],
            3
        );
        let updated = store
            .update_contract(&goal.id, goal.version, &contract)
            .await?;
        assert_eq!(
            store
                .observation_checkpoint(&goal.id, updated.revision, "source")
                .await?["version"],
            0
        );
        assert!(store
            .record_observation_batch(&goal.id, 1, &batch)
            .await
            .is_err());
        store.close().await;
        std::fs::remove_dir_all(root)?;
        Ok(())
    }
}
