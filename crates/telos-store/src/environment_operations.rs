//! Durable intent precedes external coordination; capabilities never enter the ledger.
use super::*;
#[derive(Debug, Clone)]
pub struct EnvironmentIntent {
    pub loop_id: String,
    pub revision: i64,
    pub adapter_key: String,
    pub adapter_version: i64,
    pub verification_lease_id: Option<String>,
    pub environment_sha256: String,
    pub adapter_sha256: String,
    pub session_id: String,
    pub request_key: String,
    pub action: String,
    pub expected_version: i64,
}
impl Store {
    pub async fn begin_environment_operation(&self, intent: &EnvironmentIntent) -> Result<Value> {
        ensure!(
            matches!(
                intent.action.as_str(),
                "register" | "acquire" | "heartbeat" | "release"
            ),
            "unsupported environment action"
        );
        ensure!(
            (0..i64::MAX).contains(&intent.expected_version),
            "invalid environment operation version"
        );
        for hash in [&intent.environment_sha256, &intent.adapter_sha256] {
            ensure!(
                hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()),
                "invalid environment binding"
            )
        }
        for key in [&intent.session_id, &intent.request_key] {
            ensure!(
                !key.is_empty() && key.len() <= 128 && !key.chars().any(char::is_control),
                "invalid environment operation key"
            )
        }
        validate_adapter_key(&intent.adapter_key)?;
        ensure!(intent.adapter_version > 0, "adapter grant version required");
        let mut tx = self.pool.begin().await?;
        let goal = Self::get_in(&mut tx, &intent.loop_id).await?;
        ensure!(
            ((goal.revision == intent.revision && goal.state == LoopState::Active)
                || intent.action == "release") && goal.kind == LoopKind::Delivery,
            "environment operations require current active delivery goal or exact-session release cleanup"
        );
        Self::require_recovery_clear(&mut tx, &goal.project_id).await?;
        if intent.action != "release" {
            Self::require_environment_certain(&mut tx, &goal.project_id).await?;
        }
        let busy: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM verification_leases WHERE project_id=? AND state IN ('running','outcome_unknown')")
            .bind(&goal.project_id).fetch_one(&mut *tx).await?;
        if let Some(lease) = &intent.verification_lease_id {
            let owned: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM verification_leases WHERE id=? AND project_id=? AND loop_id=? AND contract_revision=? AND state='running'")
                .bind(lease).bind(&goal.project_id).bind(&intent.loop_id).bind(intent.revision).fetch_one(&mut *tx).await?;
            ensure!(
                owned == 1 && busy == 1,
                "environment operation requires its exact running verification owner"
            );
        } else {
            ensure!(
                busy == 0,
                "verification must be idle for an unbound environment operation"
            );
        }

        let enabled:i64=sqlx::query_scalar("SELECT COUNT(*) FROM project_execution_policies WHERE project_id=? AND json_extract(body,'$.enabled')=1 AND json_extract(body,'$.sandbox')='workspace_write'").bind(&goal.project_id).fetch_one(&mut *tx).await?;
        ensure!(
            enabled == 1 || intent.action == "release",
            "existing operator write policy required; environment adapter grant is separate"
        );
        let mut grant_sequence = None;
        if intent.action != "release" {
            let grant =
                Self::environment_adapter_in(&mut tx, &goal.project_id, &intent.adapter_key)
                    .await?
                    .context("explicit environment adapter grant required")?;
            let preparation: Option<i64> = sqlx::query_scalar(
                "SELECT MAX(sequence) FROM events WHERE kind='recovery_prepared'",
            )
            .fetch_one(&mut *tx)
            .await?;
            ensure!(grant["config"]["enabled"] == true
                && grant["version"].as_i64() == Some(intent.adapter_version)
                && grant["environment_sha256"] == intent.environment_sha256
                && grant["adapter_sha256"] == intent.adapter_sha256
                && preparation.is_none_or(|sequence| grant["event_sequence"].as_i64().is_some_and(|s| s > sequence)),
                "disabled, stale or mismatched environment adapter grant; restore requires reauthorization");
            grant_sequence = grant["event_sequence"].as_i64();
        }
        let prior:Option<String>=sqlx::query_scalar("SELECT body FROM events WHERE kind IN ('environment_operation_intent','environment_operation_outcome','environment_operation_reconciled','environment_session_uncertain') AND json_extract(body,'$.session_id')=? ORDER BY sequence DESC LIMIT 1").bind(&intent.session_id).fetch_optional(&mut *tx).await?;
        let mut state = "new".to_owned();
        let mut version = 0;
        if let Some(prior) = prior {
            let prior: Value = serde_json::from_str(&prior)?;
            ensure!(
                prior["loop_id"] == intent.loop_id
                    && prior["revision"].as_i64() == Some(intent.revision)
                    && prior["environment_sha256"] == intent.environment_sha256
                    && prior["adapter_sha256"] == intent.adapter_sha256
                    && prior["adapter_key"] == intent.adapter_key
                    && prior["adapter_version"].as_i64() == Some(intent.adapter_version)
                    && prior["verification_lease_id"] == json!(intent.verification_lease_id),
                "environment session binding changed"
            );
            if intent.action == "release" {
                grant_sequence = prior["adapter_grant_sequence"].as_i64();
            }
            state = prior["phase"]
                .as_str()
                .context("invalid environment phase")?
                .into();
            version = prior["version"]
                .as_i64()
                .context("missing environment version")?;
        }
        ensure!(
            version == intent.expected_version,
            "environment session version conflict"
        );
        ensure!(
            matches!(
                (state.as_str(), intent.action.as_str()),
                ("new", "register")
                    | ("registered", "acquire")
                    | ("acquired", "heartbeat")
                    | ("acquired", "release")
            ),
            "pending or unknown environment action cannot be repeated"
        );
        let used:i64=sqlx::query_scalar("SELECT COUNT(*) FROM events WHERE kind='environment_operation_intent' AND json_extract(body,'$.session_id')=? AND json_extract(body,'$.request_key')=?").bind(&intent.session_id).bind(&intent.request_key).fetch_one(&mut *tx).await?;
        ensure!(
            used == 0,
            "environment mutation request already exists; inspect its receipt"
        );
        let occupied:i64=sqlx::query_scalar("SELECT COUNT(*) FROM events e WHERE e.kind IN ('environment_operation_intent','environment_operation_outcome','environment_operation_reconciled','environment_session_uncertain') AND json_extract(e.body,'$.environment_sha256')=? AND json_extract(e.body,'$.session_id')<>? AND json_extract(e.body,'$.phase') IN ('pending','outcome_unknown','acquired') AND e.sequence=(SELECT MAX(s.sequence) FROM events s WHERE s.kind IN ('environment_operation_intent','environment_operation_outcome','environment_operation_reconciled','environment_session_uncertain') AND json_extract(s.body,'$.session_id')=json_extract(e.body,'$.session_id'))").bind(&intent.environment_sha256).bind(&intent.session_id).fetch_one(&mut *tx).await?;
        ensure!(
            occupied == 0 || intent.action == "release",
            "another local session owns or may own environment; reconcile first"
        );
        let body = json!({"loop_id":intent.loop_id,"revision":intent.revision,"adapter_key":intent.adapter_key,"adapter_version":intent.adapter_version,"verification_lease_id":intent.verification_lease_id,"adapter_grant_sequence":grant_sequence,"environment_sha256":intent.environment_sha256,"adapter_sha256":intent.adapter_sha256,"session_id":intent.session_id,"request_key":intent.request_key,"action":intent.action,"previous_phase":state,"phase":"pending","version":version+1});
        Self::event(
            &mut tx,
            Some(&intent.loop_id),
            "environment_operation_intent",
            body.clone(),
            false,
        )
        .await?;
        let sequence: i64 = sqlx::query_scalar("SELECT last_insert_rowid()")
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        let mut receipt = body;
        receipt["intent_sequence"] = json!(sequence);
        Ok(receipt)
    }
    pub async fn finish_environment_operation(
        &self,
        intent_sequence: i64,
        phase: &str,
    ) -> Result<Value> {
        let mut tx = self.pool.begin().await?;
        let body: String = sqlx::query_scalar(
            "SELECT body FROM events WHERE sequence=? AND kind='environment_operation_intent'",
        )
        .bind(intent_sequence)
        .fetch_optional(&mut *tx)
        .await?
        .context("environment intent missing")?;
        let mut body: Value = serde_json::from_str(&body)?;
        let action = body["action"]
            .as_str()
            .context("missing environment action")?;
        ensure!(
            phase == "outcome_unknown"
                || matches!(
                    (action, phase),
                    ("register", "registered")
                        | ("acquire", "acquired")
                        | ("heartbeat", "acquired")
                        | ("release", "released")
                ),
            "invalid environment outcome"
        );
        let latest=sqlx::query("SELECT sequence,kind,body FROM events WHERE kind IN ('environment_operation_intent','environment_operation_outcome','environment_operation_reconciled','environment_session_uncertain') AND json_extract(body,'$.session_id')=? ORDER BY sequence DESC LIMIT 1").bind(body["session_id"].as_str()).fetch_one(&mut *tx).await?;
        if latest.get::<&str, _>("kind") == "environment_operation_outcome" {
            let old: Value = serde_json::from_str(latest.get::<&str, _>("body"))?;
            ensure!(
                old["intent_sequence"].as_i64() == Some(intent_sequence) && old["phase"] == phase,
                "conflicting environment receipt"
            );
            tx.commit().await?;
            return Ok(old);
        }
        ensure!(
            latest.get::<i64, _>("sequence") == intent_sequence,
            "stale environment operation completion"
        );
        body["phase"] = json!(phase);
        body["intent_sequence"] = json!(intent_sequence);
        let loop_id = body["loop_id"]
            .as_str()
            .context("missing environment goal")?
            .to_owned();
        // Cleanup outcomes must remain recordable even if the goal was paused/revised.
        Self::event(
            &mut tx,
            Some(&loop_id),
            "environment_operation_outcome",
            body.clone(),
            false,
        )
        .await?;
        tx.commit().await?;
        Ok(body)
    }
}

pub(crate) const LATEST_ENVIRONMENT:&str="SELECT e.sequence,e.loop_id,e.body,l.project_id FROM events e JOIN loops l ON l.id=e.loop_id WHERE e.kind IN ('environment_operation_intent','environment_operation_outcome','environment_operation_reconciled','environment_session_uncertain') AND e.sequence=(SELECT MAX(s.sequence) FROM events s WHERE s.kind IN ('environment_operation_intent','environment_operation_outcome','environment_operation_reconciled','environment_session_uncertain') AND json_extract(s.body,'$.session_id')=json_extract(e.body,'$.session_id'))";
impl Store {
    pub(crate) async fn require_environment_certain(
        tx: &mut Transaction<'_, Sqlite>,
        project: &str,
    ) -> Result<()> {
        let count:i64=sqlx::query_scalar(&format!("SELECT COUNT(*) FROM ({LATEST_ENVIRONMENT}) e WHERE project_id=? AND json_extract(body,'$.phase') IN ('pending','outcome_unknown')")).bind(project).fetch_one(&mut **tx).await?;
        ensure!(
            count == 0,
            "project has pending or unknown external environment operation"
        );
        Ok(())
    }
    pub(crate) async fn require_environment_settled(
        tx: &mut Transaction<'_, Sqlite>,
        project: &str,
    ) -> Result<()> {
        let count:i64=sqlx::query_scalar(&format!("SELECT COUNT(*) FROM ({LATEST_ENVIRONMENT}) e WHERE project_id=? AND json_extract(body,'$.phase') IN ('pending','outcome_unknown','acquired')")).bind(project).fetch_one(&mut **tx).await?;
        ensure!(
            count == 0,
            "external environment ownership must be reconciled before project recovery"
        );
        Ok(())
    }
    pub async fn environment_status(&self, loop_id: &str) -> Result<Value> {
        let mut tx = self.pool.begin().await?;
        let goal = Self::get_in(&mut tx, loop_id).await?;
        let count:i64=sqlx::query_scalar(&format!("SELECT COUNT(*) FROM ({LATEST_ENVIRONMENT}) e WHERE project_id=? AND json_extract(body,'$.phase') IN ('pending','outcome_unknown')")).bind(&goal.project_id).fetch_one(&mut *tx).await?;
        let owned:i64=sqlx::query_scalar(&format!("SELECT COUNT(*) FROM ({LATEST_ENVIRONMENT}) e WHERE project_id=? AND json_extract(body,'$.phase') IN ('pending','outcome_unknown','acquired')")).bind(&goal.project_id).fetch_one(&mut *tx).await?;
        let rows=sqlx::query(&format!("SELECT sequence,loop_id,body FROM ({LATEST_ENVIRONMENT}) e WHERE project_id=? AND json_extract(body,'$.phase') NOT IN ('released','retired') ORDER BY sequence DESC LIMIT 17")).bind(&goal.project_id).fetch_all(&mut *tx).await?;
        let more = rows.len() > 16;
        let items:Vec<Value>=rows.into_iter().take(16).map(|r|->Result<Value>{let b:Value=serde_json::from_str(r.get::<&str,_>("body"))?;Ok(json!({"event_sequence":r.get::<i64,_>("sequence"),"loop_id":r.get::<String,_>("loop_id"),"revision":b["revision"],"session_id":b["session_id"],"environment_sha256":b["environment_sha256"],"adapter_sha256":b["adapter_sha256"],"action":b["action"],"phase":b["phase"],"version":b["version"],"adapter_key":b["adapter_key"],"adapter_version":b["adapter_version"],"adapter_grant_sequence":b["adapter_grant_sequence"],"verification_lease_id":b["verification_lease_id"],"uncertainty_reason":b["uncertainty_reason"]}))}).collect::<Result<_>>()?;
        tx.commit().await?;
        Ok(
            json!({"items":items,"more":more,"limit":16,"uncertain_session_count":count,"sessions_requiring_recovery_review":owned,"new_execution_blocked":count>0,"scope":"Local durable observations only, not live provider status or permission to release/take over. Includes other goals in this project; elapsed time never clears ownership."}),
        )
    }
}

impl Store {
    /// Trusted operator only. Never expose this through ordinary MCP tools.
    /// Retires a local fence after review, without mutating the remote provider.
    pub async fn reconcile_environment(
        &self,
        review: &telos_core::EnvironmentReconciliation,
    ) -> Result<Value> {
        self.reconcile_environment_review(review, None).await
    }

    pub async fn reconcile_recovered_environment(
        &self,
        disposition: &telos_core::RecoveryEnvironmentDisposition,
    ) -> Result<Value> {
        disposition.validate()?;
        self.reconcile_environment_review(&disposition.session, Some(&disposition.review))
            .await
    }

    async fn reconcile_environment_review(
        &self,
        review: &telos_core::EnvironmentReconciliation,
        recovery: Option<&telos_core::RecoveryProjectReview>,
    ) -> Result<Value> {
        review.validate()?;
        let recovery_proof = serde_json::to_value(recovery)?;
        let proof = serde_json::to_value(review)?;
        let hash = format!("{:x}", Sha256::digest(serde_json::to_vec(&proof)?));
        let mut tx = self.pool.begin().await?;
        let previous:Option<String>=sqlx::query_scalar("SELECT body FROM events WHERE kind='environment_operation_reconciled' AND json_extract(body,'$.review.decision_id')=? ORDER BY sequence DESC LIMIT 1").bind(&review.decision_id).fetch_optional(&mut *tx).await?;
        if let Some(previous) = previous {
            let previous: Value = serde_json::from_str(&previous)?;
            ensure!(
                previous["review_sha256"] == hash
                    && previous["review"] == proof
                    && previous["recovery_review"] == recovery_proof,
                "environment decision reused with another review"
            );
            tx.commit().await?;
            return Ok(previous);
        }
        let at = now();
        ensure!(
            review.provider_observed_at >= at.saturating_sub(300)
                && review.provider_observed_at <= at,
            "provider review must be recent and not future-dated"
        );
        let goal = Self::get_in(&mut tx, &review.loop_id).await?;
        if let Some(recovery) = recovery {
            ensure!(
                goal.project_id == recovery.project_id,
                "environment review belongs to another project"
            );
            let held: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM recovery_project_holds h JOIN recovery_preparations p ON p.singleton=1 AND p.manifest_sha256=h.manifest_sha256 WHERE h.project_id=? AND h.manifest_sha256=? AND h.state='held'")
                .bind(&recovery.project_id).bind(&recovery.manifest_sha256).fetch_one(&mut *tx).await?;
            ensure!(held == 1, "matching prepared project hold required");
            self.require_recovery_review_material(&mut tx, recovery)
                .await?;
        }
        ensure!(
            matches!(
                goal.state,
                LoopState::Paused | LoopState::Stopped | LoopState::Completed
            ),
            "pause environment owner goal before operator reconciliation"
        );
        let row=sqlx::query("SELECT sequence,loop_id,body FROM events WHERE kind IN ('environment_operation_intent','environment_operation_outcome','environment_operation_reconciled','environment_session_uncertain') AND json_extract(body,'$.session_id')=? ORDER BY sequence DESC LIMIT 1").bind(&review.session_id).fetch_optional(&mut *tx).await?.context("environment session missing")?;
        let mut body: Value = serde_json::from_str(row.get::<&str, _>("body"))?;
        ensure!(
            row.get::<i64, _>("sequence") == review.expected_sequence
                && row.get::<&str, _>("loop_id") == review.loop_id
                && body["revision"].as_i64() == Some(review.revision)
                && body["version"].as_i64() == Some(review.expected_version),
            "stale environment review"
        );
        ensure!(
            body["environment_sha256"] == review.environment_sha256
                && body["adapter_sha256"] == review.adapter_sha256,
            "environment review binding changed"
        );
        ensure!(
            matches!(
                body["phase"].as_str(),
                Some("pending" | "outcome_unknown" | "acquired")
            ),
            "environment session already settled or does not need retirement"
        );
        body["previous_phase"] = body["phase"].clone();
        body["phase"] = json!("retired");
        body["version"] = json!(review.expected_version + 1);
        body["review"] = proof;
        if recovery.is_some() {
            body["recovery_review"] = recovery_proof;
        }
        body["review_sha256"] = json!(hash);
        body["scope"]=json!("Trusted operator attestation retiring local ownership fence only; remote state was not independently inspected or changed by Telos. No execution success, lease release, goal resume or provider authority is implied.");
        Self::event(
            &mut tx,
            Some(&review.loop_id),
            "environment_operation_reconciled",
            body.clone(),
            false,
        )
        .await?;
        tx.commit().await?;
        Ok(body)
    }
}

fn validate_adapter_key(key: &str) -> Result<()> {
    ensure!(
        !key.is_empty()
            && key.len() <= 128
            && key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b)),
        "invalid environment adapter key"
    );
    Ok(())
}
impl Store {
    async fn environment_adapter_in(
        tx: &mut Transaction<'_, Sqlite>,
        project: &str,
        key: &str,
    ) -> Result<Option<Value>> {
        let row = sqlx::query("SELECT sequence,body FROM events WHERE kind='environment_adapter_configured' AND json_extract(body,'$.project_id')=? AND json_extract(body,'$.adapter_key')=? ORDER BY sequence DESC LIMIT 1")
            .bind(project).bind(key).fetch_optional(&mut **tx).await?;
        row.map(|row| -> Result<Value> {
            let mut body: Value = serde_json::from_str(row.get::<&str, _>("body"))?;
            body["event_sequence"] = json!(row.get::<i64, _>("sequence"));
            Ok(body)
        })
        .transpose()
    }
    pub async fn environment_adapter(&self, project: &str, key: &str) -> Result<Option<Value>> {
        validate_adapter_key(key)?;
        let mut tx = self.pool.begin().await?;
        let mut result = Self::environment_adapter_in(&mut tx, project, key).await?;
        if let Some(grant) = &mut result {
            let preparation: Option<i64> = sqlx::query_scalar(
                "SELECT MAX(sequence) FROM events WHERE kind='recovery_prepared'",
            )
            .fetch_one(&mut *tx)
            .await?;
            grant["requires_reauthorization"] = json!(preparation
                .is_some_and(|sequence| grant["event_sequence"]
                    .as_i64()
                    .is_none_or(|s| s <= sequence)));
        }
        tx.commit().await?;
        Ok(result)
    }
}

impl Store {
    /// Runtime bookkeeping only. No provider command, acceptance or permission is implied.
    pub async fn fence_environment_session(
        &self,
        loop_id: &str,
        revision: i64,
        session: &str,
        expected_version: i64,
        reason: &str,
    ) -> Result<Value> {
        ensure!(
            matches!(
                reason,
                "heartbeat_failed" | "cancelled" | "deadline" | "operation_failed"
            ),
            "unsupported environment uncertainty reason"
        );
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query(&format!("SELECT sequence,body FROM ({LATEST_ENVIRONMENT}) e WHERE loop_id=? AND json_extract(body,'$.session_id')=?"))
            .bind(loop_id).bind(session).fetch_optional(&mut *tx).await?.context("environment session missing")?;
        let mut body: Value = serde_json::from_str(row.get::<&str, _>("body"))?;
        ensure!(
            body["revision"].as_i64() == Some(revision),
            "environment session revision changed"
        );
        // Pending/unknown already fence project execution, including a lost intent receipt.
        if matches!(body["phase"].as_str(), Some("pending" | "outcome_unknown")) {
            tx.commit().await?;
            return Ok(body);
        }
        ensure!(
            body["phase"] == "acquired"
                && body["version"].as_i64() == Some(expected_version)
                && expected_version > 0
                && expected_version < i64::MAX,
            "stale or settled environment session cannot be fenced"
        );
        body["previous_phase"] = body["phase"].clone();
        body["previous_sequence"] = json!(row.get::<i64, _>("sequence"));
        body["phase"] = json!("outcome_unknown");
        body["version"] = json!(expected_version + 1);
        body["uncertainty_reason"] = json!(reason);
        body["scope"] = json!("Runtime operation or maintenance uncertainty; provider ownership retained. No remote stop, release or test success is asserted.");
        Self::event(
            &mut tx,
            Some(loop_id),
            "environment_session_uncertain",
            body.clone(),
            false,
        )
        .await?;
        tx.commit().await?;
        Ok(body)
    }
}

impl Store {
    /// Trusted embedding/operator code only. Records authority; never invokes a provider.
    pub async fn configure_environment_adapter(
        &self,
        project: &str,
        key: &str,
        expected_version: i64,
        grant: &telos_core::EnvironmentAdapterGrant,
    ) -> Result<Value> {
        validate_adapter_key(key)?;
        grant.validate()?;
        ensure!(
            (0..i64::MAX).contains(&expected_version),
            "invalid adapter version"
        );
        let mut tx = self.pool.begin().await?;
        let exists: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM projects WHERE id=?")
            .bind(project)
            .fetch_one(&mut *tx)
            .await?;
        ensure!(exists == 1, "project not registered");
        let old = Self::environment_adapter_in(&mut tx, project, key).await?;
        ensure!(
            old.as_ref()
                .map_or(0, |v| v["version"].as_i64().unwrap_or(-1))
                == expected_version,
            "environment adapter version conflict"
        );
        if grant.enabled {
            Self::require_verification_idle(&mut tx, project).await?;
            Self::require_environment_settled(&mut tx, project).await?;
            let allowed: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM project_execution_policies WHERE project_id=? AND json_extract(body,'$.enabled')=1 AND json_extract(body,'$.sandbox')='workspace_write'")
                .bind(project).fetch_one(&mut *tx).await?;
            ensure!(allowed == 1, "existing operator write policy required");
        } else {
            let old = old
                .as_ref()
                .context("adapter must exist before revocation")?;
            ensure!(
                old["environment_sha256"] == grant.environment_sha256
                    && old["adapter_sha256"] == grant.adapter_sha256,
                "revocation cannot change adapter identity"
            );
        }
        let body = json!({"project_id":project,"adapter_key":key,"version":expected_version+1,
            "config":{"enabled":grant.enabled},"environment_sha256":grant.environment_sha256,
            "adapter_sha256":grant.adapter_sha256});
        Self::event(
            &mut tx,
            None,
            "environment_adapter_configured",
            body.clone(),
            false,
        )
        .await?;
        tx.commit().await?;
        Ok(body)
    }
}
