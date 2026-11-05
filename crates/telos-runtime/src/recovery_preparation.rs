//! Offline daemon maintenance for a verified recovery copy. Never launches processes.
use anyhow::{ensure, Context, Result};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{fs::OpenOptions, io::Read, os::unix::fs::OpenOptionsExt, path::Path};
use telos_store::Store;

pub async fn prepare(
    root: &Path,
    manifest_sha256: &str,
    database_sha256: &str,
    original_state: &Path,
) -> Result<Value> {
    let _owner = crate::daemon::maintenance_lock(root)?;
    verify_copy(root, manifest_sha256, database_sha256, original_state)?;
    let store = Store::open(root).await.context("open recovery copy")?;
    let result = store
        .prepare_recovery(manifest_sha256, database_sha256, original_state)
        .await;
    store.close().await;
    result
}

fn verify_copy(
    root: &Path,
    manifest_sha256: &str,
    database_sha256: &str,
    original_state: &Path,
) -> Result<()> {
    let marker = root.join("restore-required.json");
    ensure!(
        std::fs::symlink_metadata(&marker)?.is_file(),
        "recovery marker must be regular"
    );
    let mut bytes = Vec::new();
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(marker)?
        .take(65537)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 65536, "recovery marker too large");
    let gate: Value = serde_json::from_slice(&bytes)?;
    ensure!(
        gate["format"] == 1
            && gate["state"] == "reconciliation_required"
            && gate["manifest_sha256"] == manifest_sha256
            && gate["original_state"].as_str() == original_state.to_str(),
        "recovery marker identity mismatch"
    );
    let path = root.join("telos.db");
    ensure!(
        std::fs::symlink_metadata(&path)?.is_file(),
        "recovery database must be regular"
    );
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        digest.update(&buffer[..n]);
    }
    ensure!(
        format!("{:x}", digest.finalize()) == database_sha256,
        "recovery database changed before preparation"
    );
    for suffix in ["telos.db-wal", "telos.db-shm"] {
        ensure!(
            !root.join(suffix).try_exists()?,
            "recovery copy has SQLite sidecars"
        );
    }
    ensure!(
        original_state.is_absolute(),
        "original state must be absolute"
    );
    Ok(())
}

pub enum OperatorRecoveryAction {
    Review(telos_core::RecoveryProjectReview),
    AdoptWorkspace(telos_core::RecoveryWorkspaceAdoption),
    Discard(telos_core::RecoveryExecutionDisposition),
    DiscardVerification(telos_core::RecoveryVerificationDisposition),
    ReconcileEnvironment(telos_core::RecoveryEnvironmentDisposition),
}
impl OperatorRecoveryAction {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Review(r) => r.validate(),
            Self::AdoptWorkspace(a) => a.validate(),
            Self::Discard(d) => d.validate(),
            Self::DiscardVerification(d) => d.validate(),
            Self::ReconcileEnvironment(d) => d.validate(),
        }
    }
    pub fn review(&self) -> &telos_core::RecoveryProjectReview {
        match self {
            Self::Review(r) => r,
            Self::AdoptWorkspace(a) => &a.review,
            Self::Discard(d) => &d.review,
            Self::DiscardVerification(d) => &d.review,
            Self::ReconcileEnvironment(d) => &d.review,
        }
    }
    fn replace_evidence(&mut self, evidence: Vec<telos_core::RecoveryReviewEvidence>) {
        match self {
            Self::Review(r) => r.evidence = evidence,
            Self::AdoptWorkspace(a) => a.review.evidence = evidence,
            Self::Discard(d) => d.review.evidence = evidence,
            Self::DiscardVerification(d) => d.review.evidence = evidence,
            Self::ReconcileEnvironment(d) => d.review.evidence = evidence,
        }
    }
}

/// Mutate only a private candidate whose complete source image was verified by the CLI.
/// Material bytes are supplied explicitly; this boundary never reads operator source paths.
pub async fn apply_operator_action(
    root: &Path,
    database_sha256: &str,
    original_state: &Path,
    mut action: OperatorRecoveryAction,
    materials: Vec<Vec<u8>>,
) -> Result<Value> {
    action.validate()?;
    let review = action.review();
    ensure!(
        materials.len() == review.evidence.len(),
        "review material count mismatch"
    );
    for (reference, bytes) in review.evidence.iter().zip(&materials) {
        ensure!(
            !bytes.is_empty() && bytes.len() <= 8 * 1024 * 1024,
            "review material size invalid"
        );
        ensure!(
            format!("{:x}", Sha256::digest(bytes)) == reference.sha256,
            "review material hash mismatch"
        );
    }
    let _owner = crate::daemon::maintenance_lock(root)?;
    verify_copy(
        root,
        &review.manifest_sha256,
        database_sha256,
        original_state,
    )?;
    let store = Store::open(root).await.context("open review candidate")?;
    let result = async {
        let review = action.review();
        let preparation = store
            .recovery_preparation()
            .await?
            .context("candidate is not prepared")?;
        ensure!(
            preparation["manifest_sha256"] == review.manifest_sha256
                && preparation["original_state"].as_str() == original_state.to_str(),
            "candidate preparation identity mismatch"
        );
        let mut admitted = Vec::new();
        for bytes in materials {
            admitted.push(
                store
                    .record_recovery_review_evidence(
                        &review.project_id,
                        &review.manifest_sha256,
                        &review.decision_id,
                        &bytes,
                    )
                    .await?,
            );
        }
        action.replace_evidence(admitted);
        match action {
            OperatorRecoveryAction::ReconcileEnvironment(disposition) => {
                let result = store.reconcile_recovered_environment(&disposition).await?;
                Ok(serde_json::json!({"disposition":disposition,"result":result}))
            }
            OperatorRecoveryAction::AdoptWorkspace(adoption) => {
                let result = adopt_workspace(&store, &adoption).await?;
                Ok(serde_json::json!({"adoption":adoption,"result":result}))
            }
            OperatorRecoveryAction::Review(review) => {
                let result = store.release_recovery_project(&review).await?;
                Ok(serde_json::json!({"review":review,"result":result}))
            }
            OperatorRecoveryAction::DiscardVerification(disposition) => {
                let result = store.discard_recovered_verifications(&disposition).await?;
                Ok(serde_json::json!({"disposition":disposition,"result":result}))
            }
            OperatorRecoveryAction::Discard(disposition) => {
                let result = store.discard_recovered_executions(&disposition).await?;
                Ok(serde_json::json!({"disposition":disposition,"result":result}))
            }
        }
    }
    .await;
    store.close().await;
    result
}

/// Caller owns the offline candidate's maintenance lock. Never edits Git metadata or files.
pub async fn adopt_workspace(
    store: &Store,
    adoption: &telos_core::RecoveryWorkspaceAdoption,
) -> Result<Value> {
    adoption.validate()?;
    let inspection = inspect_workspace(
        store,
        &adoption.workspace_id,
        Path::new(&adoption.candidate_path),
    )
    .await?;
    ensure!(
        inspection["project_id"] == adoption.review.project_id,
        "workspace belongs to another project"
    );
    ensure!(
        inspection["snapshot_sha256"] == adoption.expected_snapshot_sha256,
        "candidate changed since operator inspection"
    );
    let expected = serde_json::from_value(inspection["recorded_workspace"].clone())?;
    store
        .adopt_recovered_workspace(adoption, &expected, &inspection["snapshot"])
        .await
}

/// Observe explicitly selected external worktree files without changing Git or the ledger.
pub async fn inspect_workspace(store: &Store, workspace_id: &str, path: &Path) -> Result<Value> {
    let context = store.recovery_workspace_context(workspace_id).await?;
    let expected: telos_core::DeliveryWorkspace =
        serde_json::from_value(context["workspace"].clone())?;
    ensure!(path.canonicalize()? == path, "candidate must be canonical");
    let project = Path::new(
        context["project_root"]
            .as_str()
            .context("project root missing")?,
    );
    ensure!(
        !path.starts_with(project) && !project.starts_with(path),
        "candidate overlaps original project"
    );
    let mut candidate = expected.clone();
    candidate.path = path.to_str().context("candidate must be UTF-8")?.to_owned();
    let mut extra_paths = Vec::new();
    if !context["verification_plan"].is_null() {
        let plan: telos_core::VerificationPlan =
            serde_json::from_value(context["verification_plan"]["plan"].clone())?;
        extra_paths.extend(plan.checks.into_iter().map(|c| c.relative_path));
    }
    let observed = crate::workspace::snapshot(&candidate, &extra_paths).await?;
    let after = crate::workspace::snapshot(&candidate, &extra_paths).await?;
    ensure!(
        after == observed,
        "candidate changed during workspace inspection"
    );
    Ok(
        serde_json::json!({"project_id":context["project_id"],"recorded_workspace":expected,"candidate_path":candidate.path,"snapshot_sha256":format!("{:x}",Sha256::digest(serde_json::to_vec(&observed)?)),"snapshot":observed,"observed_at":telos_store::now(),"execution_enabled":false,"scope":"Current Git-visible files and configured acceptance paths; not backup equality, operator authorization or task acceptance."}),
    )
}

/// Isolate historical process journals without interpreting or acting on their identities.
pub async fn archive_runs(
    root: &Path,
    manifest: &str,
    database_sha256: &str,
    original_state: &Path,
    tree_sha256: &str,
    file_count: usize,
) -> Result<Value> {
    use std::os::unix::fs::PermissionsExt;
    let _owner = crate::daemon::maintenance_lock(root)?;
    verify_copy(root, manifest, database_sha256, original_state)?;
    let store = Store::open(root).await?;
    let result = async {
        let inventory = store.recovery_activation_inventory().await?;
        ensure!(
            inventory["counts"]["unfinished_steps"] == 0
                && inventory["counts"]["unfinished_verifications"] == 0
                && inventory["counts"]["external_environment_sessions"] == 0,
            "resolve execution and verification uncertainty before archiving run evidence"
        );
        let source = root.join("runs");
        ensure!(
            std::fs::symlink_metadata(&source)?.is_dir() && source.canonicalize()? == source,
            "historical run namespace must be a regular canonical directory"
        );
        let history = root.join("recovery-history");
        std::fs::create_dir_all(&history)?;
        ensure!(
            history.canonicalize()? == history,
            "recovery history directory changed"
        );
        std::fs::set_permissions(&history, std::fs::Permissions::from_mode(0o700))?;
        let destination = history.join("original-runs");
        ensure!(
            std::fs::symlink_metadata(&destination)
                .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
            "archive destination already exists or is inaccessible"
        );
        std::fs::rename(&source, &destination)?;
        std::fs::File::open(&destination)?.sync_all()?;
        std::fs::File::open(&history)?.sync_all()?;
        std::fs::File::open(root)?.sync_all()?;
        store
            .record_recovery_run_archive(manifest, tree_sha256, file_count)
            .await
    }
    .await;
    store.close().await;
    result
}

/// Static publication proof, valid across later ordinary task/ledger changes.
pub async fn verify_activation_publication(
    store: &Store,
    root: &Path,
    require_target: bool,
) -> Result<Value> {
    let record = store
        .recovery_activation()
        .await?
        .context("recovery activation authorization missing")?;
    let decision: telos_core::RecoveryActivation =
        serde_json::from_value(record["decision"].clone())?;
    decision.validate()?;
    ensure!(
        store
            .recovery_preparation()
            .await?
            .context("recovery preparation missing")?["manifest_sha256"]
            == decision.manifest_sha256,
        "activation snapshot mismatch"
    );
    let path = root.join("recovery-activated.json");
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    ensure!(
        file.metadata()?.is_file(),
        "activation publication must be regular"
    );
    let mut bytes = Vec::new();
    file.take(65537).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 65536, "activation publication too large");
    let marker: Value = serde_json::from_slice(&bytes)?;
    ensure!(
        marker == activation_marker(&record)?,
        "activation publication proof mismatch"
    );
    if require_target {
        ensure!(
            root.canonicalize()?.to_str() == Some(decision.target_state.as_str()),
            "activation is bound to a different state directory"
        );
    }
    Ok(record)
}
fn activation_marker(record: &Value) -> Result<Value> {
    Ok(
        serde_json::json!({"format":1,"target_state":record["decision"]["target_state"],"decision_sha256":format!("{:x}",Sha256::digest(serde_json::to_vec(&record["decision"])?)),"preflight_sha256":format!("{:x}",Sha256::digest(serde_json::to_vec(&record["preflight"])?))}),
    )
}

/// Authorize a private copy and seal its eventual target binding. Does not publish/start it.
pub async fn authorize_activation(
    root: &Path,
    database_sha256: &str,
    original_state: &Path,
    decision: &telos_core::RecoveryActivation,
    preflight: &Value,
) -> Result<Value> {
    use std::io::Write;
    decision.validate()?;
    let _owner = crate::daemon::maintenance_lock(root)?;
    verify_copy(
        root,
        &decision.manifest_sha256,
        database_sha256,
        original_state,
    )?;
    let store = Store::open(root).await?;
    let result = async {
        store
            .authorize_recovery_activation(decision, preflight)
            .await?;
        let record = store
            .recovery_activation()
            .await?
            .context("activation record missing")?;
        let mut marker = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(root.join("recovery-activated.json"))?;
        marker.write_all(&serde_json::to_vec_pretty(&activation_marker(&record)?)?)?;
        marker.sync_all()?;
        std::fs::File::open(root)?.sync_all()?;
        verify_activation_publication(&store, root, false).await
    }
    .await;
    store.close().await;
    result
}
