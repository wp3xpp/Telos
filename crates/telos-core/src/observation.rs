//! Provider-neutral observations. Business interpretation belongs to project adapters.
use crate::Verdict;
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub const ADAPTER: &str = "local.observation_json";
pub const CONTEXT_ENV: &str = "TELOS_OBSERVATION_CONTEXT";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservationPolicy {
    pub source_id: String,
    pub window_seconds: i64,
    pub max_age_seconds: i64,
    #[serde(default = "default_poll")]
    pub poll_interval_seconds: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_stage: Option<String>,
}
fn default_poll() -> i64 {
    30
}
impl ObservationPolicy {
    pub fn validate(&self) -> Result<()> {
        identifier(&self.source_id)?;
        ensure!(
            (5..=3600).contains(&self.poll_interval_seconds),
            "invalid observation poll interval"
        );
        if let Some(stage) = &self.after_stage {
            identifier(stage)?;
        }
        ensure!(
            (5..=86400).contains(&self.window_seconds),
            "observation window must be 5..86400 seconds"
        );
        ensure!(
            (1..=3600).contains(&self.max_age_seconds),
            "observation age must be 1..3600 seconds"
        );
        Ok(())
    }
}
fn identifier(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control),
        "invalid observation identifier"
    );
    Ok(())
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    pub loop_id: String,
    pub revision: i64,
    pub cycle_id: String,
    pub request_id: String,
    pub criterion_id: String,
    pub source_id: String,
    pub window_start: i64,
    pub window_end: i64,
    pub not_before: Option<i64>,
    pub subject_id: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Coverage {
    pub start: i64,
    pub end: i64,
    pub complete: bool,
    pub gap: bool,
    pub backlog_bytes: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Signal {
    pub resource_key: String,
    pub episode_key: String,
    pub kind: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Assertion {
    pub binding: Binding,
    pub observed_at: i64,
    pub coverage: Coverage,
    pub verdict: Verdict,
    pub summary: String,
    /// Must cite actual project measurements, not the absence of log events.
    pub evidence: Vec<String>,
    #[serde(default)]
    pub signals: Vec<Signal>,
}
impl Assertion {
    /// Returns unique physical-resource/episode/kind identities. Logical consumer
    /// fanout is deliberately not a framework incident dimension.
    pub fn evaluate(
        &self,
        expected: &Binding,
        policy: &ObservationPolicy,
        now: i64,
    ) -> Result<(Verdict, usize)> {
        policy.validate()?;
        ensure!(
            &self.binding == expected,
            "observation source/cycle/request binding mismatch"
        );
        ensure!(
            self.summary.len() <= 4096 && !self.summary.trim().is_empty(),
            "observation summary required (max 4096 bytes)"
        );
        ensure!(
            self.signals.len() <= 4096 && self.evidence.len() <= 64,
            "observation item bound exceeded"
        );
        let mut signals = BTreeSet::new();
        for signal in &self.signals {
            identifier(&signal.resource_key)?;
            identifier(&signal.episode_key)?;
            identifier(&signal.kind)?;
            signals.insert((&signal.resource_key, &signal.episode_key, &signal.kind));
        }
        ensure!(
            self.coverage.start >= 0
                && self.coverage.start <= self.coverage.end
                && self.observed_at >= 0,
            "invalid observation time range"
        );
        for item in &self.evidence {
            ensure!(
                !item.trim().is_empty() && item.len() <= 2048,
                "invalid observation evidence reference"
            );
        }
        let fresh = self.observed_at <= now + 5
            && self.observed_at >= now - policy.max_age_seconds
            && self.coverage.end <= self.observed_at + 5
            && self.coverage.end >= expected.window_end - policy.max_age_seconds;
        let covered = self.coverage.complete
            && !self.coverage.gap
            && self.coverage.backlog_bytes == 0
            && self.coverage.start <= expected.window_start
            && self.coverage.end - self.coverage.start >= policy.window_seconds
            && expected
                .not_before
                .is_none_or(|start| self.coverage.start >= start);
        let verdict = if !fresh || !covered || self.evidence.is_empty() {
            Verdict::Inconclusive
        } else {
            self.verdict
        };
        Ok((verdict, signals.len()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (Binding, ObservationPolicy, Assertion) {
        let b = Binding {
            loop_id: "goal".into(),
            revision: 1,
            cycle_id: "cycle".into(),
            request_id: "lease".into(),
            criterion_id: "health".into(),
            source_id: "source".into(),
            window_start: 100,
            window_end: 200,
            not_before: None,
            subject_id: None,
        };
        let p = ObservationPolicy {
            source_id: "source".into(),
            window_seconds: 100,
            max_age_seconds: 10,
            poll_interval_seconds: 30,
            after_stage: None,
        };
        let a = Assertion {
            binding: b.clone(),
            observed_at: 200,
            coverage: Coverage {
                start: 100,
                end: 200,
                complete: true,
                gap: false,
                backlog_bytes: 0,
            },
            verdict: Verdict::Passed,
            summary: "project predicate measured".into(),
            evidence: vec!["measurement-sha256".into()],
            signals: vec![],
        };
        (b, p, a)
    }
    #[test]
    fn missing_data_backlog_gaps_stale_windows_and_empty_measurements_never_pass() -> Result<()> {
        let (b, p, a) = fixture();
        assert_eq!(a.evaluate(&b, &p, 200)?.0, Verdict::Passed);
        let mut cases = vec![];
        let mut v = a.clone();
        v.coverage.backlog_bytes = 1;
        cases.push(v);
        let mut v = a.clone();
        v.coverage.gap = true;
        cases.push(v);
        let mut v = a.clone();
        v.coverage.complete = false;
        cases.push(v);
        let mut v = a.clone();
        v.coverage.start = 101;
        cases.push(v);
        let mut v = a.clone();
        v.observed_at = 100;
        cases.push(v);
        let mut v = a.clone();
        v.evidence.clear();
        cases.push(v);
        for v in cases {
            assert_eq!(v.evaluate(&b, &p, 200)?.0, Verdict::Inconclusive);
        }
        let mut other = b.clone();
        other.request_id = "old-request".into();
        assert!(a.evaluate(&other, &p, 200).is_err());
        Ok(())
    }
    #[test]
    fn fanout_deduplicates_physical_resource_and_recovery_episode() -> Result<()> {
        let (b, p, mut a) = fixture();
        a.verdict = Verdict::Failed;
        for _ in 0..111 {
            a.signals.push(Signal {
                resource_key: "physical-A".into(),
                episode_key: "recovery-1".into(),
                kind: "disconnect".into(),
            });
        }
        assert_eq!(a.evaluate(&b, &p, 200)?, (Verdict::Failed, 1));
        a.signals.push(Signal {
            resource_key: "physical-A".into(),
            episode_key: "recovery-2".into(),
            kind: "disconnect".into(),
        });
        assert_eq!(a.evaluate(&b, &p, 200)?.1, 2);
        Ok(())
    }
}

/// A project collector supplies normalized metadata, never a health verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cursor {
    pub generation: String,
    pub offset: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Batch {
    pub source_id: String,
    pub request_key: String,
    pub expected_version: i64,
    pub generation: String,
    pub start: u64,
    pub end: u64,
    pub source_size: u64,
    pub gap_reason: Option<String>,
    pub signals: Vec<Signal>,
    pub evidence: Vec<String>,
}
impl Batch {
    pub fn validate(&self) -> Result<()> {
        identifier(&self.source_id)?;
        identifier(&self.request_key)?;
        identifier(&self.generation)?;
        ensure!(
            self.expected_version >= 0
                && self.start <= self.end
                && self.end <= self.source_size
                && self.source_size <= i64::MAX as u64
                && self.end - self.start <= 256 * 1024,
            "invalid or unbounded collection range"
        );
        ensure!(
            self.signals.len() <= 4096 && self.evidence.len() <= 64,
            "collection item bound exceeded"
        );
        for s in &self.signals {
            identifier(&s.resource_key)?;
            identifier(&s.episode_key)?;
            identifier(&s.kind)?;
        }
        for s in &self.evidence {
            ensure!(
                !s.trim().is_empty() && s.len() <= 2048,
                "invalid collection evidence reference"
            );
        }
        if let Some(reason) = &self.gap_reason {
            ensure!(
                !reason.trim().is_empty() && reason.len() <= 2048,
                "gap reason required"
            );
        }
        Ok(())
    }
}
/// Collection cadence is independent of the goal's analysis interval. Project
/// collectors can drain multiple bounded ranges per tick without unbounded work.
#[derive(Debug, Clone, Copy)]
pub struct CatchUpBudget {
    pub max_batches: u32,
    pub max_bytes: u64,
}
impl CatchUpBudget {
    pub fn next_limit(&self, batches: u32, bytes: u64, backlog_bytes: u64) -> Result<u64> {
        if !self.should_continue(batches, bytes, backlog_bytes)? {
            return Ok(0);
        }
        Ok((self.max_bytes - bytes).min(backlog_bytes).min(256 * 1024))
    }
    pub fn should_continue(&self, batches: u32, bytes: u64, backlog_bytes: u64) -> Result<bool> {
        ensure!(
            (1..=1024).contains(&self.max_batches)
                && self.max_bytes > 0
                && self.max_bytes <= 256 * 1024 * 1024,
            "invalid collection budget"
        );
        Ok(backlog_bytes > 0 && batches < self.max_batches && bytes < self.max_bytes)
    }
}

/// Trusted operator attestation from a project-owned action driver. Recording it
/// never executes an action or establishes independent recovery.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StageReceipt {
    pub request_key: String,
    pub stage: String,
    pub artifact: String,
    pub subject_id: String,
    pub outcome: StageOutcome,
    pub evidence_refs: Vec<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StageOutcome {
    Succeeded,
    Failed,
    OutcomeUnknown,
}
impl StageReceipt {
    pub fn validate(&self) -> Result<()> {
        identifier(&self.request_key)?;
        identifier(&self.stage)?;
        identifier(&self.subject_id)?;
        ensure!(
            !self.artifact.is_empty() && self.artifact.len() <= 512,
            "stage artifact required"
        );
        ensure!(
            !self.evidence_refs.is_empty() && self.evidence_refs.len() <= 64,
            "stage evidence references required"
        );
        for value in &self.evidence_refs {
            ensure!(
                !value.trim().is_empty() && value.len() <= 2048,
                "invalid stage evidence reference"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod stage_tests {
    use super::*;
    #[test]
    fn post_stage_window_cannot_reuse_pre_stage_coverage_or_subject() -> Result<()> {
        let policy = ObservationPolicy {
            source_id: "source".into(),
            window_seconds: 300,
            max_age_seconds: 30,
            poll_interval_seconds: 30,
            after_stage: Some("activate".into()),
        };
        let binding = Binding {
            loop_id: "goal".into(),
            revision: 1,
            cycle_id: "cycle".into(),
            request_id: "lease".into(),
            criterion_id: "health".into(),
            source_id: "source".into(),
            window_start: 100,
            window_end: 400,
            not_before: Some(200),
            subject_id: Some("artifact-live-2".into()),
        };
        let mut assertion = Assertion {
            binding: binding.clone(),
            observed_at: 400,
            coverage: Coverage {
                start: 100,
                end: 400,
                complete: true,
                gap: false,
                backlog_bytes: 0,
            },
            verdict: Verdict::Passed,
            summary: "project checks passed".into(),
            evidence: vec!["current-measurement".into()],
            signals: vec![],
        };
        assert_eq!(
            assertion.evaluate(&binding, &policy, 400)?.0,
            Verdict::Inconclusive
        );
        let mut next = binding.clone();
        next.window_start = 200;
        next.window_end = 500;
        assertion.binding = next.clone();
        assertion.observed_at = 500;
        assertion.coverage.start = 200;
        assertion.coverage.end = 500;
        assert_eq!(assertion.evaluate(&next, &policy, 500)?.0, Verdict::Passed);
        assertion.binding.subject_id = Some("artifact-live-1".into());
        assert!(assertion.evaluate(&next, &policy, 500).is_err());
        Ok(())
    }
    #[test]
    fn catchup_continues_until_caught_up_or_budget_without_jumping_offsets() -> Result<()> {
        let budget = CatchUpBudget {
            max_batches: 32,
            max_bytes: 8 * 1024 * 1024,
        };
        assert_eq!(budget.next_limit(0, 0, 20 * 1024 * 1024)?, 256 * 1024);
        assert_eq!(budget.next_limit(31, 8 * 1024 * 1024 - 7, 500)?, 7);
        assert_eq!(budget.next_limit(32, 8 * 1024 * 1024, 500)?, 0);
        assert_eq!(budget.next_limit(1, 256 * 1024, 0)?, 0);
        Ok(())
    }
}
