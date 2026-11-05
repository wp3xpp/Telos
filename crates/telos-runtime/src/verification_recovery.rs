//! Conservative operator recovery: discard observations only with known cleanup.
use crate::{
    process::{snapshot, ProcessSet},
    test_runner::TestReport,
};
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{io::Read, os::unix::fs::OpenOptionsExt, path::Path};
use telos_core::VerificationPlan;
use telos_store::Store;

fn read(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let parent = path.parent().context("recovery evidence parent missing")?;
    ensure!(
        parent.canonicalize()? == parent,
        "recovery evidence parent changed or symlinked"
    );
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    ensure!(
        file.metadata()?.is_file(),
        "recovery evidence must be a regular file"
    );
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= limit,
        "recovery evidence exceeds limit"
    );
    Ok(bytes)
}

pub async fn reconcile(store: &Store, lease: &str) -> Result<Value> {
    // The lease ID comes from the ledger, and must never act as a caller-supplied path.
    let context = store.verification_recovery_context(lease).await?;
    ensure!(
        lease.starts_with("verification-lease-")
            && lease
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-'),
        "invalid lease identity"
    );
    let mut checks = Vec::new();
    if context["test_attempt"] == true {
        let plan: VerificationPlan = serde_json::from_value(context["plan"].clone())
            .context("original test plan unavailable")?;
        ensure!(
            !plan.test_checks.is_empty(),
            "original test commands unavailable"
        );
        let table = snapshot().await?;
        for (index, check) in plan.test_checks.iter().enumerate() {
            let dir = store
                .verification_run_parent()
                .join(format!("{lease}-{index}"));
            // Missing reports/journals are not proof that a process never started.
            let metadata = std::fs::symlink_metadata(&dir)
                .context("test run evidence missing; independent reconciliation required")?;
            ensure!(
                metadata.is_dir()
                    && !metadata.file_type().is_symlink()
                    && dir.canonicalize()? == dir,
                "recovery directory changed or symlinked"
            );
            let report_bytes = read(&dir.join("report.json"), 65536)
                .context("completed cleanup report missing")?;
            let report: TestReport = serde_json::from_slice(&report_bytes)?;
            ensure!(
                report.cleanup_confirmed && report.run_dir == dir,
                "test cleanup not confirmed or report belongs to another run"
            );
            let request = read(&dir.join("request.json"), 65536)?;
            ensure!(
                serde_json::from_slice::<Value>(&request)? == serde_json::to_value(&check.command)?,
                "test request does not match original plan"
            );
            ensure!(
                matches!(
                    report.profile_file.as_str(),
                    "codex-home/config.toml" | "readonly.sb"
                ),
                "unrecognized verifier profile file"
            );
            let profile = read(&dir.join(&report.profile_file), 65536)?;
            ensure!(
                format!("{:x}", Sha256::digest(&profile)) == report.profile_sha256,
                "test profile integrity failure"
            );
            for (name, hash) in [
                ("stdout", &report.stdout_sha256),
                ("stderr", &report.stderr_sha256),
            ] {
                let data = read(
                    &dir.join(format!("{name}.log")),
                    check.command.max_output_bytes,
                )?;
                ensure!(
                    format!("{:x}", Sha256::digest(&data)) == *hash,
                    "test output integrity failure"
                );
            }
            let journal = read(&dir.join("processes.json"), 1024 * 1024)?;
            let owned: ProcessSet = serde_json::from_slice(&journal)?;
            ensure!(owned.leader > 0, "invalid process journal");
            ensure!(
                owned.alive_in(&table).is_empty(),
                "recorded test process still alive"
            );
            checks.push(json!({"index":index,"report_sha256":format!("{:x}",Sha256::digest(report_bytes)),"process_journal_sha256":format!("{:x}",Sha256::digest(journal)),"cleanup_confirmed":true,"observed_live_processes":0}));
        }
    }
    let proof = json!({"adapter":"local_verification_cleanup","observed_at":telos_store::now(),"test_attempt":context["test_attempt"],"checks":checks,"scope":"recorded cleanup and observed process identities; does not assert test success or full process containment"});
    store.resolve_verification_unknown(lease, &proof).await
}
