pub mod observation;
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

pub mod duration;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LoopKind {
    Delivery,
    Continuous,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LoopState {
    Active,
    Paused,
    Blocked,
    Completed,
    Stopped,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Criterion {
    pub id: String,
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Contract {
    pub goal: String,
    pub delivery_scope: String,
    pub criteria: Vec<Criterion>,
    /// An observation interval, not an unrestricted permission to act.
    pub interval_seconds: Option<u64>,
    pub max_attempts: u32,
}

impl Contract {
    pub fn validate(&self, kind: LoopKind) -> Result<()> {
        ensure!(!self.goal.trim().is_empty(), "goal is required");
        ensure!(
            !self.delivery_scope.trim().is_empty(),
            "delivery_scope is required"
        );
        ensure!(
            !self.criteria.is_empty(),
            "at least one acceptance criterion is required"
        );
        ensure!(
            (1..=20).contains(&self.max_attempts),
            "max_attempts must be 1..20"
        );
        let mut ids = HashSet::new();
        for c in &self.criteria {
            ensure!(
                !c.id.trim().is_empty() && !c.description.trim().is_empty(),
                "empty criterion"
            );
            ensure!(ids.insert(&c.id), "duplicate criterion id");
        }
        match kind {
            LoopKind::Continuous => ensure!(
                self.interval_seconds
                    .is_some_and(|s| (30..=2_592_000).contains(&s)),
                "continuous loops require interval_seconds in 30..2592000"
            ),
            LoopKind::Delivery => ensure!(
                self.interval_seconds.is_none(),
                "delivery loops have no recurring interval"
            ),
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoopRecord {
    pub id: String,
    pub project_id: String,
    pub name: String,
    pub kind: LoopKind,
    pub state: LoopState,
    pub revision: i64,
    pub version: i64,
    pub contract: Contract,
    pub next_due_at: Option<i64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Passed,
    Failed,
    Inconclusive,
    Error,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Sandbox {
    ReadOnly,
    WorkspaceWrite,
}

/// Operator-owned limits for future goal-managed execution. Not an OS sandbox.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProjectExecutionPolicy {
    pub enabled: bool,
    pub sandbox: Sandbox,
    pub timeout_seconds: u64,
    pub max_output_bytes: u64,
    pub max_steps_per_cycle: u32,
}

impl ProjectExecutionPolicy {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1..=3600).contains(&self.timeout_seconds),
            "policy timeout must be 1..3600 seconds"
        );
        ensure!(
            (1024..=16 * 1024 * 1024).contains(&self.max_output_bytes),
            "invalid policy output limit"
        );
        ensure!(
            (1..=20).contains(&self.max_steps_per_cycle),
            "policy step limit must be 1..20"
        );
        Ok(())
    }

    pub fn permits(&self, spec: &ExecutionSpec) -> Result<()> {
        self.validate()?;
        spec.validate()?;
        ensure!(self.enabled, "project execution is disabled");
        ensure!(
            self.sandbox == Sandbox::WorkspaceWrite || spec.sandbox == Sandbox::ReadOnly,
            "execution sandbox exceeds project policy"
        );
        ensure!(
            spec.timeout_seconds <= self.timeout_seconds,
            "execution timeout exceeds project policy"
        );
        ensure!(
            spec.max_output_bytes <= self.max_output_bytes,
            "execution output limit exceeds project policy"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionSpec {
    pub task: String,
    pub cwd: String,
    pub sandbox: Sandbox,
    pub timeout_seconds: u64,
    pub max_output_bytes: u64,
}

impl ExecutionSpec {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.task.trim().is_empty() && self.task.len() <= 64 * 1024,
            "invalid task size"
        );
        ensure!(
            std::path::Path::new(&self.cwd).is_absolute(),
            "cwd must be absolute"
        );
        ensure!(
            (1..=3600).contains(&self.timeout_seconds),
            "timeout must be 1..3600 seconds"
        );
        ensure!(
            (1024..=16 * 1024 * 1024).contains(&self.max_output_bytes),
            "invalid output byte limit"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobLease {
    pub step_id: String,
    pub loop_id: String,
    pub cycle_id: String,
    pub contract_revision: i64,
    pub generation: i64,
    pub worker_id: String,
    pub lease_until: i64,
    #[serde(default)]
    pub workspace_id: Option<String>,
    pub spec: ExecutionSpec,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionOutcome {
    Succeeded,
    Failed,
    OutcomeUnknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CriterionResult {
    pub criterion_id: String,
    pub verdict: Verdict,
    pub evidence_ids: Vec<String>,
    pub explanation: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Verification {
    pub cycle_id: String,
    pub contract_revision: i64,
    pub artifact: String,
    pub environment: String,
    pub observation_complete: bool,
    pub results: Vec<CriterionResult>,
}

/// Deterministic acceptance adapters. Only operators configure expected results.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationPlan {
    #[serde(default)]
    pub checks: Vec<FileHashCheck>,
    #[serde(default)]
    pub log_checks: Vec<LogWindowCheck>,
    #[serde(default)]
    pub test_checks: Vec<TestCheck>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestCheck {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation: Option<observation::ObservationPolicy>,
    pub criterion_id: String,
    pub command: TestCommand,
    #[serde(
        default = "default_test_adapter",
        skip_serializing_if = "is_default_test_adapter"
    )]
    pub adapter: String,
}

pub fn default_test_adapter() -> String {
    "local.sandbox_command".into()
}
fn is_default_test_adapter(value: &str) -> bool {
    value == "local.sandbox_command"
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogWindowCheck {
    pub criterion_id: String,
    pub relative_path: String,
    pub window_seconds: i64,
    pub max_gap_seconds: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileHashCheck {
    pub criterion_id: String,
    pub relative_path: String,
    pub expected_sha256: String,
}

impl VerificationPlan {
    pub fn validate(&self, contract: &Contract) -> Result<()> {
        ensure!(
            self.checks.len() + self.log_checks.len() + self.test_checks.len() > 0
                && self.checks.len() + self.log_checks.len() + self.test_checks.len() <= 100,
            "verification requires 1..100 checks"
        );
        let known: HashSet<_> = contract.criteria.iter().map(|c| c.id.as_str()).collect();
        let mut seen = HashSet::new();
        for check in &self.checks {
            ensure!(
                known.contains(check.criterion_id.as_str()) && seen.insert(&check.criterion_id),
                "unknown or duplicate verifier criterion"
            );
            let path = std::path::Path::new(&check.relative_path);
            ensure!(
                !check.relative_path.is_empty()
                    && path
                        .components()
                        .all(|c| matches!(c, std::path::Component::Normal(_))),
                "verifier paths must be project-relative without traversal"
            );
            ensure!(
                check.expected_sha256.len() == 64
                    && check
                        .expected_sha256
                        .bytes()
                        .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)),
                "expected hash must be lowercase SHA256"
            );
        }
        for check in &self.log_checks {
            ensure!(
                known.contains(check.criterion_id.as_str()) && seen.insert(&check.criterion_id),
                "unknown or duplicate verifier criterion"
            );
            ensure!(
                !check.relative_path.is_empty()
                    && std::path::Path::new(&check.relative_path)
                        .components()
                        .all(|c| matches!(c, std::path::Component::Normal(_))),
                "log path must be project-relative without traversal"
            );
            ensure!(
                (5..=86400).contains(&check.window_seconds)
                    && (5..=check.window_seconds).contains(&check.max_gap_seconds),
                "invalid log observation window or heartbeat gap"
            );
        }
        ensure!(
            self.test_checks.len() <= 16,
            "at most 16 test commands per verification"
        );
        ensure!(
            self.test_checks
                .iter()
                .map(|c| c.command.timeout_seconds as u128)
                .sum::<u128>()
                <= 3600,
            "total test timeout exceeds one hour"
        );
        for check in &self.test_checks {
            ensure!(
                !check.adapter.is_empty()
                    && check.adapter.len() <= 128
                    && check
                        .adapter
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)),
                "invalid test verifier adapter ID"
            );
            ensure!(
                known.contains(check.criterion_id.as_str()) && seen.insert(&check.criterion_id),
                "unknown or duplicate verifier criterion"
            );
            check.command.validate()?;
            match (&check.observation, check.adapter.as_str()) {
                (Some(policy), "local.observation_json") => policy.validate()?,
                (None, "local.observation_json") => {
                    anyhow::bail!("observation adapter requires a coverage policy")
                }
                (None, _) => {}
                (Some(_), _) => anyhow::bail!("observation policy requires local.observation_json"),
            }
        }
        ensure!(
            seen.len() == known.len(),
            "every criterion requires an independent check"
        );
        Ok(())
    }
}

/// A model's completion message cannot satisfy these evidence obligations.
pub fn evaluate(contract: &Contract, verification: &Verification) -> Result<Verdict> {
    ensure!(
        !verification.artifact.trim().is_empty(),
        "artifact identity required"
    );
    ensure!(
        !verification.environment.trim().is_empty(),
        "environment identity required"
    );
    let known: HashSet<_> = contract.criteria.iter().map(|c| c.id.as_str()).collect();
    let mut seen = HashSet::new();
    for r in &verification.results {
        ensure!(known.contains(r.criterion_id.as_str()), "unknown criterion");
        ensure!(
            seen.insert(r.criterion_id.as_str()),
            "duplicate criterion result"
        );
        ensure!(
            !r.explanation.trim().is_empty(),
            "verification explanation required"
        );
        if r.verdict == Verdict::Passed {
            ensure!(
                !r.evidence_ids.is_empty(),
                "passed criterion requires evidence"
            );
        }
    }
    if verification
        .results
        .iter()
        .any(|r| r.verdict == Verdict::Failed)
    {
        return Ok(Verdict::Failed);
    }
    if verification
        .results
        .iter()
        .any(|r| r.verdict == Verdict::Error)
    {
        return Ok(Verdict::Error);
    }
    if !verification.observation_complete
        || seen.len() != known.len()
        || verification
            .results
            .iter()
            .any(|r| r.verdict == Verdict::Inconclusive)
    {
        return Ok(Verdict::Inconclusive);
    }
    Ok(Verdict::Passed)
}

/// Ledger-owned detached Git worktree; not an OS sandbox or acceptance result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeliveryWorkspace {
    pub id: String,
    pub cycle_id: String,
    pub contract_revision: i64,
    pub policy_revision: i64,
    pub path: String,
    pub repository_git_dir: String,
    pub base_commit: String,
    pub state: String,
    pub last_error: Option<String>,
}

/// Operator-owned local test command; arguments are never interpreted by a shell.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestCommand {
    pub program: String,
    pub args: Vec<String>,
    pub readable_roots: Vec<String>,
    pub timeout_seconds: u64,
    pub max_output_bytes: u64,
}
impl TestCommand {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            std::path::Path::new(&self.program).is_absolute(),
            "test executable must be absolute"
        );
        ensure!(
            self.args.len() <= 128
                && self
                    .args
                    .iter()
                    .all(|a| a.len() <= 8192 && !a.contains('\0'))
                && self.args.iter().map(String::len).sum::<usize>() <= 32768,
            "test arguments exceed limits"
        );
        ensure!(
            self.readable_roots.len() <= 16
                && self
                    .readable_roots
                    .iter()
                    .all(|p| std::path::Path::new(p).is_absolute()),
            "test runtime read roots must be absolute and bounded"
        );
        ensure!(
            (1..=3600).contains(&self.timeout_seconds),
            "test timeout must be 1..3600 seconds"
        );
        ensure!(
            (1024..=8 * 1024 * 1024).contains(&self.max_output_bytes),
            "test output limit must be 1KiB..8MiB"
        );
        Ok(())
    }
}

/// Trusted operator review, not model-authored acceptance or proof of task success.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryProjectReview {
    pub project_id: String,
    pub manifest_sha256: String,
    pub decision_id: String,
    pub operator: String,
    pub statement: String,
    pub reviewed_external_state: bool,
    pub evidence: Vec<RecoveryReviewEvidence>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryReviewEvidence {
    pub id: String,
    pub sha256: String,
}
impl RecoveryProjectReview {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.project_id.trim().is_empty() && self.project_id.len() <= 256,
            "invalid recovery project id"
        );
        ensure!(
            !self.decision_id.is_empty()
                && self.decision_id.len() <= 128
                && self
                    .decision_id
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'),
            "invalid recovery decision id"
        );
        ensure!(
            !self.operator.trim().is_empty() && self.operator.len() <= 256,
            "operator attribution required"
        );
        ensure!(
            !self.statement.trim().is_empty() && self.statement.len() <= 8192,
            "bounded operator review statement required"
        );
        ensure!(
            self.reviewed_external_state,
            "external state review must be explicit"
        );
        ensure!(
            (1..=16).contains(&self.evidence.len()),
            "recovery review needs 1..16 evidence references"
        );
        let mut ids = HashSet::new();
        for reference in &self.evidence {
            ensure!(
                !reference.id.is_empty()
                    && reference.id.len() <= 256
                    && !reference.id.contains(['/', '\\'])
                    && ids.insert(&reference.id),
                "invalid or duplicate recovery evidence id"
            );
        }
        for hash in
            std::iter::once(&self.manifest_sha256).chain(self.evidence.iter().map(|r| &r.sha256))
        {
            ensure!(
                hash.len() == 64 && hash.bytes().all(|c| c.is_ascii_hexdigit()),
                "recovery digest must be SHA256"
            );
        }
        Ok(())
    }
}

/// Explicit operator disposition of recovered executions, never task acceptance.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryExecutionDisposition {
    pub review: RecoveryProjectReview,
    pub step_ids: Vec<String>,
    pub processes_confirmed_stopped: bool,
    pub external_effects_reconciled: bool,
}
impl RecoveryExecutionDisposition {
    pub fn validate(&self) -> Result<()> {
        self.review.validate()?;
        ensure!(
            self.processes_confirmed_stopped && self.external_effects_reconciled,
            "explicit process-stop and external-effect reconciliation required"
        );
        ensure!(
            (1..=100).contains(&self.step_ids.len()),
            "disposition needs 1..100 explicit steps"
        );
        let mut seen = HashSet::new();
        for id in &self.step_ids {
            ensure!(
                !id.trim().is_empty() && id.len() <= 256 && seen.insert(id),
                "invalid or duplicate disposition step"
            );
        }
        Ok(())
    }
}

/// Explicit operator disposition of recovered verifications, never task acceptance.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryVerificationDisposition {
    pub review: RecoveryProjectReview,
    pub lease_ids: Vec<String>,
    pub processes_confirmed_stopped: bool,
    pub external_effects_reconciled: bool,
}
impl RecoveryVerificationDisposition {
    pub fn validate(&self) -> Result<()> {
        self.review.validate()?;
        ensure!(
            self.processes_confirmed_stopped && self.external_effects_reconciled,
            "explicit process-stop and external-effect reconciliation required"
        );
        ensure!(
            (1..=100).contains(&self.lease_ids.len()),
            "disposition needs 1..100 explicit verification leases"
        );
        let mut seen = HashSet::new();
        for id in &self.lease_ids {
            ensure!(
                !id.trim().is_empty() && id.len() <= 256 && seen.insert(id),
                "invalid or duplicate disposition verification lease"
            );
        }
        Ok(())
    }
}

/// Operator-reviewed worktree binding; never an acceptance result or new write grant.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryWorkspaceAdoption {
    pub review: RecoveryProjectReview,
    pub workspace_id: String,
    pub candidate_path: String,
    pub expected_snapshot_sha256: String,
    pub exclusive_access_confirmed: bool,
}
impl RecoveryWorkspaceAdoption {
    pub fn validate(&self) -> Result<()> {
        self.review.validate()?;
        ensure!(
            !self.workspace_id.is_empty() && self.workspace_id.len() <= 256,
            "workspace identity required"
        );
        ensure!(
            std::path::Path::new(&self.candidate_path).is_absolute()
                && self.candidate_path.len() <= 4096,
            "absolute bounded candidate path required"
        );
        ensure!(
            self.expected_snapshot_sha256.len() == 64
                && self
                    .expected_snapshot_sha256
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit()),
            "workspace snapshot SHA256 required"
        );
        ensure!(
            self.exclusive_access_confirmed,
            "exclusive workspace access must be confirmed independently"
        );
        Ok(())
    }
}

/// Explicit authorization for one new restored state location; goals remain paused.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryActivation {
    pub manifest_sha256: String,
    pub source_receipt_sha256: String,
    pub target_state: String,
    pub decision_id: String,
    pub operator: String,
    pub statement: String,
    pub source_instance_retired: bool,
    pub exclusive_access_confirmed: bool,
}
impl RecoveryActivation {
    pub fn validate(&self) -> Result<()> {
        for hash in [&self.manifest_sha256, &self.source_receipt_sha256] {
            ensure!(
                hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()),
                "activation fingerprints must be SHA256"
            );
        }
        let path = std::path::Path::new(&self.target_state);
        ensure!(
            path.is_absolute()
                && path.file_name().is_some()
                && path
                    .components()
                    .collect::<std::path::PathBuf>()
                    .as_os_str()
                    == path.as_os_str()
                && self.target_state.len() <= 4096
                && path.components().all(|c| matches!(
                    c,
                    std::path::Component::RootDir | std::path::Component::Normal(_)
                )),
            "activation needs an absolute normalized target state path"
        );
        ensure!(
            !self.decision_id.is_empty()
                && self.decision_id.len() <= 128
                && self
                    .decision_id
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'),
            "invalid activation decision ID"
        );
        ensure!(
            !self.operator.trim().is_empty()
                && self.operator.len() <= 256
                && !self.statement.trim().is_empty()
                && self.statement.len() <= 8192,
            "bounded operator attribution and statement required"
        );
        ensure!(
            self.source_instance_retired && self.exclusive_access_confirmed,
            "source retirement and exclusive access require explicit independent confirmation"
        );
        Ok(())
    }
}

/// Trusted operator attestation, not a model-authored ownership claim.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentReconciliation {
    pub loop_id: String,
    pub revision: i64,
    pub session_id: String,
    pub environment_sha256: String,
    pub adapter_sha256: String,
    pub expected_sequence: i64,
    pub expected_version: i64,
    pub decision_id: String,
    pub provider_observed_at: i64,
    pub provider_lease_owner: Option<String>,
    pub provider_operation_active: bool,
    pub remote_operations_stopped: bool,
    pub local_owners_retired: bool,
    pub provider_protocol_reconciled: bool,
    pub operator_note: String,
}
impl EnvironmentReconciliation {
    pub fn validate(&self) -> Result<()> {
        for key in [&self.loop_id, &self.session_id, &self.decision_id] {
            ensure!(
                !key.is_empty() && key.len() <= 128 && !key.chars().any(char::is_control),
                "invalid environment review identity"
            )
        }
        for hash in [&self.environment_sha256, &self.adapter_sha256] {
            ensure!(
                hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()),
                "invalid environment review binding"
            )
        }
        ensure!(
            self.revision > 0
                && self.expected_sequence > 0
                && self.expected_version > 0
                && self.expected_version < i64::MAX,
            "invalid environment review version"
        );
        ensure!(
            !self.operator_note.trim().is_empty() && self.operator_note.len() <= 2048,
            "environment review note required"
        );
        ensure!(
            self.remote_operations_stopped
                && self.local_owners_retired
                && self.provider_protocol_reconciled,
            "explicit process retirement and provider reconciliation required"
        );
        ensure!(
            self.provider_lease_owner.as_deref() != Some(self.session_id.as_str()),
            "provider still reports reviewed session as owner"
        );
        if let Some(owner) = &self.provider_lease_owner {
            ensure!(
                !owner.is_empty() && owner.len() <= 128 && !owner.chars().any(char::is_control),
                "invalid provider owner observation"
            )
        }
        ensure!(
            !self.provider_operation_active || self.provider_lease_owner.is_some(),
            "active provider operation requires an observed owner"
        );
        Ok(())
    }
}

/// Operator material and exact local-session review for an offline recovery copy.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryEnvironmentDisposition {
    pub review: RecoveryProjectReview,
    pub session: EnvironmentReconciliation,
}
impl RecoveryEnvironmentDisposition {
    pub fn validate(&self) -> Result<()> {
        self.review.validate()?;
        self.session.validate()?;
        ensure!(
            self.review.decision_id == self.session.decision_id,
            "environment and recovery decision identities must match"
        );
        Ok(())
    }
}

/// Explicit library-level authority for a project-owned environment adapter.
/// Fingerprints are computed by the adapter; the framework does not interpret hosts or scripts.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentAdapterGrant {
    pub enabled: bool,
    pub environment_sha256: String,
    pub adapter_sha256: String,
}
impl EnvironmentAdapterGrant {
    pub fn validate(&self) -> Result<()> {
        for hash in [&self.environment_sha256, &self.adapter_sha256] {
            ensure!(
                hash.len() == 64
                    && hash
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                "adapter bindings require lowercase SHA256"
            );
        }
        Ok(())
    }
}
