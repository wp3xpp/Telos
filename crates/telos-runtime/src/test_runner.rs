//! Deterministic local tests and native read-only project observation probes.
//! This layer records command observations; the ledger decides acceptance separately.
use crate::process::{snapshot, ProcessSet};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use telos_core::{TestCommand, Verdict};
use tokio::{process::Command, sync::watch};

#[derive(Debug, Serialize, Deserialize)]
pub struct TestReport {
    pub verdict: Verdict,
    pub exit_code: Option<i32>,
    pub reason: String,
    pub cleanup_confirmed: bool,
    pub program_sha256: String,
    pub profile_sha256: String,
    #[serde(
        default = "default_profile_file",
        skip_serializing_if = "is_default_profile_file"
    )]
    pub profile_file: String,
    pub stdout_sha256: String,
    pub stderr_sha256: String,
    pub run_dir: PathBuf,
}

fn default_profile_file() -> String {
    "codex-home/config.toml".into()
}
fn is_default_profile_file(value: &str) -> bool {
    value == "codex-home/config.toml"
}

fn readonly_profile(cwd: &Path, roots: &BTreeSet<String>, home: &Path) -> Result<String> {
    ensure!(
        cfg!(target_os = "macos"),
        "native read-only project observation currently requires macOS"
    );
    let mut allowed = roots.clone();
    allowed.insert(
        cwd.to_str()
            .context("observation root must be UTF-8")?
            .into(),
    );
    allowed.insert(
        home.to_str()
            .context("observation home must be UTF-8")?
            .into(),
    );
    for path in [
        "/System",
        "/usr",
        "/bin",
        "/sbin",
        "/dev",
        "/private/etc",
        "/Library/Apple",
        "/private/var/db/oah",
        "/private/var/db/dyld",
    ] {
        allowed.insert(path.into());
    }
    // Directory traversal/listing is needed by runtime launchers. Ancestors are
    // literal paths only: allowing their names does not allow child file data.
    let mut ancestors = BTreeSet::new();
    for path in &allowed {
        for parent in Path::new(path).ancestors() {
            ancestors.insert(
                parent
                    .to_str()
                    .context("profile ancestor must be UTF-8")?
                    .to_owned(),
            );
        }
    }
    let mut permits = String::new();
    for root in allowed {
        permits.push_str(&format!(" (subpath {})", serde_json::to_string(&root)?));
    }
    for parent in ancestors {
        permits.push_str(&format!(" (literal {})", serde_json::to_string(&parent)?));
    }
    Ok(format!("(version 1)\n(allow default)\n(deny file-write* (require-not (literal \"/dev/null\")))\n(deny network*)\n(deny file-read-data (require-not (require-any{permits})))\n"))
}

pub struct TestRunner {
    pub codex: PathBuf,
}
impl TestRunner {
    pub async fn run(
        &self,
        spec: &TestCommand,
        cwd: &Path,
        run_dir: &Path,
        cancel: watch::Receiver<bool>,
    ) -> Result<TestReport> {
        self.run_scoped(spec, cwd, run_dir, cancel, None).await
    }

    pub async fn run_observation(
        &self,
        spec: &TestCommand,
        cwd: &Path,
        run_dir: &Path,
        cancel: watch::Receiver<bool>,
        binding: &telos_core::observation::Binding,
    ) -> Result<TestReport> {
        self.run_scoped(spec, cwd, run_dir, cancel, Some(binding))
            .await
    }

    async fn run_scoped(
        &self,
        spec: &TestCommand,
        cwd: &Path,
        run_dir: &Path,
        mut cancel: watch::Receiver<bool>,
        binding: Option<&telos_core::observation::Binding>,
    ) -> Result<TestReport> {
        spec.validate()?;
        ensure!(!*cancel.borrow(), "test cancelled before launch");
        ensure!(!run_dir.exists(), "test run directory must be new");
        let cwd = cwd.canonicalize()?;
        ensure!(cwd.is_dir(), "test working directory missing");
        let parent = run_dir
            .parent()
            .context("test run directory requires a parent")?
            .canonicalize()?;
        ensure!(
            !parent.starts_with(&cwd),
            "test evidence directory must be outside the writable test workspace"
        );
        let program = Path::new(&spec.program).canonicalize()?;
        ensure!(program.is_file(), "test executable missing");
        let program_hash = format!("{:x}", Sha256::digest(tokio::fs::read(&program).await?));
        let mut roots = BTreeSet::from([program
            .to_str()
            .context("test executable must be UTF-8")?
            .to_owned()]);
        for root in &spec.readable_roots {
            roots.insert(
                Path::new(root)
                    .canonicalize()?
                    .to_str()
                    .context("runtime root must be UTF-8")?
                    .to_owned(),
            );
        }
        let (profile, profile_file) = if binding.is_some() {
            (
                readonly_profile(
                    &cwd,
                    &roots,
                    &parent
                        .join(run_dir.file_name().context("run name missing")?)
                        .join("home"),
                )?,
                "readonly.sb".to_owned(),
            )
        } else {
            let mut profile=String::from("[permissions.telos-tests]\nextends = \":workspace\"\n[permissions.telos-tests.filesystem]\n\":root\" = \"deny\"\n\":minimal\" = \"read\"\n");
            for root in roots {
                profile.push_str(&format!("{} = \"read\"\n", serde_json::to_string(&root)?));
            }
            profile.push_str("[permissions.telos-tests.network]\nenabled = false\n");
            (profile, default_profile_file())
        };
        snapshot().await?;
        tokio::fs::create_dir_all(run_dir.join("codex-home")).await?;
        tokio::fs::create_dir_all(run_dir.join("home")).await?;
        let run_dir = run_dir.canonicalize()?;
        tokio::fs::write(run_dir.join(&profile_file), &profile).await?;
        tokio::fs::write(
            run_dir.join("request.json"),
            serde_json::to_vec_pretty(spec)?,
        )
        .await?;
        let mut command = if binding.is_some() {
            let mut native = Command::new("/usr/bin/sandbox-exec");
            native.arg("-f").arg(run_dir.join(&profile_file));
            native
        } else {
            let mut sandbox = Command::new(&self.codex);
            sandbox
                .args([
                    "sandbox",
                    "--include-managed-config",
                    "-P",
                    "telos-tests",
                    "-C",
                ])
                .arg(&cwd)
                .arg("--");
            sandbox
        };
        command
            .arg(&program)
            .args(&spec.args)
            .current_dir(&cwd)
            .env_clear()
            .env("HOME", run_dir.join("home"))
            .env("CODEX_HOME", run_dir.join("codex-home"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .process_group(0);
        for key in ["PATH", "TMPDIR", "LANG", "LC_ALL"] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        if let Some(binding) = binding {
            command.env(
                telos_core::observation::CONTEXT_ENV,
                serde_json::to_string(binding)?,
            );
        }
        let mut child = command
            .spawn()
            .context("starting independent test sandbox")?;
        let mut owned = ProcessSet::new(child.id().context("test process missing")? as i32);
        let total = Arc::new(AtomicU64::new(0));
        let overflow = Arc::new(AtomicBool::new(false));
        let stdout = tokio::spawn(crate::runner::capture(
            child.stdout.take().context("test stdout missing")?,
            run_dir.join("stdout.log"),
            total.clone(),
            overflow.clone(),
            spec.max_output_bytes,
        ));
        let stderr = tokio::spawn(crate::runner::capture(
            child.stderr.take().context("test stderr missing")?,
            run_dir.join("stderr.log"),
            total,
            overflow.clone(),
            spec.max_output_bytes,
        ));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(spec.timeout_seconds);
        let mut tick = tokio::time::interval(Duration::from_millis(100));
        let mut reason = String::new();
        let mut exit_code = None;
        loop {
            tokio::select! {
                _=tick.tick()=>{
                    match snapshot().await {
                        Ok(table)=>{owned.observe(&table);if let Err(e)=owned.persist(&run_dir.join("processes.json")).await {reason=format!("process journal error: {e}");break;}},
                        Err(e)=>{reason=format!("process inspection failed: {e}");break;}
                    }
                    if overflow.load(Ordering::SeqCst){reason="test output limit exceeded".into();break;}
                    match child.try_wait(){Ok(Some(status))=>{exit_code=status.code();break;},Ok(None)=>{},Err(e)=>{reason=format!("test status error: {e}");break;}}
                },
                _=tokio::time::sleep_until(deadline)=>{reason="test timeout".into();break;},
                changed=cancel.changed()=>{if changed.is_err() || *cancel.borrow(){reason="test cancelled".into();break;}}
            }
        }
        let cleanup = owned.terminate().await;
        let cleanup_confirmed = cleanup.is_ok();
        if let Err(e) = cleanup {
            reason = format!("{reason}; cleanup uncertain: {e}");
        }
        if child.try_wait()?.is_none() {
            let _ = child.kill().await;
        }
        let status = child.wait().await?;
        if exit_code.is_none() {
            exit_code = status.code();
        }
        for mut handle in [stdout, stderr] {
            match tokio::time::timeout(Duration::from_secs(3), &mut handle).await {
                Ok(Ok(Ok(()))) => {}
                Ok(other) => {
                    reason = format!("{reason}; output capture failed: {other:?}");
                }
                Err(_) => {
                    handle.abort();
                    reason = format!("{reason}; output pipe did not close");
                }
            }
        }
        if overflow.load(Ordering::SeqCst) {
            reason = "test output limit exceeded".into();
        }
        let verdict = if !reason.is_empty() || !cleanup_confirmed || exit_code.is_none() {
            Verdict::Error
        } else if exit_code == Some(0) {
            Verdict::Passed
        } else {
            Verdict::Failed
        };
        if reason.is_empty() {
            reason = if verdict == Verdict::Passed {
                "configured test command exited zero"
            } else {
                "test command or sandbox wrapper exited nonzero"
            }
            .into();
        }
        let report = TestReport {
            verdict,
            exit_code,
            reason,
            cleanup_confirmed,
            program_sha256: program_hash,
            profile_sha256: format!("{:x}", Sha256::digest(profile.as_bytes())),
            profile_file,
            stdout_sha256: format!(
                "{:x}",
                Sha256::digest(tokio::fs::read(run_dir.join("stdout.log")).await?)
            ),
            stderr_sha256: format!(
                "{:x}",
                Sha256::digest(tokio::fs::read(run_dir.join("stderr.log")).await?)
            ),
            run_dir: run_dir.clone(),
        };
        tokio::fs::write(
            run_dir.join("report.json"),
            serde_json::to_vec_pretty(&report)?,
        )
        .await?;
        Ok(report)
    }
}
