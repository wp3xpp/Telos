//! Independent verifier adapters. Execution-backend results alone do not accept a goal.
use anyhow::{ensure, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    future::Future,
    path::{Path, PathBuf},
};
use telos_core::{TestCheck, Verdict, VerificationPlan};
use tokio::sync::watch;

pub struct StreamEvidence {
    pub name: &'static str,
    pub path: PathBuf,
    pub sha256: String,
}
pub struct TestObservation {
    pub verdict: Verdict,
    pub cleanup_confirmed: bool,
    pub detail: Value,
    pub streams: Vec<StreamEvidence>,
}
pub trait TestVerificationAdapter: Send + Sync {
    fn verify<'a>(
        &'a self,
        check: &'a TestCheck,
        cwd: &'a Path,
        run_dir: &'a Path,
        codex: &'a Path,
        cancel: watch::Receiver<bool>,
        binding: Option<&'a telos_core::observation::Binding>,
    ) -> impl Future<Output = Result<TestObservation>> + Send + 'a;
}

#[derive(Clone, Copy)]
enum Format {
    ExitCode,
    JsonAssertion,
    Observation,
}
struct LocalSandbox {
    format: Format,
}
/// Trusted compiled registry. Selection does not change any execution permissions.
pub const IDS: &[&str] = &[
    "local.sandbox_command",
    "local.sandbox_json",
    "local.observation_json",
];

pub fn resolve(id: &str) -> Result<impl TestVerificationAdapter> {
    let format = match id {
        "local.sandbox_command" => Format::ExitCode,
        "local.sandbox_json" => Format::JsonAssertion,
        "local.observation_json" => Format::Observation,
        _ => anyhow::bail!("test verification adapter is not registered: {id}"),
    };
    Ok(LocalSandbox { format })
}
pub fn validate_plan(plan: &VerificationPlan) -> Result<()> {
    for check in &plan.test_checks {
        let _ = resolve(&check.adapter)?;
    }
    Ok(())
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Assertion {
    criterion_id: String,
    verdict: Verdict,
    summary: String,
}

impl TestVerificationAdapter for LocalSandbox {
    async fn verify<'a>(
        &'a self,
        check: &'a TestCheck,
        cwd: &'a Path,
        run_dir: &'a Path,
        codex: &'a Path,
        cancel: watch::Receiver<bool>,
        binding: Option<&'a telos_core::observation::Binding>,
    ) -> Result<TestObservation> {
        let runner = crate::test_runner::TestRunner {
            codex: codex.to_path_buf(),
        };
        let report = if matches!(self.format, Format::Observation) {
            runner
                .run_observation(
                    &check.command,
                    cwd,
                    run_dir,
                    cancel,
                    binding.context("observation binding required")?,
                )
                .await?
        } else {
            runner.run(&check.command, cwd, run_dir, cancel).await?
        };
        let mut verdict = report.verdict;
        let mut reason = report.reason.clone();
        let mut assertion = Value::Null;
        // A process error/nonzero exit is never overridden by a passing JSON body.
        if matches!(self.format, Format::JsonAssertion) && verdict == Verdict::Passed {
            let interpreted: Result<Assertion> = async {
                let bytes = tokio::fs::read(report.run_dir.join("stdout.log")).await?;
                ensure!(
                    bytes.len() as u64 <= check.command.max_output_bytes
                        && format!("{:x}", Sha256::digest(&bytes)) == report.stdout_sha256,
                    "structured verifier output changed or exceeds bound"
                );
                let value: Assertion = serde_json::from_slice(&bytes)
                    .context("structured verifier requires exactly one JSON assertion")?;
                ensure!(
                    value.criterion_id == check.criterion_id,
                    "structured verifier criterion mismatch"
                );
                ensure!(
                    !value.summary.trim().is_empty() && value.summary.len() <= 4096,
                    "structured verifier summary must contain 1..4096 bytes"
                );
                Ok(value)
            }
            .await;
            match interpreted {
                Ok(value) => {
                    verdict = value.verdict;
                    reason = value.summary.clone();
                    assertion = json!({"criterion_id":value.criterion_id,"verdict":value.verdict,"summary":value.summary});
                }
                Err(error) => {
                    verdict = Verdict::Error;
                    reason = error.to_string();
                }
            }
        }
        if matches!(self.format, Format::Observation) && verdict == Verdict::Passed {
            let interpreted: Result<(Value, Verdict)> = async {
                let bytes = tokio::fs::read(report.run_dir.join("stdout.log")).await?;
                ensure!(
                    bytes.len() as u64 <= check.command.max_output_bytes
                        && format!("{:x}", Sha256::digest(&bytes)) == report.stdout_sha256,
                    "observation output changed or exceeds bound"
                );
                let value: telos_core::observation::Assertion = serde_json::from_slice(&bytes)
                    .context("observation adapter requires one protocol assertion")?;
                let policy = check
                    .observation
                    .as_ref()
                    .context("observation policy missing")?;
                let (verdict, unique) = value.evaluate(
                    binding.context("observation binding missing")?,
                    policy,
                    telos_store::now(),
                )?;
                Ok((
                    json!({"observation":value,"unique_signals":unique,"verdict":verdict}),
                    verdict,
                ))
            }
            .await;
            match interpreted {
                Ok((value, observed)) => {
                    verdict = observed;
                    reason = "project observation independently measured; coverage and freshness checked".into();
                    assertion = value;
                }
                Err(error) => {
                    verdict = Verdict::Error;
                    reason = error.to_string();
                }
            }
        }
        let streams = vec![
            StreamEvidence {
                name: "stdout",
                path: report.run_dir.join("stdout.log"),
                sha256: report.stdout_sha256.clone(),
            },
            StreamEvidence {
                name: "stderr",
                path: report.run_dir.join("stderr.log"),
                sha256: report.stderr_sha256.clone(),
            },
        ];
        Ok(TestObservation {
            verdict,
            cleanup_confirmed: report.cleanup_confirmed,
            detail: json!({"adapter":check.adapter,"reason":reason,"report":report,"assertion":assertion}),
            streams,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn old_test_plan_roundtrips_without_new_fields_and_explicit_adapter_is_preserved() -> Result<()>
    {
        let old = json!({"criterion_id":"test","command":{"program":"/bin/true","args":[],"readable_roots":[],"timeout_seconds":1,"max_output_bytes":1024}});
        let mut parsed: TestCheck = serde_json::from_value(old.clone())?;
        assert_eq!(parsed.adapter, "local.sandbox_command");
        assert_eq!(serde_json::to_value(&parsed)?, old);
        parsed.adapter = "local.sandbox_json".into();
        assert_eq!(
            serde_json::to_value(&parsed)?["adapter"],
            "local.sandbox_json"
        );
        Ok(())
    }
}
