//! Durable execution ownership. Expired work becomes uncertain, never auto-ready.
use super::*;
use telos_core::{ExecutionOutcome, ExecutionSpec, JobLease};

impl Store {
    pub async fn find_managed_request(
        &self,
        loop_id: &str,
        revision: i64,
        request_key: &str,
    ) -> Result<Option<Value>> {
        let row=sqlx::query("SELECT s.id,s.state,w.cycle_id FROM enqueue_keys k JOIN step_runs s ON s.id=k.step_id JOIN work_items w ON w.id=s.work_item_id JOIN cycles c ON c.id=w.cycle_id WHERE c.loop_id=? AND s.contract_revision=? AND k.request_key=? AND s.project_policy_revision IS NOT NULL ORDER BY s.rowid LIMIT 1")
            .bind(loop_id).bind(revision).bind(request_key).fetch_optional(&self.pool).await?;
        Ok(row.map(|r|json!({"cycle_id":r.get::<String,_>("cycle_id"),"step_id":r.get::<String,_>("id"),"state":r.get::<String,_>("state"),"reused":true})))
    }
    pub async fn enqueue_step(
        &self,
        cycle_id: &str,
        revision: i64,
        request_key: &str,
        spec: &ExecutionSpec,
        due_at: i64,
    ) -> Result<String> {
        self.enqueue_inner(
            cycle_id,
            revision,
            request_key,
            spec,
            due_at,
            None,
            None,
            None,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn enqueue_managed_step(
        &self,
        cycle_id: &str,
        revision: i64,
        request_key: &str,
        spec: &ExecutionSpec,
        due_at: i64,
        policy_revision: i64,
    ) -> Result<String> {
        self.enqueue_inner(
            cycle_id,
            revision,
            request_key,
            spec,
            due_at,
            Some(policy_revision),
            None,
            None,
        )
        .await
    }

    pub async fn enqueue_diagnosis(
        &self,
        candidate: &DiagnosisCandidate,
        spec: &ExecutionSpec,
    ) -> Result<String> {
        self.enqueue_inner(
            &candidate.cycle_id,
            candidate.revision,
            &format!("diagnosis:{}", candidate.cycle_id),
            spec,
            now(),
            Some(candidate.policy_revision),
            Some(candidate),
            None,
        )
        .await
    }

    pub async fn enqueue_repair(
        &self,
        candidate: &RepairCandidate,
        spec: &ExecutionSpec,
    ) -> Result<String> {
        self.enqueue_inner(
            &candidate.cycle_id,
            candidate.revision,
            &format!("repair:{}", candidate.verification_id),
            spec,
            now(),
            Some(candidate.policy_revision),
            None,
            Some(candidate),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn enqueue_inner(
        &self,
        cycle_id: &str,
        revision: i64,
        request_key: &str,
        spec: &ExecutionSpec,
        due_at: i64,
        policy_revision: Option<i64>,
        diagnosis: Option<&DiagnosisCandidate>,
        repair: Option<&RepairCandidate>,
    ) -> Result<String> {
        spec.validate()?;
        ensure!(!request_key.trim().is_empty(), "enqueue key required");
        let cwd = Path::new(&spec.cwd).canonicalize()?;
        ensure!(cwd.is_dir(), "execution cwd is not a directory");
        let mut tx = self.pool.begin().await?;

        if let Some(r) = repair {
            let eligible = sqlx::query(&format!(
                "{} AND c.id=? AND v.id=? AND p.revision=? AND vp.version=? AND d.artifact=?",
                repair::ELIGIBLE
            ))
            .bind(&r.cycle_id)
            .bind(&r.verification_id)
            .bind(r.policy_revision)
            .bind(r.plan_version)
            .bind(&r.artifact)
            .fetch_optional(&mut *tx)
            .await?;
            ensure!(eligible.is_some(), "repair candidate changed or consumed");
            ensure!(
                spec.sandbox == telos_core::Sandbox::WorkspaceWrite,
                "repair must use owned write workspace"
            );
        }
        if let Some(d) = diagnosis {
            let eligible = sqlx::query(&format!(
                "{} AND c.id=? AND v.id=? AND p.revision=? AND vp.version=?",
                diagnosis::ELIGIBLE
            ))
            .bind(&d.cycle_id)
            .bind(&d.verification_id)
            .bind(d.policy_revision)
            .bind(d.plan_version)
            .fetch_optional(&mut *tx)
            .await?;
            ensure!(
                eligible.is_some(),
                "diagnosis candidate changed or already consumed"
            );
        }
        let cycle=sqlx::query("SELECT c.loop_id,c.state,p.root,p.id AS project_id FROM cycles c JOIN loops l ON l.id=c.loop_id JOIN projects p ON p.id=l.project_id WHERE c.id=?")
            .bind(cycle_id).fetch_one(&mut *tx).await?;
        let workspace_id = if policy_revision.is_some()
            && spec.sandbox == telos_core::Sandbox::WorkspaceWrite
        {
            let workspace = sqlx::query("SELECT id,path FROM delivery_workspaces WHERE cycle_id=? AND contract_revision=? AND policy_revision=? AND state='ready'")
                .bind(cycle_id).bind(revision).bind(policy_revision).fetch_optional(&mut *tx).await?.context("write execution requires a ready workspace under the current authority")?;
            let path: String = workspace.get("path");
            let m = std::fs::symlink_metadata(&path)?;
            ensure!(
                m.is_dir()
                    && !m.file_type().is_symlink()
                    && cwd == Path::new(&path)
                    && spec.cwd == path,
                "write execution must use the exact owned workspace root"
            );
            Some(workspace.get::<String, _>("id"))
        } else {
            ensure!(
                cwd.starts_with(Path::new(&cycle.get::<String, _>("root"))),
                "execution cwd is outside registered project"
            );
            None
        };
        let loop_id: String = cycle.get("loop_id");
        let record = Self::get_in(&mut tx, &loop_id).await?;
        ensure!(
            record.kind != LoopKind::Continuous || spec.sandbox == telos_core::Sandbox::ReadOnly,
            "continuous execution must remain read-only"
        );
        Self::require_verification_idle(&mut tx, &record.project_id).await?;
        ensure!(
            record.state == LoopState::Active && record.revision == revision,
            "inactive loop or stale contract"
        );
        ensure!(
            !matches!(cycle.get::<&str, _>("state"), "closed" | "cancelled"),
            "cycle is terminal"
        );
        let policy = if let Some(required) = policy_revision {
            let row = sqlx::query(
                "SELECT revision,body FROM project_execution_policies WHERE project_id=?",
            )
            .bind(cycle.get::<String, _>("project_id"))
            .fetch_optional(&mut *tx)
            .await?
            .context("project execution not configured")?;
            ensure!(
                row.get::<i64, _>("revision") == required,
                "stale project execution policy"
            );
            let policy: telos_core::ProjectExecutionPolicy =
                serde_json::from_str(&row.get::<String, _>("body"))?;
            policy.permits(spec)?;
            Some(policy)
        } else {
            None
        };
        if let Some(row)=sqlx::query("SELECT k.step_id,s.spec,s.project_policy_revision FROM enqueue_keys k JOIN step_runs s ON s.id=k.step_id WHERE k.cycle_id=? AND k.request_key=?")
            .bind(cycle_id).bind(request_key).fetch_optional(&mut *tx).await? {
            ensure!(serde_json::from_str::<Value>(&row.get::<String,_>("spec"))?==serde_json::to_value(spec)?, "enqueue key reused for a different request");
            ensure!(row.get::<Option<i64>,_>("project_policy_revision") == policy_revision, "enqueue key reused under different authority");
            return Ok(row.get("step_id"));
        }
        if let Some(policy) = policy {
            let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM step_runs s JOIN work_items w ON w.id=s.work_item_id WHERE w.cycle_id=?")
                .bind(cycle_id).fetch_one(&mut *tx).await?;
            if workspace_id.is_some() {
                ensure!(
                    count < i64::from(record.contract.max_attempts),
                    "delivery contract attempt budget exhausted"
                );
            }
            ensure!(
                count < i64::from(policy.max_steps_per_cycle),
                "project cycle step budget exhausted"
            );
        }
        let work_id = id("work");
        if record.kind == LoopKind::Continuous {
            if let Some(policy_revision) = policy_revision {
                sqlx::query("INSERT INTO loop_schedules(loop_id,contract_revision,policy_revision,enabled,updated_at) VALUES(?,?,?,1,?) ON CONFLICT(loop_id) DO UPDATE SET contract_revision=excluded.contract_revision,policy_revision=excluded.policy_revision,enabled=1,suspended_reason=NULL,updated_at=excluded.updated_at")
                    .bind(&loop_id).bind(revision).bind(policy_revision).bind(now()).execute(&mut *tx).await?;
                Self::event(
                    &mut tx,
                    Some(&loop_id),
                    "schedule_armed",
                    json!({"revision":revision,"policy_revision":policy_revision}),
                    false,
                )
                .await?;
            }
        }
        let step_id = id("step");
        sqlx::query(
            "INSERT INTO work_items(id,cycle_id,goal,state,created_at) VALUES(?,?,?,'queued',?)",
        )
        .bind(&work_id)
        .bind(cycle_id)
        .bind(&spec.task)
        .bind(now())
        .execute(&mut *tx)
        .await?;
        sqlx::query("INSERT INTO step_runs(id,work_item_id,contract_revision,state,created_at,spec,due_at,project_policy_revision,workspace_id) VALUES(?,?,?,'ready',?,?,?,?,?)")
            .bind(&step_id).bind(work_id).bind(revision).bind(now()).bind(serde_json::to_string(spec)?).bind(due_at).bind(policy_revision).bind(&workspace_id).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO enqueue_keys(cycle_id,request_key,step_id) VALUES(?,?,?)")
            .bind(cycle_id)
            .bind(request_key)
            .bind(&step_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE cycles SET state='acting' WHERE id=?")
            .bind(cycle_id)
            .execute(&mut *tx)
            .await?;
        Self::event(
            &mut tx,
            Some(&loop_id),
            "step_enqueued",
            json!({"step_id":step_id,"cycle_id":cycle_id}),
            false,
        )
        .await?;
        if let Some(r) = repair {
            Self::event(&mut tx, Some(&loop_id), "repair_enqueued", json!({"step_id":step_id,"verification_id":r.verification_id,"artifact":r.artifact,"plan_version":r.plan_version}), true).await?;
        }
        if let Some(d) = diagnosis {
            Self::event(&mut tx, Some(&loop_id), "diagnosis_enqueued", json!({"cycle_id":cycle_id,"step_id":step_id,"verification_id":d.verification_id,"plan_version":d.plan_version,"policy_revision":d.policy_revision}), false).await?;
        }

        tx.commit().await?;
        Ok(step_id)
    }

    pub async fn claim_next(
        &self,
        worker_id: &str,
        at: i64,
        lease_seconds: i64,
    ) -> Result<Option<JobLease>> {
        ensure!(
            !worker_id.is_empty() && (1..=3600).contains(&lease_seconds),
            "invalid lease parameters"
        );
        let until = at
            .checked_add(lease_seconds)
            .context("lease timestamp overflow")?;
        let mut tx = self.pool.begin().await?;
        let Some(row)=sqlx::query("SELECT s.*,w.cycle_id,c.loop_id FROM step_runs s JOIN work_items w ON w.id=s.work_item_id JOIN cycles c ON c.id=w.cycle_id JOIN loops l ON l.id=c.loop_id WHERE s.state='ready' AND s.due_at<=? AND l.state='active' AND NOT EXISTS (SELECT 1 FROM recovery_project_holds h WHERE h.project_id=l.project_id AND h.state='held') AND NOT EXISTS(SELECT 1 FROM events e JOIN loops el ON el.id=e.loop_id WHERE el.project_id=l.project_id AND e.kind IN ('environment_operation_intent','environment_operation_outcome','environment_operation_reconciled','environment_session_uncertain') AND json_extract(e.body,'$.phase') IN ('pending','outcome_unknown') AND e.sequence=(SELECT MAX(s2.sequence) FROM events s2 WHERE s2.kind IN ('environment_operation_intent','environment_operation_outcome','environment_operation_reconciled','environment_session_uncertain') AND json_extract(s2.body,'$.session_id')=json_extract(e.body,'$.session_id'))) AND s.contract_revision=l.revision AND c.state NOT IN ('closed','cancelled') ORDER BY s.due_at,s.created_at,s.id LIMIT 1")
            .bind(at).fetch_optional(&mut *tx).await? else { return Ok(None) };
        let loop_id: String = row.get("loop_id");
        let record = Self::get_in(&mut tx, &loop_id).await?;
        ensure!(
            row.get::<i64, _>("attempt") < i64::from(record.contract.max_attempts),
            "attempt budget exhausted"
        );
        let step_id: String = row.get("id");
        let diagnosis: Option<String> = sqlx::query_scalar("SELECT body FROM events WHERE kind='diagnosis_enqueued' AND json_extract(body,'$.step_id')=? ORDER BY sequence DESC LIMIT 1")
            .bind(&step_id).fetch_optional(&mut *tx).await?;
        if let Some(body) = diagnosis {
            let body: Value = serde_json::from_str(&body)?;
            let valid: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM verification_plans p JOIN verifications v ON v.rowid=(SELECT MAX(v2.rowid) FROM verifications v2 WHERE v2.cycle_id=?) WHERE p.loop_id=? AND p.contract_revision=? AND p.version=? AND v.plan_version=p.version AND v.verdict='failed' AND v.id=?")
                .bind(row.get::<String,_>("cycle_id")).bind(&loop_id).bind(record.revision).bind(body["plan_version"].as_i64()).bind(body["verification_id"].as_str())
                .fetch_one(&mut *tx).await?;
            if valid == 0 {
                sqlx::query("UPDATE step_runs SET state='cancelled' WHERE id=?")
                    .bind(&step_id)
                    .execute(&mut *tx)
                    .await?;
                sqlx::query("UPDATE work_items SET state='cancelled' WHERE id=?")
                    .bind(row.get::<String, _>("work_item_id"))
                    .execute(&mut *tx)
                    .await?;
                Self::event(
                    &mut tx,
                    Some(&loop_id),
                    "diagnosis_cancelled",
                    json!({"step_id":step_id,"reason":"observation or verification plan changed"}),
                    false,
                )
                .await?;
                tx.commit().await?;
                return Ok(None);
            }
        }
        let generation = row.get::<i64, _>("lease_generation") + 1;
        sqlx::query("UPDATE step_runs SET state='running',attempt=attempt+1,lease_generation=?,worker_id=?,lease_until=? WHERE id=? AND state='ready'")
            .bind(generation).bind(worker_id).bind(until).bind(&step_id).execute(&mut *tx).await?;
        sqlx::query("UPDATE work_items SET state='running' WHERE id=?")
            .bind(row.get::<String, _>("work_item_id"))
            .execute(&mut *tx)
            .await?;
        Self::event(
            &mut tx,
            Some(&loop_id),
            "step_claimed",
            json!({"step_id":step_id,"worker_id":worker_id,"generation":generation}),
            false,
        )
        .await?;
        let lease = JobLease {
            step_id,
            loop_id,
            cycle_id: row.get("cycle_id"),
            contract_revision: row.get("contract_revision"),
            generation,
            worker_id: worker_id.into(),
            lease_until: until,
            workspace_id: row.get("workspace_id"),
            spec: serde_json::from_str(&row.get::<String, _>("spec"))?,
        };
        tx.commit().await?;
        Ok(Some(lease))
    }

    pub async fn heartbeat(&self, lease: &JobLease, at: i64, lease_seconds: i64) -> Result<()> {
        ensure!(
            (1..=3600).contains(&lease_seconds),
            "invalid lease duration"
        );
        let until = at
            .checked_add(lease_seconds)
            .context("lease timestamp overflow")?;
        let mut tx = self.pool.begin().await?;
        let rows=sqlx::query("UPDATE step_runs SET lease_until=? WHERE id=? AND state='running' AND worker_id=? AND lease_generation=? AND lease_until>? AND contract_revision=?")
            .bind(until).bind(&lease.step_id).bind(&lease.worker_id).bind(lease.generation).bind(at).bind(lease.contract_revision).execute(&mut *tx).await?.rows_affected();
        ensure!(rows == 1, "stale or expired lease");
        tx.commit().await?;
        Ok(())
    }

    pub async fn finish_step(
        &self,
        lease: &JobLease,
        outcome: ExecutionOutcome,
        result: &Value,
        at: i64,
    ) -> Result<()> {
        let serialized = serde_json::to_string(result)?;
        ensure!(serialized.len() <= 1024 * 1024, "step result too large");
        let mut tx = self.pool.begin().await?;
        let row=sqlx::query("SELECT s.*,w.cycle_id,c.loop_id FROM step_runs s JOIN work_items w ON w.id=s.work_item_id JOIN cycles c ON c.id=w.cycle_id WHERE s.id=?")
            .bind(&lease.step_id).fetch_one(&mut *tx).await?;
        ensure!(
            row.get::<&str, _>("state") == "running"
                && row.get::<i64, _>("lease_generation") == lease.generation
                && row.get::<String, _>("worker_id") == lease.worker_id,
            "stale execution owner"
        );
        ensure!(
            row.get::<i64, _>("lease_until") > at,
            "execution lease expired"
        );
        ensure!(
            row.get::<String, _>("loop_id") == lease.loop_id
                && row.get::<String, _>("cycle_id") == lease.cycle_id,
            "lease scope mismatch"
        );
        let record = Self::get_in(&mut tx, &lease.loop_id).await?;
        ensure!(
            record.revision == lease.contract_revision
                && row.get::<i64, _>("contract_revision") == record.revision,
            "stale result revision"
        );
        sqlx::query(
            "UPDATE step_runs SET state=?,result=?,finished_at=?,lease_until=NULL WHERE id=?",
        )
        .bind(tag(outcome)?)
        .bind(serialized)
        .bind(at)
        .bind(&lease.step_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE work_items SET state=? WHERE id=?")
            .bind(tag(outcome)?)
            .bind(row.get::<String, _>("work_item_id"))
            .execute(&mut *tx)
            .await?;
        if record.state == LoopState::Active {
            if outcome == ExecutionOutcome::Succeeded {
                // Agent completion only advances to verification; never completes a Loop.
                sqlx::query("UPDATE cycles SET state='verifying' WHERE id=?")
                    .bind(&lease.cycle_id)
                    .execute(&mut *tx)
                    .await?;
            } else {
                sqlx::query("UPDATE cycles SET state='blocked' WHERE id=?")
                    .bind(&lease.cycle_id)
                    .execute(&mut *tx)
                    .await?;
                sqlx::query(
                    "UPDATE loops SET state='blocked',version=version+1,updated_at=? WHERE id=?",
                )
                .bind(at)
                .bind(&lease.loop_id)
                .execute(&mut *tx)
                .await?;
            }
        }
        if outcome == ExecutionOutcome::Succeeded {
            let diagnosis: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM enqueue_keys WHERE step_id=? AND request_key=?",
            )
            .bind(&lease.step_id)
            .bind(format!("diagnosis:{}", lease.cycle_id))
            .fetch_one(&mut *tx)
            .await?;
            if diagnosis > 0 {
                Self::event(&mut tx, Some(&lease.loop_id), "diagnosis_ready", json!({"step_id":lease.step_id,"cycle_id":lease.cycle_id,"note":"Diagnosis is available; recovery still requires independent verification."}), true).await?;
            }
        }
        Self::event(
            &mut tx,
            Some(&lease.loop_id),
            "step_finished",
            json!({"step_id":lease.step_id,"outcome":outcome,"result":result}),
            outcome != ExecutionOutcome::Succeeded,
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn reconcile_expired(&self, at: i64) -> Result<usize> {
        self.reconcile_unfinished(at, false).await
    }

    /// Only the exclusively locked daemon calls this after process inspection.
    pub async fn reconcile_interrupted(&self, at: i64) -> Result<usize> {
        self.reconcile_unfinished(at, true).await
    }

    async fn reconcile_unfinished(&self, at: i64, interrupted: bool) -> Result<usize> {
        let mut tx = self.pool.begin().await?;
        let rows=sqlx::query("SELECT s.id,s.work_item_id,w.cycle_id,c.loop_id FROM step_runs s JOIN work_items w ON w.id=s.work_item_id JOIN cycles c ON c.id=w.cycle_id WHERE s.state='running' AND (? OR s.lease_until<=?)")
            .bind(interrupted).bind(at).fetch_all(&mut *tx).await?;
        for row in &rows {
            let step_id: String = row.get("id");
            let loop_id: String = row.get("loop_id");
            sqlx::query("UPDATE step_runs SET state='outcome_unknown',finished_at=? WHERE id=?")
                .bind(at)
                .bind(&step_id)
                .execute(&mut *tx)
                .await?;
            sqlx::query("UPDATE work_items SET state='outcome_unknown' WHERE id=?")
                .bind(row.get::<String, _>("work_item_id"))
                .execute(&mut *tx)
                .await?;
            sqlx::query("UPDATE cycles SET state='blocked' WHERE id=? AND state NOT IN ('closed','cancelled')").bind(row.get::<String,_>("cycle_id")).execute(&mut *tx).await?;
            sqlx::query("UPDATE loops SET state='blocked',version=version+1,updated_at=? WHERE id=? AND state='active'").bind(at).bind(&loop_id).execute(&mut *tx).await?;
            Self::event(&mut tx,Some(&loop_id),"execution_uncertain",json!({"step_id":step_id,"reason":if interrupted {"daemon restarted; reconcile execution before retry"} else {"lease expired; reconcile process and external actions before retry"}}),true).await?;
        }
        tx.commit().await?;
        Ok(rows.len())
    }

    pub async fn step_status(&self, step_id: &str) -> Result<Value> {
        let row = sqlx::query(
            "SELECT state,attempt,lease_generation,result,workspace_id FROM step_runs WHERE id=?",
        )
        .bind(step_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(
            json!({"state":row.get::<String,_>("state"),"attempt":row.get::<i64,_>("attempt"),"generation":row.get::<i64,_>("lease_generation"),"workspace_id":row.get::<Option<String>,_>("workspace_id"),"result":row.get::<Option<String>,_>("result").map(|s|serde_json::from_str::<Value>(&s)).transpose()?}),
        )
    }
}
