//! Generic project-test orchestration. Provider paths/protocols belong to adapters.
use crate::{
    process::{snapshot, ProcessSet},
    workspace,
};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{future::Future, path::Path, process::Stdio, time::Duration};
use telos_core::DeliveryWorkspace;
use telos_store::Store;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    sync::watch,
};

#[derive(Clone, Copy, Serialize, Deserialize)]
pub struct TestLimits {
    pub heartbeat_seconds: u64,
    pub timeout_seconds: u64,
    pub max_output_bytes: usize,
}
impl TestLimits {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1..=60).contains(&self.heartbeat_seconds)
                && (1..=3600).contains(&self.timeout_seconds)
                && (1024..=1024 * 1024).contains(&self.max_output_bytes),
            "invalid project test bounds"
        );
        Ok(())
    }
}
pub struct AdapterIdentity {
    pub adapter: String,
    pub session_id: String,
    pub environment: String,
}
pub struct AdapterExecution {
    pub exit_code: i32,
    /// Adapter-filtered streams only; raw capabilities must not cross this boundary.
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub heartbeat_count: u64,
    pub local_cleanup_confirmed: bool,
}
#[derive(Clone, Copy)]
pub struct AdapterRun<'a> {
    pub store: &'a Store,
    pub loop_id: &'a str,
    pub revision: i64,
    pub directory: &'a Path,
    pub limits: TestLimits,
}
/// Trusted runtime implementations, not model-supplied plugins or permission grants.
/// The adapter owns its environment protocol, source/driver pinning, credential
/// handling and resource cleanup. The framework owns evidence and verification
/// ownership. Implementations must support bounded cancellation and never replay
/// an uncertain external operation. Success here is not a criterion verdict.
pub trait ProjectTestAdapter: Send {
    fn identity(&self) -> Result<AdapterIdentity>;
    fn metadata(&self) -> Result<Value>;
    fn prepare(&mut self, workspace: &Path) -> Result<()>;
    fn execute<'a>(
        &'a mut self,
        context: AdapterRun<'a>,
        cancel: watch::Receiver<bool>,
    ) -> impl Future<Output = Result<AdapterExecution>> + Send + 'a;
    fn release<'a>(
        &'a mut self,
        context: AdapterRun<'a>,
    ) -> impl Future<Output = Result<()>> + Send + 'a;
    fn fence<'a>(&'a mut self, context: AdapterRun<'a>) -> impl Future<Output = ()> + Send + 'a;
}

async fn supervise<A: ProjectTestAdapter>(
    adapter: &mut A,
    context: AdapterRun<'_>,
    cycle: &str,
    lease: &str,
    mut cancel: watch::Receiver<bool>,
) -> Result<AdapterExecution> {
    let (stop, stopped) = watch::channel(false);
    let work = adapter.execute(context, stopped);
    tokio::pin!(work);
    let deadline = tokio::time::sleep(Duration::from_secs(context.limits.timeout_seconds));
    tokio::pin!(deadline);
    let mut tick = tokio::time::interval(Duration::from_millis(200));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let check =
        context
            .store
            .check_verification_owner(context.loop_id, context.revision, cycle, lease);
    tokio::pin!(check);
    let mut checking = true;
    let reason = loop {
        tokio::select! {
            biased;
            changed=cancel.changed()=>{if changed.is_err() || *cancel.borrow() {break "project test cancelled";}},
            _=&mut deadline=>break "project test deadline reached",
            result=&mut check, if checking=>{
                if result.is_err() {break "project test verification owner changed";}
                checking=false;
            },
            _=tick.tick(), if !checking=>{
                check.set(context.store.check_verification_owner(context.loop_id,context.revision,cycle,lease));
                checking=true;
            },
            result=&mut work=>return result,
        }
    };
    let _ = stop.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(10), &mut work).await;
    anyhow::bail!("{reason}; adapter outcome requires reconciliation")
}

#[derive(Serialize)]
pub struct TestExecutionObservation {
    pub adapter: String,
    pub session_id: String,
    pub environment: String,
    pub input_snapshot_sha256: String,
    pub intent_evidence_id: String,
    pub stdout_evidence_id: Option<String>,
    pub stderr_evidence_id: Option<String>,
    pub stdout_bytes: usize,
    pub stderr_bytes: usize,
    pub stdout_sha256: String,
    pub stderr_sha256: String,
    pub exit_code: i32,
    pub heartbeat_count: u64,
    pub source_snapshot_unchanged: bool,
    pub local_cleanup_confirmed: bool,
    pub owned_operations_idle_before_release: bool,
    pub adapter_cleanup_confirmed: bool,
    pub scope: &'static str,
}

/// In-memory output for trusted adapters to filter before recording evidence.
/// Deliberately not serializable: streams may contain adapter-managed credentials.
pub struct RawOutput {
    pub exit_code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}
async fn bounded<R: AsyncRead + Unpin>(reader: R, limit: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .await?;
    ensure!(bytes.len() <= limit, "project test output exceeded bound");
    Ok(bytes)
}
/// Run an adapter command with bounded output, cancellation and owned-process cleanup.
/// The caller supplies isolation, authority and the enclosing deadline.
pub async fn execute(
    mut command: Command,
    directory: &Path,
    limit: usize,
    mut cancel: watch::Receiver<bool>,
) -> Result<RawOutput> {
    ensure!(!*cancel.borrow(), "project test cancelled before spawn");
    snapshot().await?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    let mut child = command.spawn().context("project test launch unavailable")?;
    let mut owned = ProcessSet::new(child.id().context("project test PID missing")? as i32);
    // Capture only in memory. On cancellation/overflow the streams are dropped,
    // local identities are drained, and provider ownership remains uncertain.
    let output = bounded(child.stdout.take().context("test stdout missing")?, limit);
    let errors = bounded(child.stderr.take().context("test stderr missing")?, limit);
    let capture = async { tokio::try_join!(output, errors) };
    tokio::pin!(capture);
    let mut streams = None;
    let mut status = None;
    let mut tick = tokio::time::interval(Duration::from_millis(200));
    let result: Result<RawOutput> = async {
        loop {
            tokio::select! {
                biased;
                changed = cancel.changed() => {
                    if changed.is_err() || *cancel.borrow() { anyhow::bail!("project test cancelled; adapter outcome unknown"); }
                }
                result = &mut capture, if streams.is_none() => streams=Some(result?),
                _ = tick.tick() => {
                    let table = snapshot().await?;
                    owned.observe(&table);
                    owned.persist(&directory.join("processes.json")).await?;
                    if status.is_none() { status=child.try_wait()?; }
                    if status.is_some() && streams.is_some() {
                        ensure!(!owned.identities.is_empty(), "test process identity was not observed; cleanup uncertain");
                        ensure!(owned.alive_in(&snapshot().await?).is_empty(), "test descendants remain; adapter outcome unknown");
                        let (stdout,stderr)=streams.take().unwrap();
                        ensure!(stdout.len()+stderr.len()<=limit, "combined project test output exceeded bound");
                        return Ok(RawOutput {exit_code:status.unwrap().code().context("project test terminated by signal")?,stdout,stderr});
                    }
                }
            }
        }
    }.await;
    if result.is_err() {
        owned.terminate().await?;
        let _ = child.wait().await;
    }
    result
}

/// Run one adapter under the caller's existing verification owner. No provider
/// kind is assumed here; acceptance remains the independent verifier's job.
#[allow(clippy::too_many_arguments)]
pub async fn run<A: ProjectTestAdapter>(
    store: &Store,
    loop_id: &str,
    revision: i64,
    verification_lease: &str,
    assigned: &DeliveryWorkspace,
    adapter: &mut A,
    limits: TestLimits,
    cancel: watch::Receiver<bool>,
) -> Result<TestExecutionObservation> {
    limits.validate()?;
    ensure!(
        !*cancel.borrow(),
        "project test cancelled before preparation"
    );
    let current = store
        .workspace_for_loop(loop_id, revision)
        .await?
        .context("delivery workspace missing")?;
    ensure!(
        serde_json::to_value(&current)? == serde_json::to_value(assigned)?
            && assigned.state == "ready"
            && assigned.contract_revision == revision,
        "project test needs current owned delivery workspace"
    );
    ensure!(
        verification_lease.starts_with("verification-lease-")
            && verification_lease
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-'),
        "invalid verification lease ID"
    );
    workspace::validate_identity(assigned).await?;
    store
        .check_verification_owner(loop_id, revision, &assigned.cycle_id, verification_lease)
        .await?;
    let goal = store.get_loop(loop_id).await?;
    let project = store.inspect_project(&goal.project_id).await?;
    ensure!(
        project["execution_policy"]["revision"].as_i64() == Some(assigned.policy_revision),
        "test workspace policy is stale"
    );
    adapter.prepare(Path::new(&assigned.path))?;
    let identity = adapter.identity()?;
    for label in [
        &identity.adapter,
        &identity.session_id,
        &identity.environment,
    ] {
        ensure!(
            !label.is_empty() && label.len() <= 512 && !label.chars().any(char::is_control),
            "invalid adapter identity"
        );
    }
    // A session identity is metadata, never a filesystem component supplied by a plugin.
    let key = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&(
            &identity.adapter,
            &identity.session_id,
            &identity.environment
        ))?)
    );
    let directory = store
        .verification_run_parent()
        .join(format!("{verification_lease}-project-{}", &key[..24]));
    tokio::fs::create_dir_all(store.verification_run_parent()).await?;
    ensure!(
        !directory.exists(),
        "project test process directory already exists"
    );
    tokio::fs::create_dir(&directory).await?;
    let input = workspace::snapshot(assigned, &[]).await?;
    let input_hash = format!("{:x}", Sha256::digest(serde_json::to_vec(&input)?));
    let artifact = format!("project-test-input:{input_hash}");
    let intent=store.add_evidence(&assigned.cycle_id,revision,&artifact,&identity.environment,
        &serde_json::to_vec(&json!({"kind":"project_test_intent","verification_lease_id":verification_lease,
            "workspace_id":assigned.id,"input_snapshot_sha256":input_hash,"adapter":identity.adapter,
            "session_id":identity.session_id,"metadata":adapter.metadata()?,"limits":limits,
            "scope":"Intent only; no execution result or delivery acceptance"}))?).await?;
    let context = AdapterRun {
        store,
        loop_id,
        revision,
        directory: &directory,
        limits,
    };
    let attempted:Result<TestExecutionObservation>=async {
        let result=supervise(adapter,context,&assigned.cycle_id,verification_lease,cancel).await?;
        store.check_verification_owner(loop_id,revision,&assigned.cycle_id,verification_lease).await?;
        ensure!(result.local_cleanup_confirmed,"project test local cleanup uncertain");
        ensure!(result.stdout.len()+result.stderr.len()<=limits.max_output_bytes,"adapter output exceeds bound");
        ensure!(workspace::snapshot(assigned,&[]).await?==input,"project test source changed; no acceptance");
        let stdout_id=if result.stdout.is_empty(){None}else{Some(store.add_evidence(&assigned.cycle_id,revision,&artifact,&identity.environment,&result.stdout).await?)};
        let stderr_id=if result.stderr.is_empty(){None}else{Some(store.add_evidence(&assigned.cycle_id,revision,&artifact,&identity.environment,&result.stderr).await?)};
        adapter.release(context).await?;
        Ok(TestExecutionObservation {adapter:identity.adapter,session_id:identity.session_id,environment:identity.environment,
            input_snapshot_sha256:input_hash,intent_evidence_id:intent,stdout_evidence_id:stdout_id,stderr_evidence_id:stderr_id,
            stdout_bytes:result.stdout.len(),stderr_bytes:result.stderr.len(),
            stdout_sha256:format!("{:x}",Sha256::digest(&result.stdout)),stderr_sha256:format!("{:x}",Sha256::digest(&result.stderr)),
            exit_code:result.exit_code,heartbeat_count:result.heartbeat_count,source_snapshot_unchanged:true,
            local_cleanup_confirmed:true,owned_operations_idle_before_release:true,adapter_cleanup_confirmed:true,
            scope:"Execution and adapter cleanup observations only, within the declared adapter scope. Input snapshots are not remote artifact attestations or criterion acceptance; streams are adapter-filtered and hashes describe the stored bytes."})
    }.await;
    if attempted.is_err() {
        adapter.fence(context).await;
        store
            .mark_verification_unknown(
                verification_lease,
                "project test adapter operation requires reconciliation",
            )
            .await?;
    }
    attempted
}

#[cfg(test)]
mod tests {
    use super::*;
    use telos_core::*;
    struct Fixture(std::path::PathBuf);
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    struct LocalAdapter {
        command: Option<Command>,
        cleaned: bool,
        delay_seconds: f64,
    }
    impl ProjectTestAdapter for LocalAdapter {
        fn identity(&self) -> Result<AdapterIdentity> {
            Ok(AdapterIdentity {
                adapter: "fixture.local_process".into(),
                session_id: "local-once".into(),
                environment: "fixture-local".into(),
            })
        }
        fn metadata(&self) -> Result<Value> {
            Ok(json!({"kind":"local-process","external_environment_required":false}))
        }
        fn prepare(&mut self, workspace: &Path) -> Result<()> {
            ensure!(self.command.is_none(), "already prepared");
            let mut command = Command::new("/bin/sh");
            command
                .args([
                    "-c",
                    &format!(
                        "sleep {}; printf 'generic adapter result\\n'",
                        self.delay_seconds
                    ),
                ])
                .current_dir(workspace)
                .env_clear();
            self.command = Some(command);
            Ok(())
        }
        async fn execute<'a>(
            &'a mut self,
            context: AdapterRun<'a>,
            cancel: watch::Receiver<bool>,
        ) -> Result<AdapterExecution> {
            let raw = super::execute(
                self.command.take().context("not prepared")?,
                context.directory,
                context.limits.max_output_bytes,
                cancel,
            )
            .await?;
            Ok(AdapterExecution {
                exit_code: raw.exit_code,
                stdout: raw.stdout,
                stderr: raw.stderr,
                heartbeat_count: 0,
                local_cleanup_confirmed: true,
            })
        }
        async fn release<'a>(&'a mut self, _context: AdapterRun<'a>) -> Result<()> {
            self.cleaned = true;
            Ok(())
        }
        async fn fence<'a>(&'a mut self, _context: AdapterRun<'a>) {}
    }
    #[tokio::test]
    async fn same_coordinator_runs_an_adapter_without_project_scripts() -> Result<()> {
        let f = Fixture(
            std::env::temp_dir().join(format!("telos-generic-test-{}", uuid::Uuid::new_v4())),
        );
        let project = f.0.join("project");
        std::fs::create_dir_all(&project)?;
        std::fs::write(project.join("input.txt"), b"generic input")?;
        for args in [
            vec!["init", "--quiet"],
            vec!["add", "."],
            vec![
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "commit.gpgSign=false",
                "-c",
                "core.hooksPath=/dev/null",
                "commit",
                "-qm",
                "fixture",
            ],
        ] {
            ensure!(
                std::process::Command::new("git")
                    .arg("-C")
                    .arg(&project)
                    .args(args)
                    .output()?
                    .status
                    .success(),
                "fixture Git failed"
            );
        }
        let store = Store::open(&f.0.join("state")).await?;
        store.register_project("generic", &project).await?;
        store
            .configure_project(
                "generic",
                0,
                &ProjectExecutionPolicy {
                    enabled: true,
                    sandbox: Sandbox::WorkspaceWrite,
                    timeout_seconds: 10,
                    max_output_bytes: 65536,
                    max_steps_per_cycle: 1,
                },
            )
            .await?;
        let goal = store
            .create_loop(
                "generic",
                "generic adapter",
                LoopKind::Delivery,
                &Contract {
                    goal: "Exercise provider-neutral execution".into(),
                    delivery_scope: "Fixture only".into(),
                    criteria: vec![Criterion {
                        id: "check".into(),
                        description: "No criterion acceptance in this component test".into(),
                    }],
                    interval_seconds: None,
                    max_attempts: 1,
                },
            )
            .await?;
        let assigned = workspace::prepare(&store, &goal.id, 1, 1, "HEAD").await?;
        store
            .configure_verification(
                &goal.id,
                1,
                0,
                &VerificationPlan {
                    checks: vec![FileHashCheck {
                        criterion_id: "check".into(),
                        relative_path: "input.txt".into(),
                        expected_sha256: format!("{:x}", Sha256::digest(b"generic input")),
                    }],
                    log_checks: vec![],
                    test_checks: vec![],
                },
            )
            .await?;
        let lease = store.acquire_verification(&goal.id, 1, 1).await?;
        let mut adapter = LocalAdapter {
            command: None,
            cleaned: false,
            delay_seconds: 0.4,
        };
        let limits = TestLimits {
            heartbeat_seconds: 1,
            timeout_seconds: 5,
            max_output_bytes: 65536,
        };
        let (_stop, cancel) = watch::channel(false);
        assert!(run(
            &store,
            &goal.id,
            1,
            "verification-lease-unowned",
            &assigned,
            &mut adapter,
            limits,
            cancel.clone()
        )
        .await
        .is_err());
        assert!(
            adapter.command.is_none(),
            "foreign lease must fail before adapter preparation"
        );
        let observed = run(
            &store,
            &goal.id,
            1,
            &lease,
            &assigned,
            &mut adapter,
            limits,
            cancel,
        )
        .await?;
        assert_eq!(observed.adapter, "fixture.local_process");
        assert_eq!(observed.exit_code, 0);
        assert!(adapter.cleaned && observed.adapter_cleanup_confirmed);
        assert!(!project.join("scripts").exists());
        assert!(store
            .environment_adapter("generic", "fixture")
            .await?
            .is_none());
        let output = store
            .get_evidence(observed.stdout_evidence_id.as_deref().unwrap(), 0, 32768)
            .await?;
        assert_eq!(output["text"], "generic adapter result\n");
        assert!(store.task_context(&goal.id, 1).await?["latest_verification"].is_null());
        store.finish_verification(&lease).await?;
        // A provider without its own lease heartbeat must still obey the same
        // framework deadline and verification ownership rules.
        let lease = store.acquire_verification(&goal.id, 1, 1).await?;
        let mut slow = LocalAdapter {
            command: None,
            cleaned: false,
            delay_seconds: 30.0,
        };
        let (_stop, cancel) = watch::channel(false);
        let began = std::time::Instant::now();
        let result = run(
            &store,
            &goal.id,
            1,
            &lease,
            &assigned,
            &mut slow,
            TestLimits {
                timeout_seconds: 1,
                ..limits
            },
            cancel,
        )
        .await;
        assert!(result.is_err() && !slow.cleaned);
        assert!(began.elapsed() < Duration::from_secs(10));
        assert_eq!(
            store.verification_status(&goal.id).await?["active_verification"]["state"],
            "outcome_unknown"
        );
        let table = snapshot().await?;
        for entry in std::fs::read_dir(f.0.join("state/runs"))? {
            let path = entry?.path().join("processes.json");
            if path.exists() {
                let owned: ProcessSet = serde_json::from_slice(&std::fs::read(path)?)?;
                assert!(owned.alive_in(&table).is_empty());
            }
        }
        store.close().await;
        Ok(())
    }
}
