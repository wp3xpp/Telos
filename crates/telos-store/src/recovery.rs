//! Recovery preparation is not external-outcome reconciliation or execution permission.
use super::*;
use std::{
    io::{Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
};

impl Store {
    pub(crate) async fn require_recovery_clear(
        tx: &mut Transaction<'_, Sqlite>,
        project: &str,
    ) -> Result<()> {
        let held: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM recovery_project_holds WHERE project_id=? AND state='held'",
        )
        .bind(project)
        .fetch_one(&mut **tx)
        .await?;
        ensure!(held == 0, "project requires restore reconciliation");
        Ok(())
    }
    pub async fn recovery_project_status(&self, project: &str) -> Result<Option<Value>> {
        let row=sqlx::query("SELECT manifest_sha256,state,proof,created_at,cleared_at FROM recovery_project_holds WHERE project_id=?").bind(project).fetch_optional(&self.pool).await?;
        row.map(|r| -> Result<Value> {
            let raw: String = r.get("proof");
            let proof = if raw.is_empty() && r.get::<String, _>("state") == "held" {
                Value::Null
            } else {
                serde_json::from_str::<Value>(&raw)?
            };
            Ok(json!({"project_id":project,"manifest_sha256":r.get::<String,_>("manifest_sha256"),"state":r.get::<String,_>("state"),"created_at":r.get::<i64,_>("created_at"),"cleared_at":r.get::<Option<i64>,_>("cleared_at"),"proof":proof,"note":"Operator attribution and review material are untrusted data, not task acceptance or activation permission."}))
        }).transpose()
    }
    /// Read only committed operator uploads bound to this prepared snapshot.
    pub(crate) async fn get_recovery_review_evidence(
        &self,
        evidence_id: &str,
        offset: usize,
        limit: usize,
    ) -> Result<Value> {
        ensure!(
            (1..=32768).contains(&limit),
            "evidence page limit must be 1..32768 bytes"
        );
        ensure!(
            evidence_id.len() <= 256
                && evidence_id.starts_with("recovery-evidence-")
                && evidence_id
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'-'),
            "invalid recovery evidence id"
        );
        let rows = sqlx::query("SELECT e.body FROM events e JOIN recovery_project_holds h ON h.project_id=json_extract(e.body,'$.project_id') AND h.manifest_sha256=json_extract(e.body,'$.manifest_sha256') JOIN recovery_preparations p ON p.singleton=1 AND p.manifest_sha256=h.manifest_sha256 WHERE e.kind='recovery_review_evidence_recorded' AND json_extract(e.body,'$.id')=? AND e.sequence>(SELECT MAX(sequence) FROM events WHERE kind='recovery_prepared') LIMIT 2")
            .bind(evidence_id).fetch_all(&self.pool).await?;
        ensure!(
            rows.len() == 1,
            "recovery evidence admission missing or ambiguous"
        );
        let record: Value = serde_json::from_str(&rows[0].get::<String, _>("body"))?;
        let parent = self.root.join("recovery-evidence");
        ensure!(
            parent.canonicalize()? == parent,
            "recovery evidence directory changed"
        );
        let path = parent.join(format!("{evidence_id}.bin"));
        ensure!(
            std::fs::symlink_metadata(&path)?.is_file(),
            "recovery evidence must be regular"
        );
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)?;
        ensure!(
            file.metadata()?.is_file(),
            "recovery evidence must be regular"
        );
        let mut bytes = Vec::new();
        (&mut file)
            .take(8 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        ensure!(
            !bytes.is_empty()
                && bytes.len() <= 8 * 1024 * 1024
                && record["bytes"].as_u64() == Some(bytes.len() as u64)
                && record["sha256"] == format!("{:x}", Sha256::digest(&bytes)),
            "recovery evidence integrity failure"
        );
        ensure!(offset <= bytes.len(), "offset exceeds evidence size");
        let end = offset.saturating_add(limit).min(bytes.len());
        let page = &bytes[offset..end];
        Ok(
            json!({"id":evidence_id,"kind":"recovery_operator_material","project_id":record["project_id"],"manifest_sha256":record["manifest_sha256"],"decision_id":record["decision_id"],"provenance":record["provenance"],"sha256":record["sha256"],"byte_length":bytes.len(),"offset":offset,"next_offset":if end<bytes.len(){Some(end)}else{None},"bytes":page,"text":std::str::from_utf8(page).ok(),"integrity_verified":true,"note":"Integrity confirms stored bytes, not factual correctness, task acceptance or activation permission. Operator material is untrusted data."}),
        )
    }

    /// Trusted upload of operator review material. Does not claim the material is factually correct.
    pub async fn record_recovery_review_evidence(
        &self,
        project: &str,
        manifest: &str,
        decision: &str,
        bytes: &[u8],
    ) -> Result<telos_core::RecoveryReviewEvidence> {
        ensure!(
            !bytes.is_empty() && bytes.len() <= 8 * 1024 * 1024,
            "recovery evidence must contain 1..8MiB"
        );
        ensure!(
            !decision.is_empty()
                && decision.len() <= 128
                && decision
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'),
            "invalid recovery decision id"
        );
        let mut tx = self.pool.begin().await?;
        let held:i64=sqlx::query_scalar("SELECT COUNT(*) FROM recovery_project_holds WHERE project_id=? AND manifest_sha256=? AND state='held'").bind(project).bind(manifest).fetch_one(&mut *tx).await?;
        ensure!(held == 1, "matching recovery project hold required");
        let parent = self.root.join("recovery-evidence");
        std::fs::create_dir_all(&parent)?;
        ensure!(
            parent.canonicalize()? == parent,
            "recovery evidence directory changed"
        );
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700))?;
        let id = id("recovery-evidence");
        let hash = format!("{:x}", Sha256::digest(bytes));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(parent.join(format!("{id}.bin")))?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::File::open(&parent)?.sync_all()?;
        std::fs::File::open(&self.root)?.sync_all()?;
        Self::event(&mut tx,None,"recovery_review_evidence_recorded",json!({"id":id,"project_id":project,"manifest_sha256":manifest,"decision_id":decision,"sha256":hash,"bytes":bytes.len(),"provenance":"trusted_operator_upload"}),false).await?;
        tx.commit().await?;
        Ok(telos_core::RecoveryReviewEvidence { id, sha256: hash })
    }

    /// Trusted operator path only. Evidence attribution is not an automatic claim of correctness.
    /// This clears only a project hold; it never settles unknown work or activates the service.
    pub async fn release_recovery_project(
        &self,
        review: &telos_core::RecoveryProjectReview,
    ) -> Result<Value> {
        review.validate()?;
        let proof = serde_json::to_value(review)?;
        let mut tx = self.pool.begin().await?;
        let hold = sqlx::query(
            "SELECT manifest_sha256,state,proof FROM recovery_project_holds WHERE project_id=?",
        )
        .bind(&review.project_id)
        .fetch_optional(&mut *tx)
        .await?
        .context("recovery project hold missing")?;
        ensure!(
            hold.get::<String, _>("manifest_sha256") == review.manifest_sha256,
            "recovery snapshot changed"
        );
        if hold.get::<String, _>("state") == "cleared" {
            ensure!(
                serde_json::from_str::<Value>(&hold.get::<String, _>("proof"))? == proof,
                "hold was cleared by a different review"
            );
            tx.commit().await?;
            return Ok(
                json!({"project_id":review.project_id,"state":"cleared","decision_id":review.decision_id,"execution_started":false,"service_activated":false}),
            );
        }
        Self::require_environment_settled(&mut tx, &review.project_id).await?;
        self.require_recovery_review_material(&mut tx, review)
            .await?;
        let unresolved:i64=sqlx::query_scalar("SELECT COUNT(*) FROM step_runs s JOIN work_items w ON w.id=s.work_item_id JOIN cycles c ON c.id=w.cycle_id JOIN loops l ON l.id=c.loop_id WHERE l.project_id=? AND s.state IN ('pending','ready','running','outcome_unknown')")
            .bind(&review.project_id).fetch_one(&mut *tx).await?;
        ensure!(
            unresolved == 0,
            "resolve execution uncertainty separately before clearing project hold"
        );
        let verifiers:i64=sqlx::query_scalar("SELECT COUNT(*) FROM verification_leases WHERE project_id=? AND state IN ('running','outcome_unknown')").bind(&review.project_id).fetch_one(&mut *tx).await?;
        ensure!(
            verifiers == 0,
            "resolve verification uncertainty separately before clearing project hold"
        );
        sqlx::query("UPDATE recovery_project_holds SET state='cleared',proof=?,cleared_at=? WHERE project_id=? AND state='held'").bind(proof.to_string()).bind(now()).bind(&review.project_id).execute(&mut *tx).await?;
        Self::event(&mut tx,None,"recovery_project_reviewed",json!({"project_id":review.project_id,"provenance":"trusted_operator_review","review":proof,"execution_started":false,"service_activated":false,"task_success_asserted":false}),false).await?;
        tx.commit().await?;
        Ok(
            json!({"project_id":review.project_id,"state":"cleared","decision_id":review.decision_id,"execution_started":false,"service_activated":false}),
        )
    }

    pub(crate) async fn require_recovery_review_material(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        review: &telos_core::RecoveryProjectReview,
    ) -> Result<()> {
        let preparation_sequence: i64 =
            sqlx::query_scalar("SELECT MAX(sequence) FROM events WHERE kind='recovery_prepared'")
                .fetch_one(&mut **tx)
                .await?;
        for reference in &review.evidence {
            let rows=sqlx::query("SELECT body FROM events WHERE kind='recovery_review_evidence_recorded' AND sequence>? AND json_extract(body,'$.id')=? AND json_extract(body,'$.sha256')=? AND json_extract(body,'$.project_id')=? AND json_extract(body,'$.manifest_sha256')=? AND json_extract(body,'$.decision_id')=?")
                .bind(preparation_sequence).bind(&reference.id).bind(&reference.sha256).bind(&review.project_id).bind(&review.manifest_sha256).bind(&review.decision_id).fetch_all(&mut **tx).await?;
            ensure!(rows.len()==1,"review evidence must be new, attributed to this decision and bound to this project");
            let recorded: Value = serde_json::from_str(&rows[0].get::<String, _>("body"))?;
            let parent = self.root.join("recovery-evidence");
            ensure!(
                parent.canonicalize()? == parent,
                "recovery evidence directory changed"
            );
            let path = parent.join(format!("{}.bin", reference.id));
            ensure!(
                std::fs::symlink_metadata(&path)?.is_file(),
                "recovery evidence must be regular"
            );
            let mut bytes = Vec::new();
            std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(path)?
                .take(8 * 1024 * 1024 + 1)
                .read_to_end(&mut bytes)?;
            ensure!(
                !bytes.is_empty()
                    && bytes.len() <= 8 * 1024 * 1024
                    && recorded["bytes"].as_u64() == Some(bytes.len() as u64)
                    && format!("{:x}", Sha256::digest(&bytes)) == reference.sha256,
                "recovery evidence integrity failure"
            );
        }
        Ok(())
    }

    /// Trusted offline operator boundary. Does not inspect or terminate external processes.
    /// The operator must independently reconcile external effects before this call.
    pub async fn discard_recovered_executions(
        &self,
        disposition: &telos_core::RecoveryExecutionDisposition,
    ) -> Result<Value> {
        disposition.validate()?;
        let review = &disposition.review;
        let proof = serde_json::to_value(disposition)?;
        let mut tx = self.pool.begin().await?;
        let held: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM recovery_project_holds h JOIN recovery_preparations p ON p.singleton=1 AND p.manifest_sha256=h.manifest_sha256 WHERE h.project_id=? AND h.manifest_sha256=? AND h.state='held'")
            .bind(&review.project_id).bind(&review.manifest_sha256).fetch_one(&mut *tx).await?;
        ensure!(held == 1, "matching prepared project hold required");
        self.require_recovery_review_material(&mut tx, review)
            .await?;
        let prior = sqlx::query("SELECT body FROM events WHERE kind='recovery_executions_discarded' AND json_extract(body,'$.project_id')=? AND json_extract(body,'$.decision_id')=?")
            .bind(&review.project_id).bind(&review.decision_id).fetch_all(&mut *tx).await?;
        let result = json!({"project_id":review.project_id,"decision_id":review.decision_id,"step_ids":disposition.step_ids,"disposition":"discarded","automatic_retry":false,"task_success_asserted":false,"service_activated":false});
        if !prior.is_empty() {
            ensure!(
                prior.len() == 1
                    && serde_json::from_str::<Value>(&prior[0].get::<String, _>("body"))?["proof"]
                        == proof,
                "execution disposition decision already used with different inputs"
            );
            tx.commit().await?;
            return Ok(result);
        }
        for step in &disposition.step_ids {
            let row = sqlx::query("SELECT s.state,s.work_item_id,l.project_id FROM step_runs s JOIN work_items w ON w.id=s.work_item_id JOIN cycles c ON c.id=w.cycle_id JOIN loops l ON l.id=c.loop_id WHERE s.id=?")
                .bind(step).fetch_optional(&mut *tx).await?.context("recovered step missing")?;
            ensure!(
                row.get::<String, _>("project_id") == review.project_id,
                "recovered step belongs to another project"
            );
            ensure!(
                row.get::<String, _>("state") == "outcome_unknown",
                "only unknown recovered execution can be discarded"
            );
            sqlx::query(
                "UPDATE step_runs SET state='cancelled' WHERE id=? AND state='outcome_unknown'",
            )
            .bind(step)
            .execute(&mut *tx)
            .await?;
            sqlx::query("UPDATE work_items SET state='cancelled' WHERE id=? AND NOT EXISTS (SELECT 1 FROM step_runs WHERE work_item_id=work_items.id AND state IN ('pending','ready','running','outcome_unknown'))")
                .bind(row.get::<String,_>("work_item_id")).execute(&mut *tx).await?;
        }
        Self::event(&mut tx, None, "recovery_executions_discarded", json!({"project_id":review.project_id,"decision_id":review.decision_id,"proof":proof,"disposition":"discarded","automatic_retry":false,"task_success_asserted":false,"service_activated":false}), false).await?;
        tx.commit().await?;
        Ok(result)
    }

    /// Trusted offline operator boundary. Does not inspect or terminate external processes.
    /// The operator must independently reconcile external effects before this call.
    pub async fn discard_recovered_verifications(
        &self,
        disposition: &telos_core::RecoveryVerificationDisposition,
    ) -> Result<Value> {
        disposition.validate()?;
        let review = &disposition.review;
        let proof = serde_json::to_value(disposition)?;
        let mut tx = self.pool.begin().await?;
        let held: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM recovery_project_holds h JOIN recovery_preparations p ON p.singleton=1 AND p.manifest_sha256=h.manifest_sha256 WHERE h.project_id=? AND h.manifest_sha256=? AND h.state='held'")
            .bind(&review.project_id).bind(&review.manifest_sha256).fetch_one(&mut *tx).await?;
        ensure!(held == 1, "matching prepared project hold required");
        self.require_recovery_review_material(&mut tx, review)
            .await?;
        let prior = sqlx::query("SELECT body FROM events WHERE kind='recovery_verifications_discarded' AND json_extract(body,'$.project_id')=? AND json_extract(body,'$.decision_id')=?")
            .bind(&review.project_id).bind(&review.decision_id).fetch_all(&mut *tx).await?;
        let result = json!({"project_id":review.project_id,"decision_id":review.decision_id,"lease_ids":disposition.lease_ids,"disposition":"discarded","automatic_retry":false,"task_success_asserted":false,"service_activated":false});
        if !prior.is_empty() {
            ensure!(
                prior.len() == 1
                    && serde_json::from_str::<Value>(&prior[0].get::<String, _>("body"))?["proof"]
                        == proof,
                "verification disposition decision already used with different inputs"
            );
            tx.commit().await?;
            return Ok(result);
        }
        for lease in &disposition.lease_ids {
            let row = sqlx::query("SELECT project_id,state FROM verification_leases WHERE id=?")
                .bind(lease)
                .fetch_optional(&mut *tx)
                .await?
                .context("recovered verification lease missing")?;
            ensure!(
                row.get::<String, _>("project_id") == review.project_id,
                "verification lease belongs to another project"
            );
            ensure!(
                row.get::<String, _>("state") == "outcome_unknown",
                "only unknown recovered verification can be discarded"
            );
            sqlx::query("UPDATE verification_leases SET state='finished',finished_at=? WHERE id=? AND state='outcome_unknown'")
                .bind(now()).bind(lease).execute(&mut *tx).await?;
        }
        Self::event(&mut tx, None, "recovery_verifications_discarded", json!({"project_id":review.project_id,"decision_id":review.decision_id,"proof":proof,"disposition":"discarded","automatic_retry":false,"task_success_asserted":false,"service_activated":false}), false).await?;
        tx.commit().await?;
        Ok(result)
    }

    /// Read-only maintenance context. Does not establish external worktree identity.
    pub async fn recovery_workspace_context(&self, id: &str) -> Result<Value> {
        let row = sqlx::query("SELECT d.* FROM delivery_workspaces d WHERE d.id=?")
            .bind(id)
            .fetch_one(&self.pool)
            .await?;
        let workspace = crate::workspaces::record(row);
        let row = sqlx::query("SELECT c.loop_id,l.project_id,p.root FROM cycles c JOIN loops l ON l.id=c.loop_id JOIN projects p ON p.id=l.project_id WHERE c.id=?")
            .bind(&workspace.cycle_id).fetch_one(&self.pool).await?;
        let loop_id: String = row.get("loop_id");
        let plan = self
            .verification_plan(&loop_id, workspace.contract_revision)
            .await?;
        Ok(
            json!({"workspace":workspace,"loop_id":loop_id,"project_id":row.get::<String,_>("project_id"),"project_root":row.get::<String,_>("root"),"verification_plan":plan}),
        )
    }

    /// Internal runtime path after reciprocal Git identity and pinned snapshot checks.
    /// Restores only a binding, invalidates old verification versions, and keeps all gates.
    pub async fn adopt_recovered_workspace(
        &self,
        adoption: &telos_core::RecoveryWorkspaceAdoption,
        expected: &telos_core::DeliveryWorkspace,
        observed: &Value,
    ) -> Result<Value> {
        adoption.validate()?;
        ensure!(
            adoption.workspace_id == expected.id,
            "workspace binding mismatch"
        );
        let bytes = serde_json::to_vec(observed)?;
        ensure!(
            bytes.len() <= 4 * 1024 * 1024
                && format!("{:x}", Sha256::digest(&bytes)) == adoption.expected_snapshot_sha256
                && observed["workspace_id"] == expected.id
                && observed["base_commit"] == expected.base_commit,
            "workspace observation differs from reviewed fingerprint"
        );
        let review = &adoption.review;
        let mut tx = self.pool.begin().await?;
        let held: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM recovery_project_holds h JOIN recovery_preparations p ON p.singleton=1 AND p.manifest_sha256=h.manifest_sha256 WHERE h.project_id=? AND h.manifest_sha256=? AND h.state='held'")
            .bind(&review.project_id).bind(&review.manifest_sha256).fetch_one(&mut *tx).await?;
        ensure!(held == 1, "matching prepared project hold required");
        self.require_recovery_review_material(&mut tx, review)
            .await?;
        let row = sqlx::query("SELECT d.*,c.loop_id,c.state AS cycle_state,l.project_id,l.state AS loop_state,l.kind AS loop_kind,l.revision AS current_revision,p.revision AS current_policy FROM delivery_workspaces d JOIN cycles c ON c.id=d.cycle_id JOIN loops l ON l.id=c.loop_id JOIN project_execution_policies p ON p.project_id=l.project_id WHERE d.id=?")
            .bind(&expected.id).fetch_one(&mut *tx).await?;
        ensure!(
            row.get::<String, _>("project_id") == review.project_id
                && row.get::<String, _>("loop_kind") == "delivery"
                && row.get::<String, _>("loop_state") == "paused"
                && row.get::<String, _>("cycle_state") == "blocked"
                && row.get::<i64, _>("current_revision") == expected.contract_revision
                && row.get::<i64, _>("current_policy") == expected.policy_revision,
            "workspace project, cycle or authority changed"
        );
        let loop_id: String = row.get("loop_id");
        let current = crate::workspaces::record(row);
        ensure!(
            serde_json::to_value(&current)? == serde_json::to_value(expected)?
                && current.state == "uncertain",
            "workspace changed or already reconciled"
        );
        let unfinished: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM step_runs s JOIN work_items w ON w.id=s.work_item_id JOIN cycles c ON c.id=w.cycle_id JOIN loops l ON l.id=c.loop_id WHERE l.project_id=? AND s.state IN ('pending','ready','running','outcome_unknown')")
            .bind(&review.project_id).fetch_one(&mut *tx).await?;
        let verifiers: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM verification_leases WHERE project_id=? AND state IN ('running','outcome_unknown')")
            .bind(&review.project_id).fetch_one(&mut *tx).await?;
        ensure!(
            unfinished == 0 && verifiers == 0,
            "resolve project execution and verification uncertainty before workspace adoption"
        );
        let prior: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM events WHERE kind='recovery_workspace_adopted' AND json_extract(body,'$.project_id')=? AND json_extract(body,'$.decision_id')=?")
            .bind(&review.project_id).bind(&review.decision_id).fetch_one(&mut *tx).await?;
        ensure!(prior == 0, "workspace adoption decision already used");
        sqlx::query("UPDATE delivery_workspaces SET path=?,state='ready',last_error=NULL,updated_at=? WHERE id=?")
            .bind(&adoption.candidate_path).bind(now()).bind(&expected.id).execute(&mut *tx).await?;
        let previous_plan: Option<i64> = sqlx::query_scalar(
            "SELECT version FROM verification_plans WHERE loop_id=? AND contract_revision=?",
        )
        .bind(&loop_id)
        .bind(expected.contract_revision)
        .fetch_optional(&mut *tx)
        .await?;
        let next_plan = previous_plan
            .map(|v| v.checked_add(1).context("verification version overflow"))
            .transpose()?;
        if let Some(version) = next_plan {
            sqlx::query("UPDATE verification_plans SET version=?,updated_at=? WHERE loop_id=? AND contract_revision=?")
                .bind(version).bind(now()).bind(&loop_id).bind(expected.contract_revision).execute(&mut *tx).await?;
        }
        Self::event(&mut tx, Some(&loop_id), "recovery_workspace_adopted", json!({"project_id":review.project_id,"decision_id":review.decision_id,"adoption":adoption,"previous_binding":expected,"snapshot":observed,"execution_started":false,"task_success_asserted":false,"service_activated":false,"previous_plan_version":previous_plan,"next_plan_version":next_plan}), false).await?;
        tx.commit().await?;
        Ok(
            json!({"workspace_id":expected.id,"path":adoption.candidate_path,"state":"ready","execution_started":false,"task_success_asserted":false,"service_activated":false}),
        )
    }

    /// Internal offline archive admission. Files are moved only inside a private gated candidate.
    pub async fn record_recovery_run_archive(
        &self,
        manifest: &str,
        tree_sha256: &str,
        file_count: usize,
    ) -> Result<Value> {
        ensure!(
            tree_sha256.len() == 64
                && tree_sha256.bytes().all(|b| b.is_ascii_hexdigit())
                && file_count <= 100_000,
            "invalid archive inventory"
        );
        let proof = json!({"manifest_sha256":manifest,"source":"runs","destination":"recovery-history/original-runs","tree_sha256":tree_sha256,"file_count":file_count,"process_actions":false,"execution_enabled":false});
        let mut tx = self.pool.begin().await?;
        let preparation: String = sqlx::query_scalar("SELECT manifest_sha256 FROM recovery_preparations WHERE singleton=1 AND state='prepared'").fetch_one(&mut *tx).await?;
        ensure!(
            preparation == manifest,
            "archive snapshot identity mismatch"
        );
        let unresolved: i64 = sqlx::query_scalar("SELECT (SELECT COUNT(*) FROM step_runs WHERE state IN ('pending','ready','running','outcome_unknown')) + (SELECT COUNT(*) FROM verification_leases WHERE state IN ('running','outcome_unknown'))").fetch_one(&mut *tx).await?;
        ensure!(
            unresolved == 0,
            "resolve execution and verification uncertainty before archiving run evidence"
        );
        let existing: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM events WHERE kind='recovery_runs_archived'")
                .fetch_one(&mut *tx)
                .await?;
        ensure!(existing == 0, "historical runs already archived");
        Self::event(
            &mut tx,
            None,
            "recovery_runs_archived",
            proof.clone(),
            false,
        )
        .await?;
        tx.commit().await?;
        Ok(proof)
    }

    /// Trusted offline authorization only. Publication and startup gates are separate.
    pub async fn authorize_recovery_activation(
        &self,
        decision: &telos_core::RecoveryActivation,
        preflight: &Value,
    ) -> Result<Value> {
        decision.validate()?;
        ensure!(
            serde_json::to_vec(preflight)?.len() <= 4 * 1024 * 1024
                && preflight["checks_passed"] == true
                && preflight["source_receipt_sha256"] == decision.source_receipt_sha256
                && preflight["inventory"]["preparation"]["manifest_sha256"]
                    == decision.manifest_sha256
                && preflight["blockers"]
                    .as_array()
                    .is_some_and(|items| items.is_empty()),
            "matching successful preflight required"
        );
        ensure!(
            std::fs::symlink_metadata(self.root.join("runs"))
                .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
            "archive historical run namespace before activation"
        );
        let proof = serde_json::to_value(decision)?;
        let mut tx = self.pool.begin().await?;
        let preparation = sqlx::query("SELECT manifest_sha256,original_state FROM recovery_preparations WHERE singleton=1 AND state='prepared'").fetch_one(&mut *tx).await?;
        ensure!(
            preparation.get::<String, _>("manifest_sha256") == decision.manifest_sha256,
            "activation snapshot mismatch"
        );
        let original: String = preparation.get("original_state");
        ensure!(
            !Path::new(&decision.target_state).starts_with(&original)
                && !Path::new(&original).starts_with(&decision.target_state),
            "activation target must be outside original state"
        );
        let external:i64=sqlx::query_scalar(&format!("SELECT COUNT(*) FROM ({}) e WHERE json_extract(body,'$.phase') IN ('pending','outcome_unknown','acquired')",environment_operations::LATEST_ENVIRONMENT)).fetch_one(&mut *tx).await?;
        ensure!(
            external == 0,
            "external environment ownership must be reconciled before activation"
        );
        for sql in [
            "SELECT COUNT(*) FROM projects p LEFT JOIN recovery_project_holds h ON h.project_id=p.id WHERE h.state IS NULL OR h.state!='cleared'",
            "SELECT COUNT(*) FROM step_runs WHERE state IN ('pending','ready','running','outcome_unknown')",
            "SELECT COUNT(*) FROM verification_leases WHERE state IN ('running','outcome_unknown')",
            "SELECT COUNT(*) FROM loops WHERE state IN ('active','blocked')",
            "SELECT COUNT(*) FROM loop_schedules WHERE enabled=1",
            "SELECT COUNT(*) FROM outbox WHERE state IN ('pending','sending')",
            "SELECT COUNT(*) FROM delivery_workspaces d JOIN cycles c ON c.id=d.cycle_id JOIN loops l ON l.id=c.loop_id WHERE c.state NOT IN ('closed','cancelled') AND l.state NOT IN ('completed','stopped') AND d.state!='ready'",
        ] {
            let count: i64 = sqlx::query_scalar(sql).fetch_one(&mut *tx).await?;
            ensure!(count == 0, "activation ledger still has unresolved or armed state");
        }
        if let Some(row) =
            sqlx::query("SELECT proof,preflight FROM recovery_activations WHERE singleton=1")
                .fetch_optional(&mut *tx)
                .await?
        {
            ensure!(
                serde_json::from_str::<Value>(&row.get::<String, _>("proof"))? == proof
                    && serde_json::from_str::<Value>(&row.get::<String, _>("preflight"))?
                        == *preflight,
                "activation already bound to a different decision or observation"
            );
            tx.commit().await?;
            return Ok(
                json!({"authorized":true,"target_state":decision.target_state,"goals_resumed":false,"service_started":false}),
            );
        }
        sqlx::query("INSERT INTO recovery_activations VALUES(1,?,?,?,?,?,?)")
            .bind(&decision.manifest_sha256)
            .bind(&decision.source_receipt_sha256)
            .bind(&decision.target_state)
            .bind(proof.to_string())
            .bind(preflight.to_string())
            .bind(now())
            .execute(&mut *tx)
            .await?;
        Self::event(&mut tx, None, "recovery_activation_authorized", json!({"decision":proof,"preflight":preflight,"goals_resumed":false,"service_started":false}), false).await?;
        tx.commit().await?;
        Ok(
            json!({"authorized":true,"target_state":decision.target_state,"goals_resumed":false,"service_started":false}),
        )
    }

    pub async fn recovery_activation(&self) -> Result<Option<Value>> {
        let row = sqlx::query("SELECT * FROM recovery_activations WHERE singleton=1")
            .fetch_optional(&self.pool)
            .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let proof: Value = serde_json::from_str(&row.get::<String, _>("proof"))?;
        let preflight: Value = serde_json::from_str(&row.get::<String, _>("preflight"))?;
        for column in ["manifest_sha256", "source_receipt_sha256", "target_state"] {
            ensure!(
                proof[column] == row.get::<String, _>(column),
                "activation record identity mismatch"
            );
        }
        let events = sqlx::query(
            "SELECT body FROM events WHERE kind='recovery_activation_authorized' LIMIT 2",
        )
        .fetch_all(&self.pool)
        .await?;
        ensure!(events.len() == 1, "activation audit missing or ambiguous");
        let event: Value = serde_json::from_str(&events[0].get::<String, _>("body"))?;
        ensure!(
            event["decision"] == proof && event["preflight"] == preflight,
            "activation audit mismatch"
        );
        Ok(Some(json!({"decision":proof,"preflight":preflight})))
    }

    /// Immutable-ledger preflight for later activation. No external paths or process actions.
    pub async fn recovery_activation_inventory(&self) -> Result<Value> {
        let preparation = self
            .recovery_preparation()
            .await?
            .context("prepared recovery required")?;
        let mut counts = serde_json::Map::new();
        for (name, sql) in [
            ("held_projects", "SELECT COUNT(*) FROM recovery_project_holds WHERE state!='cleared'"),
            ("unfinished_steps", "SELECT COUNT(*) FROM step_runs WHERE state IN ('pending','ready','running','outcome_unknown')"),
            ("unfinished_verifications", "SELECT COUNT(*) FROM verification_leases WHERE state IN ('running','outcome_unknown')"),
            ("unpaused_goals", "SELECT COUNT(*) FROM loops WHERE state IN ('active','blocked')"),
            ("armed_schedules", "SELECT COUNT(*) FROM loop_schedules WHERE enabled=1"),
            ("pending_notifications", "SELECT COUNT(*) FROM outbox WHERE state IN ('pending','sending')"),
            ("current_workspaces", "SELECT COUNT(*) FROM delivery_workspaces d JOIN cycles c ON c.id=d.cycle_id JOIN loops l ON l.id=c.loop_id WHERE c.state NOT IN ('closed','cancelled') AND l.state NOT IN ('completed','stopped')"),
        ] {
            let count: i64 = sqlx::query_scalar(sql).fetch_one(&self.pool).await?;
            counts.insert(name.into(), json!(count));
        }
        let external:i64=sqlx::query_scalar(&format!("SELECT COUNT(*) FROM ({}) e WHERE json_extract(body,'$.phase') IN ('pending','outcome_unknown','acquired')",environment_operations::LATEST_ENVIRONMENT)).fetch_one(&self.pool).await?;
        counts.insert("external_environment_sessions".into(), json!(external));
        let rows = sqlx::query("SELECT d.*,l.project_id FROM delivery_workspaces d JOIN cycles c ON c.id=d.cycle_id JOIN loops l ON l.id=c.loop_id WHERE c.state NOT IN ('closed','cancelled') AND l.state NOT IN ('completed','stopped') ORDER BY d.id LIMIT 100")
            .fetch_all(&self.pool).await?;
        let mut workspaces = Vec::new();
        for row in rows {
            let project: String = row.get("project_id");
            let workspace = crate::workspaces::record(row);
            let event: Option<String> = sqlx::query_scalar("SELECT body FROM events WHERE kind='recovery_workspace_adopted' AND json_extract(body,'$.adoption.workspace_id')=? ORDER BY sequence DESC LIMIT 1")
                .bind(&workspace.id).fetch_optional(&self.pool).await?;
            let audit = event
                .map(|body| serde_json::from_str::<Value>(&body))
                .transpose()?;
            workspaces
                .push(json!({"workspace":workspace,"project_id":project,"adoption_audit":audit}));
        }
        Ok(
            json!({"preparation":preparation,"counts":counts,"workspace_records":workspaces,"workspace_records_truncated":counts["current_workspaces"].as_i64().unwrap_or(0)>100}),
        )
    }

    pub async fn recovery_preparation(&self) -> Result<Option<Value>> {
        let row = sqlx::query("SELECT manifest_sha256,database_sha256,original_state,state,report FROM recovery_preparations WHERE singleton=1")
            .fetch_optional(&self.pool).await?;
        row.map(|r| Ok(json!({"manifest_sha256":r.get::<String,_>("manifest_sha256"),"database_sha256":r.get::<String,_>("database_sha256"),"original_state":r.get::<String,_>("original_state"),"state":r.get::<String,_>("state"),"report":serde_json::from_str::<Value>(&r.get::<String,_>("report"))?}))).transpose()
    }

    /// Trusted offline daemon maintenance only. Caller must verify the image and own its lock.
    /// This transaction deliberately leaves a persistent gate; no activation API exists yet.
    pub async fn prepare_recovery(
        &self,
        manifest_sha256: &str,
        database_sha256: &str,
        original_state: &Path,
    ) -> Result<Value> {
        for digest in [manifest_sha256, database_sha256] {
            ensure!(
                digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()),
                "recovery fingerprint must be SHA256"
            );
        }
        ensure!(
            original_state.is_absolute(),
            "original state must be absolute"
        );
        let original = original_state
            .to_str()
            .context("original state is not UTF8")?;
        let mut tx = self.pool.begin().await?;
        if let Some(row) = sqlx::query("SELECT manifest_sha256,database_sha256,original_state,report FROM recovery_preparations WHERE singleton=1").fetch_optional(&mut *tx).await? {
            ensure!(row.get::<String,_>("manifest_sha256")==manifest_sha256 && row.get::<String,_>("database_sha256")==database_sha256 && row.get::<String,_>("original_state")==original,"different recovery preparation already exists");
            let report=serde_json::from_str(&row.get::<String,_>("report"))?;
            tx.commit().await?; return Ok(report);
        }
        let mut counts = serde_json::Map::new();
        let holds=sqlx::query("INSERT INTO recovery_project_holds(project_id,manifest_sha256,state,created_at) SELECT id,?,'held',? FROM projects")
            .bind(manifest_sha256).bind(now()).execute(&mut *tx).await?.rows_affected();
        counts.insert("projects_held".into(), json!(holds));

        // Pending/ready work may have run after the snapshot: cancellation alone would lose that uncertainty.
        for (label,sql) in [
            ("steps_unknown", "UPDATE step_runs SET state='outcome_unknown',lease_until=NULL WHERE state IN ('pending','ready','running')"),
            ("work_items_unknown", "UPDATE work_items SET state='outcome_unknown' WHERE id IN (SELECT work_item_id FROM step_runs WHERE state='outcome_unknown')"),
            ("cycles_blocked", "UPDATE cycles SET state='blocked',conclusion='restore_requires_reconciliation' WHERE state NOT IN ('closed','cancelled')"),
            ("goals_paused", "UPDATE loops SET state='paused',version=version+1,next_due_at=NULL,updated_at=unixepoch() WHERE state IN ('active','blocked')"),
            ("schedules_disabled", "UPDATE loop_schedules SET enabled=0,suspended_reason='restore_requires_reconciliation',updated_at=unixepoch() WHERE enabled=1"),
            ("verification_leases_unknown", "UPDATE verification_leases SET state='outcome_unknown' WHERE state='running'"),
            ("workspaces_uncertain", "UPDATE delivery_workspaces SET state='uncertain',last_error='restore_requires_reconciliation',updated_at=unixepoch() WHERE state IN ('preparing','ready')"),
            ("notifications_suppressed", "UPDATE outbox SET state='superseded',lease_token=NULL,lease_until=NULL,last_error='restore_snapshot_delivery_outcome_unknown' WHERE state IN ('pending','sending')"),
        ] {
            let changed=sqlx::query(sql).execute(&mut *tx).await?.rows_affected();
            counts.insert(label.into(),json!(changed));
        }
        let report = json!({"state":"prepared","execution_enabled":false,"counts":counts,"attempt_budgets_preserved":true,"external_outcomes_reconciled":false,"paths_rewritten":false});
        sqlx::query("INSERT INTO recovery_preparations(singleton,manifest_sha256,database_sha256,original_state,state,report,created_at) VALUES(1,?,?,?,'prepared',?,?)")
            .bind(manifest_sha256).bind(database_sha256).bind(original).bind(report.to_string()).bind(now()).execute(&mut *tx).await?;
        Self::event(&mut tx,None,"recovery_prepared",json!({"manifest_sha256":manifest_sha256,"database_sha256":database_sha256,"original_state":original,"report":report}),false).await?;
        tx.commit().await?;
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("telos-recovery-{}", Uuid::new_v4()));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    async fn populate(store: &Store, root: &Path) -> Result<()> {
        store.register_project("p", root).await?;
        let contract: Contract = serde_json::from_value(
            json!({"goal":"fixture recovery","delivery_scope":"fixture","criteria":[{"id":"check","description":"fixture"}],"interval_seconds":300,"max_attempts":3}),
        )?;
        let goal = store
            .create_loop("p", "fixture", LoopKind::Continuous, &contract)
            .await?;
        let cycle = store.start_cycle(&goal.id, 1, "fixture").await?;
        let spec = telos_core::ExecutionSpec {
            task: "fixture".into(),
            cwd: root.to_string_lossy().into(),
            sandbox: telos_core::Sandbox::ReadOnly,
            timeout_seconds: 10,
            max_output_bytes: 4096,
        };
        store.enqueue_step(&cycle, 1, "one", &spec, 0).await?;
        store.enqueue_step(&cycle, 1, "two", &spec, 0).await?;
        store
            .claim_next("fixture-worker", now(), 30)
            .await?
            .context("fixture claim failed")?;
        let mut tx = store.pool.begin().await?;
        sqlx::query("INSERT INTO loop_schedules VALUES(?,1,1,1,NULL,?)")
            .bind(&goal.id)
            .bind(now())
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO verification_leases VALUES('lease','p',?,?,1,1,'running',?,NULL)")
            .bind(&goal.id)
            .bind(&cycle)
            .bind(now())
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO verification_test_attempts VALUES('lease',?,1,?)")
            .bind(&cycle)
            .bind(now())
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO delivery_workspaces VALUES('workspace',?,1,1,'/fixture/worktree','/fixture/git','abc','ready',NULL,?,?)").bind(&cycle).bind(now()).bind(now()).execute(&mut *tx).await?;
        Store::event(&mut tx, Some(&goal.id), "fixture_notice", json!({}), true).await?;
        tx.commit().await?;
        Ok(())
    }
    #[tokio::test]
    async fn recovery_preparation_is_atomic_idempotent_and_preserves_uncertainty_and_budgets(
    ) -> Result<()> {
        let f = Fixture::new();
        let store = Store::open(&f.0).await?;
        populate(&store, &f.0).await?;
        let manifest = "a".repeat(64);
        let database = "b".repeat(64);
        let result = store.prepare_recovery(&manifest, &database, &f.0).await?;
        assert_eq!(result["counts"]["steps_unknown"], 2);
        assert_eq!(result["counts"]["goals_paused"], 1);
        assert_eq!(result["counts"]["verification_leases_unknown"], 1);
        assert_eq!(result["counts"]["workspaces_uncertain"], 1);
        assert_eq!(result["counts"]["notifications_suppressed"], 1);
        let inventory = store.recovery_activation_inventory().await?;
        assert_eq!(inventory["counts"]["held_projects"], 1);
        assert_eq!(inventory["counts"]["unfinished_steps"], 2);
        assert_eq!(inventory["counts"]["unfinished_verifications"], 1);
        assert_eq!(inventory["counts"]["unpaused_goals"], 0);
        assert_eq!(inventory["counts"]["armed_schedules"], 0);
        assert_eq!(inventory["counts"]["pending_notifications"], 0);
        assert_eq!(inventory["counts"]["current_workspaces"], 1);
        assert_eq!(
            inventory["workspace_records"][0]["workspace"]["state"],
            "uncertain"
        );
        assert!(inventory["workspace_records"][0]["adoption_audit"].is_null());
        assert!(store
            .claim_next("must-not-claim", now(), 30)
            .await?
            .is_none());
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT SUM(attempt) FROM step_runs")
                .fetch_one(&store.pool)
                .await?,
            1
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM verification_test_attempts")
                .fetch_one(&store.pool)
                .await?,
            1
        );
        assert_eq!(
            store.prepare_recovery(&manifest, &database, &f.0).await?,
            result
        );
        assert!(store
            .prepare_recovery(&"c".repeat(64), &database, &f.0)
            .await
            .is_err());
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM events WHERE kind='recovery_prepared'"
            )
            .fetch_one(&store.pool)
            .await?,
            1
        );
        store.close().await;
        let reopened = Store::open(&f.0).await?;
        assert_eq!(
            reopened.recovery_preparation().await?.unwrap()["state"],
            "prepared"
        );
        reopened.close().await;
        Ok(())
    }
    #[tokio::test]
    async fn failed_audit_insert_rolls_back_all_preparation_mutations() -> Result<()> {
        let f = Fixture::new();
        let store = Store::open(&f.0).await?;
        populate(&store, &f.0).await?;
        sqlx::query("CREATE TRIGGER reject_recovery_audit BEFORE INSERT ON events WHEN NEW.kind='recovery_prepared' BEGIN SELECT RAISE(ABORT,'fixture injected failure'); END").execute(&store.pool).await?;
        assert!(store
            .prepare_recovery(&"a".repeat(64), &"b".repeat(64), &f.0)
            .await
            .is_err());
        assert!(store.recovery_preparation().await?.is_none());
        assert!(store.recovery_project_status("p").await?.is_none());
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM step_runs WHERE state IN ('ready','running')"
            )
            .fetch_one(&store.pool)
            .await?,
            2
        );
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT state FROM loops")
                .fetch_one(&store.pool)
                .await?,
            "active"
        );
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT state FROM outbox")
                .fetch_one(&store.pool)
                .await?,
            "pending"
        );
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT state FROM verification_leases")
                .fetch_one(&store.pool)
                .await?,
            "running"
        );
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT state FROM delivery_workspaces")
                .fetch_one(&store.pool)
                .await?,
            "ready"
        );
        store.close().await;
        Ok(())
    }
    #[tokio::test]
    async fn project_holds_block_resume_and_new_work_even_without_unknown_steps() -> Result<()> {
        let f = Fixture::new();
        let store = Store::open(&f.0).await?;
        store.register_project("p", &f.0).await?;
        let contract: Contract = serde_json::from_value(
            json!({"goal":"fixture","delivery_scope":"fixture","criteria":[{"id":"a","description":"fixture"}],"max_attempts":1,"interval_seconds":null}),
        )?;
        let goal = store
            .create_loop("p", "fixture", LoopKind::Delivery, &contract)
            .await?;
        store
            .prepare_recovery(&"a".repeat(64), &"b".repeat(64), &f.0)
            .await?;
        assert_eq!(
            store.recovery_project_status("p").await?.unwrap()["state"],
            "held"
        );
        let paused = store.get_loop(&goal.id).await?;
        assert_eq!(paused.state, LoopState::Paused);
        assert!(store
            .control(&goal.id, paused.version, "resume")
            .await
            .unwrap_err()
            .to_string()
            .contains("restore reconciliation"));
        assert!(store
            .create_loop("p", "bypass", LoopKind::Delivery, &contract)
            .await
            .is_err());
        assert!(store.start_cycle(&goal.id, 1, "bypass").await.is_err());
        assert!(store
            .configure_project(
                "p",
                0,
                &telos_core::ProjectExecutionPolicy {
                    enabled: true,
                    sandbox: telos_core::Sandbox::ReadOnly,
                    timeout_seconds: 10,
                    max_output_bytes: 4096,
                    max_steps_per_cycle: 1
                }
            )
            .await
            .is_err());
        // Read access and the user's stop action remain available.
        assert_eq!(
            store.control(&goal.id, paused.version, "stop").await?.state,
            LoopState::Stopped
        );
        store.close().await;
        let reopened = Store::open(&f.0).await?;
        assert_eq!(
            reopened.recovery_project_status("p").await?.unwrap()["state"],
            "held"
        );
        reopened.close().await;
        Ok(())
    }
    #[tokio::test]
    async fn prepared_v17_migration_backfills_project_holds_with_audit() -> Result<()> {
        let f = Fixture::new();
        let store = Store::open(&f.0).await?;
        store.register_project("p", &f.0).await?;
        store
            .prepare_recovery(&"a".repeat(64), &"b".repeat(64), &f.0)
            .await?;
        // Recreate the pre-v18 schema shape in this isolated synthetic fixture.
        sqlx::query("DROP TABLE recovery_project_holds")
            .execute(&store.pool)
            .await?;
        sqlx::query("DROP TABLE recovery_activations")
            .execute(&store.pool)
            .await?;
        sqlx::query("DELETE FROM schema_migrations WHERE version>=18")
            .execute(&store.pool)
            .await?;
        store.close().await;
        let reopened = Store::open(&f.0).await?;
        assert_eq!(
            reopened.recovery_project_status("p").await?.unwrap()["state"],
            "held"
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM events WHERE kind='recovery_project_holds_migrated'"
            )
            .fetch_one(&reopened.pool)
            .await?,
            1
        );
        reopened.close().await;
        let again = Store::open(&f.0).await?;
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM events WHERE kind='recovery_project_holds_migrated'"
            )
            .fetch_one(&again.pool)
            .await?,
            1
        );
        again.close().await;
        Ok(())
    }
    async fn review_for(
        store: &Store,
        project: &str,
        decision: &str,
    ) -> Result<telos_core::RecoveryProjectReview> {
        let manifest = "a".repeat(64);
        let evidence = store
            .record_recovery_review_evidence(
                project,
                &manifest,
                decision,
                b"Synthetic operator review material; not a real external observation.",
            )
            .await?;
        Ok(telos_core::RecoveryProjectReview {
            project_id: project.into(),
            manifest_sha256: manifest,
            decision_id: decision.into(),
            operator: "fixture operator".into(),
            statement: "Synthetic review for testing the trusted operator boundary".into(),
            reviewed_external_state: true,
            evidence: vec![evidence],
        })
    }
    #[tokio::test]
    async fn operator_material_read_checks_registry_hash_paths_and_pages() -> Result<()> {
        let f = Fixture::new();
        let store = Store::open(&f.0).await?;
        store.register_project("p", &f.0).await?;
        store
            .prepare_recovery(&"a".repeat(64), &"b".repeat(64), &f.0)
            .await?;
        let review = review_for(&store, "p", "read-test").await?;
        let id = &review.evidence[0].id;
        let first = store.get_evidence(id, 0, 9).await?;
        assert_eq!(first["text"], "Synthetic");
        assert_eq!(first["next_offset"], 9);
        assert_eq!(first["kind"], "recovery_operator_material");
        assert_eq!(first["decision_id"], "read-test");
        let length = first["byte_length"].as_u64().unwrap() as usize;
        assert_eq!(store.get_evidence(id, length, 1).await?["bytes"], json!([]));
        assert!(store.get_evidence(id, length + 1, 1).await.is_err());
        assert!(store.get_evidence(id, 0, 32769).await.is_err());
        assert!(store
            .get_evidence("recovery-evidence-../../secret", 0, 1)
            .await
            .is_err());
        let parent = f.0.join("recovery-evidence");
        std::fs::write(parent.join("recovery-evidence-orphan.bin"), b"orphan")?;
        assert!(store
            .get_evidence("recovery-evidence-orphan", 0, 1)
            .await
            .is_err());
        let path = parent.join(format!("{id}.bin"));
        let original = std::fs::read(&path)?;
        std::fs::write(&path, b"tampered")?;
        assert!(store.get_evidence(id, 0, 1).await.is_err());
        std::fs::remove_file(&path)?;
        let outside = f.0.join("outside-material");
        std::fs::write(&outside, &original)?;
        std::os::unix::fs::symlink(&outside, &path)?;
        assert!(store.get_evidence(id, 0, 1).await.is_err());
        std::fs::remove_file(&path)?;
        std::fs::write(&path, &original)?;
        store.release_recovery_project(&review).await?;
        assert_eq!(
            store.recovery_project_status("p").await?.unwrap()["proof"],
            serde_json::to_value(&review)?
        );
        assert_eq!(store.get_evidence(id, 0, 9).await?["text"], "Synthetic");
        // A matching file alone cannot compensate for mismatched admission metadata.
        sqlx::query("UPDATE events SET body=json_set(body,'$.manifest_sha256','wrong') WHERE kind='recovery_review_evidence_recorded'").execute(&store.pool).await?;
        assert!(store.get_evidence(id, 0, 1).await.is_err());
        store.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn operator_review_clears_only_the_hold_and_is_atomic_and_idempotent() -> Result<()> {
        let f = Fixture::new();
        let store = Store::open(&f.0).await?;
        store.register_project("p", &f.0).await?;
        let contract: Contract = serde_json::from_value(
            json!({"goal":"fixture","delivery_scope":"fixture","criteria":[{"id":"a","description":"fixture"}],"max_attempts":1,"interval_seconds":null}),
        )?;
        let goal = store
            .create_loop("p", "fixture", LoopKind::Delivery, &contract)
            .await?;
        store
            .prepare_recovery(&"a".repeat(64), &"b".repeat(64), &f.0)
            .await?;
        let review = review_for(&store, "p", "review-1").await?;
        sqlx::query("CREATE TRIGGER reject_review BEFORE INSERT ON events WHEN NEW.kind='recovery_project_reviewed' BEGIN SELECT RAISE(ABORT,'fixture audit failure'); END").execute(&store.pool).await?;
        assert!(store.release_recovery_project(&review).await.is_err());
        assert_eq!(
            store.recovery_project_status("p").await?.unwrap()["state"],
            "held"
        );
        sqlx::query("DROP TRIGGER reject_review")
            .execute(&store.pool)
            .await?;
        let result = store.release_recovery_project(&review).await?;
        assert_eq!(result["state"], "cleared");
        assert_eq!(result["service_activated"], false);
        assert_eq!(store.get_loop(&goal.id).await?.state, LoopState::Paused);
        assert_eq!(
            store.recovery_preparation().await?.unwrap()["state"],
            "prepared"
        );
        assert_eq!(store.release_recovery_project(&review).await?, result);
        let mut other = review.clone();
        other.statement = "different decision".into();
        assert!(store.release_recovery_project(&other).await.is_err());
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM events WHERE kind='recovery_project_reviewed'"
            )
            .fetch_one(&store.pool)
            .await?,
            1
        );
        store.close().await;
        Ok(())
    }
    #[tokio::test]
    async fn activation_authorization_is_bound_atomic_and_keeps_goals_paused() -> Result<()> {
        let f = Fixture::new();
        let store = Store::open(&f.0).await?;
        store.register_project("p", &f.0).await?;
        let contract: Contract = serde_json::from_value(
            json!({"goal":"fixture","delivery_scope":"fixture","criteria":[{"id":"a","description":"fixture"}],"max_attempts":1,"interval_seconds":null}),
        )?;
        let goal = store
            .create_loop("p", "fixture", LoopKind::Delivery, &contract)
            .await?;
        store
            .prepare_recovery(&"a".repeat(64), &"b".repeat(64), &f.0)
            .await?;
        let decision = telos_core::RecoveryActivation {
            manifest_sha256: "a".repeat(64),
            source_receipt_sha256: "c".repeat(64),
            target_state: f
                .0
                .with_extension("activated")
                .join("state")
                .to_string_lossy()
                .into(),
            decision_id: "synthetic-activation".into(),
            operator: "fixture".into(),
            statement: "Synthetic fixture; no real retirement or ownership assertion".into(),
            source_instance_retired: true,
            exclusive_access_confirmed: true,
        };
        let preflight = json!({"checks_passed":true,"blockers":[],"source_receipt_sha256":decision.source_receipt_sha256,"inventory":{"preparation":{"manifest_sha256":decision.manifest_sha256}}});
        // A claimed successful observation cannot bypass unresolved ledger holds.
        assert!(store
            .authorize_recovery_activation(&decision, &preflight)
            .await
            .is_err());
        let review = review_for(&store, "p", "release-for-activation").await?;
        store.release_recovery_project(&review).await?;
        let mut wrong = decision.clone();
        wrong.source_instance_retired = false;
        assert!(store
            .authorize_recovery_activation(&wrong, &preflight)
            .await
            .is_err());
        wrong = decision.clone();
        wrong.exclusive_access_confirmed = false;
        assert!(store
            .authorize_recovery_activation(&wrong, &preflight)
            .await
            .is_err());
        wrong = decision.clone();
        wrong.target_state = f.0.to_string_lossy().into();
        assert!(store
            .authorize_recovery_activation(&wrong, &preflight)
            .await
            .is_err());
        std::fs::create_dir(f.0.join("runs"))?;
        assert!(store
            .authorize_recovery_activation(&decision, &preflight)
            .await
            .is_err());
        std::fs::remove_dir(f.0.join("runs"))?;
        sqlx::query("CREATE TRIGGER reject_activation BEFORE INSERT ON events WHEN NEW.kind='recovery_activation_authorized' BEGIN SELECT RAISE(ABORT,'fixture failure'); END").execute(&store.pool).await?;
        assert!(store
            .authorize_recovery_activation(&decision, &preflight)
            .await
            .is_err());
        assert!(store.recovery_activation().await?.is_none());
        sqlx::query("DROP TRIGGER reject_activation")
            .execute(&store.pool)
            .await?;
        let result = store
            .authorize_recovery_activation(&decision, &preflight)
            .await?;
        assert_eq!(result["goals_resumed"], false);
        assert_eq!(result["service_started"], false);
        assert_eq!(
            store
                .authorize_recovery_activation(&decision, &preflight)
                .await?,
            result
        );
        wrong = decision.clone();
        wrong.target_state.push_str("-other");
        assert!(store
            .authorize_recovery_activation(&wrong, &preflight)
            .await
            .is_err());
        assert_eq!(store.get_loop(&goal.id).await?.state, LoopState::Paused);
        assert!(store.claim_next("must-not-run", now(), 30).await?.is_none());
        assert_eq!(
            store.recovery_preparation().await?.unwrap()["state"],
            "prepared"
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM events WHERE kind='recovery_activation_authorized'"
            )
            .fetch_one(&store.pool)
            .await?,
            1
        );
        store.close().await;
        let reopened = Store::open(&f.0).await?;
        assert_eq!(
            reopened.recovery_activation().await?.unwrap()["decision"],
            serde_json::to_value(decision)?
        );
        reopened.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn run_archive_admission_requires_resolved_work_and_atomic_audit() -> Result<()> {
        let f = Fixture::new();
        let store = Store::open(&f.0).await?;
        populate(&store, &f.0).await?;
        let manifest = "a".repeat(64);
        store
            .prepare_recovery(&manifest, &"b".repeat(64), &f.0)
            .await?;
        assert!(store
            .record_recovery_run_archive(&manifest, &"c".repeat(64), 1)
            .await
            .is_err());
        let review = review_for(&store, "p", "synthetic-archive-prerequisite").await?;
        let steps = sqlx::query_scalar("SELECT id FROM step_runs")
            .fetch_all(&store.pool)
            .await?;
        store
            .discard_recovered_executions(&telos_core::RecoveryExecutionDisposition {
                review: review.clone(),
                step_ids: steps,
                processes_confirmed_stopped: true,
                external_effects_reconciled: true,
            })
            .await?;
        assert!(store
            .record_recovery_run_archive(&manifest, &"c".repeat(64), 1)
            .await
            .is_err());
        store
            .discard_recovered_verifications(&telos_core::RecoveryVerificationDisposition {
                review,
                lease_ids: vec!["lease".into()],
                processes_confirmed_stopped: true,
                external_effects_reconciled: true,
            })
            .await?;
        sqlx::query("CREATE TRIGGER reject_archive BEFORE INSERT ON events WHEN NEW.kind='recovery_runs_archived' BEGIN SELECT RAISE(ABORT,'fixture failure'); END").execute(&store.pool).await?;
        assert!(store
            .record_recovery_run_archive(&manifest, &"c".repeat(64), 1)
            .await
            .is_err());
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM events WHERE kind='recovery_runs_archived'"
            )
            .fetch_one(&store.pool)
            .await?,
            0
        );
        sqlx::query("DROP TRIGGER reject_archive")
            .execute(&store.pool)
            .await?;
        assert!(store
            .record_recovery_run_archive(&"d".repeat(64), &"c".repeat(64), 1)
            .await
            .is_err());
        let proof = store
            .record_recovery_run_archive(&manifest, &"c".repeat(64), 1)
            .await?;
        assert_eq!(proof["process_actions"], false);
        assert!(store
            .record_recovery_run_archive(&manifest, &"c".repeat(64), 1)
            .await
            .is_err());
        assert_eq!(
            store.recovery_project_status("p").await?.unwrap()["state"],
            "held"
        );
        store.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn workspace_adoption_rejects_uncertainty_and_rolls_back_on_audit_failure() -> Result<()>
    {
        let f = Fixture::new();
        let store = Store::open(&f.0).await?;
        populate(&store, &f.0).await?;
        // Synthetic legacy workspace fixture; no external filesystem adoption occurs here.
        let policy = telos_core::ProjectExecutionPolicy {
            enabled: true,
            sandbox: telos_core::Sandbox::WorkspaceWrite,
            timeout_seconds: 30,
            max_output_bytes: 4096,
            max_steps_per_cycle: 3,
        };
        sqlx::query("INSERT INTO project_execution_policies VALUES('p',1,?,?)")
            .bind(serde_json::to_string(&policy)?)
            .bind(now())
            .execute(&store.pool)
            .await?;
        sqlx::query("UPDATE loops SET kind='delivery'")
            .execute(&store.pool)
            .await?;
        store
            .prepare_recovery(&"a".repeat(64), &"b".repeat(64), &f.0)
            .await?;
        let review = review_for(&store, "p", "synthetic-workspace").await?;
        let context = store.recovery_workspace_context("workspace").await?;
        let expected: telos_core::DeliveryWorkspace =
            serde_json::from_value(context["workspace"].clone())?;
        let observed = json!({"workspace_id":"workspace","base_commit":"abc","files":[],"scope":"synthetic store test"});
        let adoption = telos_core::RecoveryWorkspaceAdoption {
            review: review.clone(),
            workspace_id: "workspace".into(),
            candidate_path: "/fixture/recovered".into(),
            expected_snapshot_sha256: format!(
                "{:x}",
                Sha256::digest(serde_json::to_vec(&observed)?)
            ),
            exclusive_access_confirmed: true,
        };
        assert!(store
            .adopt_recovered_workspace(&adoption, &expected, &observed)
            .await
            .is_err());
        let steps = sqlx::query_scalar("SELECT id FROM step_runs")
            .fetch_all(&store.pool)
            .await?;
        store
            .discard_recovered_executions(&telos_core::RecoveryExecutionDisposition {
                review: review.clone(),
                step_ids: steps,
                processes_confirmed_stopped: true,
                external_effects_reconciled: true,
            })
            .await?;
        assert!(store
            .adopt_recovered_workspace(&adoption, &expected, &observed)
            .await
            .is_err());
        store
            .discard_recovered_verifications(&telos_core::RecoveryVerificationDisposition {
                review,
                lease_ids: vec!["lease".into()],
                processes_confirmed_stopped: true,
                external_effects_reconciled: true,
            })
            .await?;
        let mut stale = expected.clone();
        stale.policy_revision += 1;
        assert!(store
            .adopt_recovered_workspace(&adoption, &stale, &observed)
            .await
            .is_err());
        sqlx::query("CREATE TRIGGER reject_adoption BEFORE INSERT ON events WHEN NEW.kind='recovery_workspace_adopted' BEGIN SELECT RAISE(ABORT,'fixture failure'); END").execute(&store.pool).await?;
        assert!(store
            .adopt_recovered_workspace(&adoption, &expected, &observed)
            .await
            .is_err());
        assert_eq!(
            store.recovery_workspace_context("workspace").await?["workspace"],
            context["workspace"]
        );
        sqlx::query("DROP TRIGGER reject_adoption")
            .execute(&store.pool)
            .await?;
        store
            .adopt_recovered_workspace(&adoption, &expected, &observed)
            .await?;
        assert_eq!(
            store.recovery_workspace_context("workspace").await?["workspace"]["path"],
            "/fixture/recovered"
        );
        assert_eq!(
            store.recovery_project_status("p").await?.unwrap()["state"],
            "held"
        );
        assert!(store.claim_next("no-retry", now(), 30).await?.is_none());
        store.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn recovered_verification_disposition_preserves_budget_and_other_blockers() -> Result<()>
    {
        let f = Fixture::new();
        let store = Store::open(&f.0).await?;
        populate(&store, &f.0).await?;
        let q = f.0.join("q");
        std::fs::create_dir(&q)?;
        store.register_project("q", &q).await?;
        store
            .prepare_recovery(&"a".repeat(64), &"b".repeat(64), &f.0)
            .await?;
        let disposition = telos_core::RecoveryVerificationDisposition {
            review: review_for(&store, "p", "discard-verification").await?,
            lease_ids: vec!["lease".into()],
            processes_confirmed_stopped: true,
            external_effects_reconciled: true,
        };
        let mut invalid = disposition.clone();
        invalid.processes_confirmed_stopped = false;
        assert!(store
            .discard_recovered_verifications(&invalid)
            .await
            .is_err());
        invalid = disposition.clone();
        invalid.external_effects_reconciled = false;
        assert!(store
            .discard_recovered_verifications(&invalid)
            .await
            .is_err());
        invalid = disposition.clone();
        invalid.lease_ids.push("lease".into());
        assert!(store
            .discard_recovered_verifications(&invalid)
            .await
            .is_err());
        invalid = disposition.clone();
        invalid.review = review_for(&store, "q", "wrong-project").await?;
        assert!(store
            .discard_recovered_verifications(&invalid)
            .await
            .unwrap_err()
            .to_string()
            .contains("another project"));
        invalid = disposition.clone();
        invalid.lease_ids.push("missing".into());
        assert!(store
            .discard_recovered_verifications(&invalid)
            .await
            .is_err());
        sqlx::query("CREATE TRIGGER reject_verification_discard BEFORE INSERT ON events WHEN NEW.kind='recovery_verifications_discarded' BEGIN SELECT RAISE(ABORT,'fixture failure'); END").execute(&store.pool).await?;
        assert!(store
            .discard_recovered_verifications(&disposition)
            .await
            .is_err());
        assert_eq!(
            sqlx::query_scalar::<_, String>(
                "SELECT state FROM verification_leases WHERE id='lease'"
            )
            .fetch_one(&store.pool)
            .await?,
            "outcome_unknown"
        );
        sqlx::query("DROP TRIGGER reject_verification_discard")
            .execute(&store.pool)
            .await?;
        let result = store.discard_recovered_verifications(&disposition).await?;
        assert_eq!(result["task_success_asserted"], false);
        assert_eq!(
            store.discard_recovered_verifications(&disposition).await?,
            result
        );
        invalid = disposition.clone();
        invalid.review.statement = "changed".into();
        assert!(store
            .discard_recovered_verifications(&invalid)
            .await
            .is_err());
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM verification_test_attempts")
                .fetch_one(&store.pool)
                .await?,
            1
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM step_runs WHERE state='outcome_unknown'"
            )
            .fetch_one(&store.pool)
            .await?,
            2
        );
        assert_eq!(
            store.recovery_project_status("p").await?.unwrap()["state"],
            "held"
        );
        assert!(store
            .release_recovery_project(&disposition.review)
            .await
            .unwrap_err()
            .to_string()
            .contains("execution uncertainty"));
        assert!(store.claim_next("no-retry", now(), 30).await?.is_none());
        store.close().await;
        let reopened = Store::open(&f.0).await?;
        assert_eq!(
            sqlx::query_scalar::<_, String>(
                "SELECT state FROM verification_leases WHERE id='lease'"
            )
            .fetch_one(&reopened.pool)
            .await?,
            "finished"
        );
        reopened.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn recovered_execution_disposition_is_explicit_atomic_and_never_retries() -> Result<()> {
        let f = Fixture::new();
        let store = Store::open(&f.0).await?;
        populate(&store, &f.0).await?;
        let qroot = f.0.join("q");
        std::fs::create_dir(&qroot)?;
        store.register_project("q", &qroot).await?;
        store
            .prepare_recovery(&"a".repeat(64), &"b".repeat(64), &f.0)
            .await?;
        let review = review_for(&store, "p", "discard-1").await?;
        let steps: Vec<String> = sqlx::query_scalar("SELECT id FROM step_runs ORDER BY id")
            .fetch_all(&store.pool)
            .await?;
        let disposition = telos_core::RecoveryExecutionDisposition {
            review,
            step_ids: steps.clone(),
            processes_confirmed_stopped: true,
            external_effects_reconciled: true,
        };
        let mut wrong_project = disposition.clone();
        wrong_project.review = review_for(&store, "q", "discard-q").await?;
        assert!(store
            .discard_recovered_executions(&wrong_project)
            .await
            .unwrap_err()
            .to_string()
            .contains("another project"));
        let material_path =
            f.0.join("recovery-evidence")
                .join(format!("{}.bin", disposition.review.evidence[0].id));
        let material = std::fs::read(&material_path)?;
        std::fs::write(&material_path, b"tampered")?;
        assert!(store
            .discard_recovered_executions(&disposition)
            .await
            .is_err());
        std::fs::write(&material_path, material)?;
        for field in ["processes_confirmed_stopped", "external_effects_reconciled"] {
            let mut invalid = serde_json::to_value(&disposition)?;
            invalid[field] = json!(false);
            let invalid = serde_json::from_value(invalid)?;
            assert!(store.discard_recovered_executions(&invalid).await.is_err());
        }
        let mut invalid = disposition.clone();
        invalid.step_ids.push(steps[0].clone());
        assert!(store.discard_recovered_executions(&invalid).await.is_err());
        invalid = disposition.clone();
        invalid.step_ids.push("missing".into());
        assert!(store.discard_recovered_executions(&invalid).await.is_err());
        assert_eq!(
            store.step_status(&steps[0]).await?["state"],
            "outcome_unknown"
        );
        sqlx::query("CREATE TRIGGER reject_discard BEFORE INSERT ON events WHEN NEW.kind='recovery_executions_discarded' BEGIN SELECT RAISE(ABORT,'fixture audit failure'); END").execute(&store.pool).await?;
        assert!(store
            .discard_recovered_executions(&disposition)
            .await
            .is_err());
        for id in &steps {
            assert_eq!(store.step_status(id).await?["state"], "outcome_unknown");
        }
        sqlx::query("DROP TRIGGER reject_discard")
            .execute(&store.pool)
            .await?;
        let result = store.discard_recovered_executions(&disposition).await?;
        assert_eq!(result["automatic_retry"], false);
        assert_eq!(
            store.discard_recovered_executions(&disposition).await?,
            result
        );
        invalid = disposition.clone();
        invalid.review.statement = "changed disposition".into();
        assert!(store.discard_recovered_executions(&invalid).await.is_err());
        for id in &steps {
            assert_eq!(store.step_status(id).await?["state"], "cancelled");
        }
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT SUM(attempt) FROM step_runs")
                .fetch_one(&store.pool)
                .await?,
            1
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM verification_test_attempts")
                .fetch_one(&store.pool)
                .await?,
            1
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM events WHERE kind='recovery_executions_discarded'"
            )
            .fetch_one(&store.pool)
            .await?,
            1
        );
        assert_eq!(
            store.recovery_project_status("p").await?.unwrap()["state"],
            "held"
        );
        assert_eq!(
            store.recovery_preparation().await?.unwrap()["state"],
            "prepared"
        );
        assert!(store
            .claim_next("must-not-retry", now(), 30)
            .await?
            .is_none());
        assert!(store
            .release_recovery_project(&disposition.review)
            .await
            .unwrap_err()
            .to_string()
            .contains("verification uncertainty"));
        store.close().await;
        let reopened = Store::open(&f.0).await?;
        assert_eq!(reopened.step_status(&steps[0]).await?["state"], "cancelled");
        reopened.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn operator_review_cannot_settle_unknown_execution_or_verification() -> Result<()> {
        let f = Fixture::new();
        let store = Store::open(&f.0).await?;
        populate(&store, &f.0).await?;
        store
            .prepare_recovery(&"a".repeat(64), &"b".repeat(64), &f.0)
            .await?;
        let review = review_for(&store, "p", "review-1").await?;
        assert!(store
            .release_recovery_project(&review)
            .await
            .unwrap_err()
            .to_string()
            .contains("execution uncertainty"));
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM step_runs WHERE state='outcome_unknown'"
            )
            .fetch_one(&store.pool)
            .await?,
            2
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT SUM(attempt) FROM step_runs")
                .fetch_one(&store.pool)
                .await?,
            1
        );
        // Synthetic fixture settlement to isolate the independent verification guard.
        sqlx::query("UPDATE step_runs SET state='cancelled'")
            .execute(&store.pool)
            .await?;
        assert!(store
            .release_recovery_project(&review)
            .await
            .unwrap_err()
            .to_string()
            .contains("verification uncertainty"));
        assert_eq!(
            store.recovery_project_status("p").await?.unwrap()["state"],
            "held"
        );
        store.close().await;
        Ok(())
    }
    #[tokio::test]
    async fn review_material_is_bound_to_snapshot_project_decision_and_bytes() -> Result<()> {
        let f = Fixture::new();
        let store = Store::open(&f.0).await?;
        store.register_project("p", &f.0).await?;
        let q = f.0.join("q");
        std::fs::create_dir(&q)?;
        store.register_project("q", &q).await?;
        assert!(store
            .record_recovery_review_evidence("p", &"a".repeat(64), "r", b"before preparation")
            .await
            .is_err());
        store
            .prepare_recovery(&"a".repeat(64), &"b".repeat(64), &f.0)
            .await?;
        let review = review_for(&store, "p", "review-1").await?;
        let mut wrong = review.clone();
        wrong.project_id = "q".into();
        assert!(store.release_recovery_project(&wrong).await.is_err());
        wrong = review.clone();
        wrong.manifest_sha256 = "c".repeat(64);
        assert!(store.release_recovery_project(&wrong).await.is_err());
        wrong = review.clone();
        wrong.decision_id = "review-2".into();
        assert!(store.release_recovery_project(&wrong).await.is_err());
        wrong = review.clone();
        wrong.reviewed_external_state = false;
        assert!(store.release_recovery_project(&wrong).await.is_err());
        wrong = review.clone();
        wrong.evidence.push(wrong.evidence[0].clone());
        assert!(store.release_recovery_project(&wrong).await.is_err());
        std::fs::write(
            f.0.join("recovery-evidence")
                .join(format!("{}.bin", review.evidence[0].id)),
            b"tampered",
        )?;
        assert!(store
            .release_recovery_project(&review)
            .await
            .unwrap_err()
            .to_string()
            .contains("integrity failure"));
        assert_eq!(
            store.recovery_project_status("p").await?.unwrap()["state"],
            "held"
        );
        store.close().await;
        Ok(())
    }
}
