use crate::process::{snapshot, ProcessSet};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use telos_core::{ExecutionOutcome, ExecutionSpec, Sandbox};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
    sync::watch,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentResult {
    pub status: AgentStatus,
    pub summary: String,
    pub evidence_paths: Vec<String>,
    pub remaining_work: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Done,
    Blocked,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunReport {
    pub outcome: ExecutionOutcome,
    pub reason: String,
    pub exit_code: Option<i32>,
    pub result: Option<AgentResult>,
    pub thread_id: Option<String>,
    pub usage: Option<Value>,
    pub run_dir: PathBuf,
    pub cleanup_confirmed: bool,
    #[serde(default)]
    pub output_truncated: bool,
}

pub fn result_schema() -> Value {
    json!({"type":"object","properties":{
        "status":{"type":"string","enum":["done","blocked"]},
        "summary":{"type":"string"},
        "evidence_paths":{"type":"array","items":{"type":"string"}},
        "remaining_work":{"type":"array","items":{"type":"string"}}
    },"required":["status","summary","evidence_paths","remaining_work"],"additionalProperties":false})
}

#[derive(Clone)]
pub struct CodexRunner {
    pub program: PathBuf,
}

pub(crate) async fn capture<R: AsyncRead + Unpin>(
    input: R,
    path: PathBuf,
    total: Arc<AtomicU64>,
    overflow: Arc<AtomicBool>,
    limit: u64,
) -> Result<()> {
    capture_with(input, path, total, overflow, limit, |_| {}).await
}

async fn capture_with<R: AsyncRead + Unpin>(
    mut input: R,
    path: PathBuf,
    total: Arc<AtomicU64>,
    overflow: Arc<AtomicBool>,
    limit: u64,
    mut observe: impl FnMut(&[u8]),
) -> Result<()> {
    let mut file = tokio::fs::File::create(path).await?;
    let mut buf = [0u8; 8192];
    loop {
        let n = input.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        observe(&buf[..n]);
        let old = total.fetch_add(n as u64, Ordering::SeqCst);
        let keep = limit.saturating_sub(old).min(n as u64) as usize;
        if keep > 0 {
            file.write_all(&buf[..keep]).await?;
        }
        if keep < n {
            overflow.store(true, Ordering::SeqCst);
        }
    }
    file.sync_all().await?;
    Ok(())
}

// Parse the incoming stream independently of the retained diagnostic bytes.
#[derive(Default)]
struct EventStream {
    line: Vec<u8>,
    oversized: bool,
    invalid: bool,
    thread_id: Option<String>,
    usage: Option<Value>,
    completed: bool,
    failed: bool,
}

impl EventStream {
    fn observe(&mut self, bytes: &[u8]) {
        const MAX_FRAME: usize = 1024 * 1024;
        for part in bytes.split_inclusive(|byte| *byte == b'\n') {
            if self.line.len() + part.len() > MAX_FRAME {
                self.oversized = true;
            }
            if !self.oversized {
                self.line.extend_from_slice(part);
            }
            if part.ends_with(b"\n") {
                self.finish_line();
            }
        }
    }

    fn finish_line(&mut self) {
        if self.oversized {
            self.invalid = true;
        } else if !self.line.is_empty() {
            match serde_json::from_slice::<Value>(&self.line) {
                Ok(event) => match event["type"].as_str() {
                    Some("thread.started") => {
                        self.thread_id = event["thread_id"].as_str().map(str::to_owned);
                    }
                    Some("turn.completed") => {
                        self.completed = true;
                        self.usage = event.get("usage").cloned();
                    }
                    Some("turn.failed") => self.failed = true,
                    _ => {}
                },
                Err(_) => self.invalid = true,
            }
        }
        self.line.clear();
        self.oversized = false;
    }
}

impl CodexRunner {
    pub async fn run(
        &self,
        spec: &ExecutionSpec,
        run_dir: &Path,
        mut cancel: watch::Receiver<bool>,
    ) -> Result<RunReport> {
        spec.validate()?;
        ensure!(!*cancel.borrow(), "execution was cancelled before launch");
        ensure!(!run_dir.exists(), "run directory must be new");
        snapshot().await?; // Fail before spawning when process inspection is unavailable.
        tokio::fs::create_dir_all(run_dir).await?;
        let run_dir = tokio::fs::canonicalize(run_dir).await?;
        let schema = run_dir.join("result.schema.json");
        let result_path = run_dir.join("result.json");
        tokio::fs::write(&schema, serde_json::to_vec(&result_schema())?).await?;
        tokio::fs::write(
            run_dir.join("request.json"),
            serde_json::to_vec_pretty(spec)?,
        )
        .await?;
        let mut command = Command::new(&self.program);
        command
            .args([
                "exec",
                "--ignore-user-config",
                "--ephemeral",
                "--skip-git-repo-check",
                "--json",
                "--color",
                "never",
            ])
            .arg("--sandbox")
            .arg(match spec.sandbox {
                Sandbox::ReadOnly => "read-only",
                Sandbox::WorkspaceWrite => "workspace-write",
            })
            .arg("-C")
            .arg(&spec.cwd)
            .arg("--output-schema")
            .arg(&schema)
            .arg("-o")
            .arg(&result_path)
            .arg("-")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // Do not propagate arbitrary service credentials into repository processes.
        command.env_clear();
        for key in [
            "PATH",
            "HOME",
            "CODEX_HOME",
            "TMPDIR",
            "LANG",
            "LC_ALL",
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "NO_PROXY",
            "SSL_CERT_FILE",
        ] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        #[cfg(unix)]
        {
            command.process_group(0);
        }
        let mut child = command.spawn().context("starting codex")?;
        let pid = child.id().context("missing Codex pid")? as i32;
        let mut owned = ProcessSet::new(pid);
        let processes_path = run_dir.join("processes.json");
        let total = Arc::new(AtomicU64::new(0));
        let overflow = Arc::new(AtomicBool::new(false));
        let event_input = child.stdout.take().context("missing stdout")?;
        let event_path = run_dir.join("events.jsonl");
        let event_total = total.clone();
        let event_overflow = overflow.clone();
        let limit = spec.max_output_bytes;
        let mut stdout = tokio::spawn(async move {
            let mut stream = EventStream::default();
            capture_with(
                event_input,
                event_path,
                event_total,
                event_overflow,
                limit,
                |bytes| stream.observe(bytes),
            )
            .await?;
            stream.finish_line();
            Ok::<_, anyhow::Error>(stream)
        });
        let stderr = tokio::spawn(capture(
            child.stderr.take().context("missing stderr")?,
            run_dir.join("stderr.log"),
            total,
            overflow.clone(),
            spec.max_output_bytes,
        ));
        let prompt=format!("Telos bounded execution. Read project instructions. Perform only this task, return the requested result schema, and do not claim overall goal verification. If required permissions or information are missing, report blocked.\n\nTask:\n{}",spec.task);
        let mut stdin = child.stdin.take().context("missing stdin")?;
        let input = tokio::spawn(async move {
            stdin.write_all(prompt.as_bytes()).await?;
            stdin.shutdown().await
        });
        let deadline = tokio::time::Instant::now() + Duration::from_secs(spec.timeout_seconds);
        let mut tick = tokio::time::interval(Duration::from_millis(200));
        let mut reason = String::new();
        let mut exit_code = None;
        loop {
            tokio::select! {
                _=tick.tick()=> {
                    match snapshot().await {
                        Ok(table)=> {owned.observe(&table); if let Err(e)=owned.persist(&processes_path).await {reason=format!("process journal error: {e}"); break;}},
                        Err(e)=> {reason=format!("process inspection failed: {e}");break;}
                    }
                    if spec.sandbox != Sandbox::ReadOnly && overflow.load(Ordering::SeqCst) {reason="output limit exceeded".into();break;}
                    if result_path.exists() {
                        match std::fs::metadata(&result_path) {
                            Ok(m) if m.len()>spec.max_output_bytes=>{reason="result file limit exceeded".into();break;},
                            Err(e)=>{reason=format!("result metadata error: {e}");break;},
                            _=>{}
                        }
                    }
                    if *cancel.borrow() {reason="cancelled".into();break;}
                    match child.try_wait() {
                        Ok(Some(status))=>{exit_code=status.code();break;},
                        Err(e)=>{reason=format!("process status error: {e}");break;},
                        Ok(None)=>{}
                    }
                },
                _=tokio::time::sleep_until(deadline)=>{reason="timeout".into();break;},
                changed=cancel.changed()=>{if changed.is_err() || *cancel.borrow() {reason="cancelled".into();break;}}
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
        if !input.is_finished() {
            input.abort();
        }
        let _ = input.await;
        // A descendant holding a pipe cannot keep this operation alive indefinitely.
        let events = match tokio::time::timeout(Duration::from_secs(3), &mut stdout).await {
            Ok(Ok(Ok(events))) => events,
            Ok(Ok(Err(error))) => {
                reason = format!("{reason}; output capture failed: {error}");
                EventStream::default()
            }
            Ok(Err(error)) => {
                reason = format!("{reason}; output capture failed: {error}");
                EventStream::default()
            }
            Err(_) => {
                stdout.abort();
                reason = format!("{reason}; output pipe did not close");
                EventStream::default()
            }
        };
        let mut stderr = stderr;
        match tokio::time::timeout(Duration::from_secs(3), &mut stderr).await {
            Ok(Ok(Ok(()))) => {}
            Ok(other) => reason = format!("{reason}; output capture failed: {other:?}"),
            Err(_) => {
                stderr.abort();
                reason = format!("{reason}; output pipe did not close");
            }
        }
        let output_truncated = overflow.load(Ordering::SeqCst);
        if spec.sandbox != Sandbox::ReadOnly && output_truncated && reason.is_empty() {
            reason = "output limit exceeded".into();
        }
        if events.invalid {
            reason = format!("{reason}; invalid JSON event stream");
        }
        if events.failed {
            reason = format!("{reason}; Codex turn failed");
        }
        if !events.completed && reason.is_empty() {
            reason = "Codex terminal completion event missing".into();
        }
        let mut result = None;
        if result_path.exists() && std::fs::metadata(&result_path)?.len() <= spec.max_output_bytes {
            match serde_json::from_slice::<AgentResult>(&tokio::fs::read(&result_path).await?) {
                Ok(value) if !value.summary.trim().is_empty() => result = Some(value),
                _ => reason = format!("{reason}; invalid structured result"),
            }
        } else {
            reason = format!("{reason}; result missing or too large");
        }
        let outcome =
            if reason.is_empty() && exit_code == Some(0) && events.completed && cleanup_confirmed {
                if result
                    .as_ref()
                    .is_some_and(|r| r.status == AgentStatus::Done)
                {
                    ExecutionOutcome::Succeeded
                } else {
                    ExecutionOutcome::Failed
                }
            } else {
                ExecutionOutcome::OutcomeUnknown
            };
        if reason.is_empty() {
            reason = if outcome == ExecutionOutcome::Succeeded {
                "execution completed; independent verification required"
            } else {
                "agent reported blocked"
            }
            .into();
        }
        let report = RunReport {
            outcome,
            reason,
            exit_code,
            result,
            thread_id: events.thread_id,
            usage: events.usage,
            run_dir: run_dir.clone(),
            cleanup_confirmed,
            output_truncated,
        };
        tokio::fs::write(
            run_dir.join("report.json"),
            serde_json::to_vec_pretty(&report)?,
        )
        .await?;
        Ok(report)
    }
}
