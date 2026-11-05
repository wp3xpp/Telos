use anyhow::Result;
use std::{os::unix::fs::PermissionsExt, path::PathBuf, time::Duration};
use telos_core::{ExecutionOutcome, ExecutionSpec, Sandbox};
use telos_runtime::{process::snapshot, runner::CodexRunner};
use tokio::sync::watch;
use uuid::Uuid;

struct Fixture {
    root: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("telos-runner-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let program = root.join("fake-codex");
        std::fs::write(&program,r#"#!/usr/bin/env python3
import json,pathlib,subprocess,sys,time
args=sys.argv
result=pathlib.Path(args[args.index('-o')+1])
prompt=sys.stdin.read()
print(json.dumps({'type':'thread.started','thread_id':'fixture-thread'}),flush=True)
if 'detached-child' in prompt:
    child=subprocess.Popen([sys.executable,'-c','import time; time.sleep(90)'],start_new_session=True)
    (result.parent/'child.pid').write_text(str(child.pid))
    time.sleep(90)
elif 'flood-output' in prompt:
    print('x'*100000,flush=True)
    time.sleep(90)
elif 'long-diagnostics' in prompt:
    print(json.dumps({'type':'item.completed','item':{'output':'é'*50000}},ensure_ascii=False),flush=True)
    result.write_text(json.dumps({'status':'done','summary':'fixture completed','evidence_paths':[],'remaining_work':[]}))
elif 'invalid-result' in prompt:
    result.write_text('{"wrong":"shape"}')
else:
    result.write_text(json.dumps({'status':'done','summary':'fixture completed','evidence_paths':[],'remaining_work':[]}))
print(json.dumps({'type':'turn.completed','usage':{'output_tokens':10}}),flush=True)
"#).unwrap();
        std::fs::set_permissions(program, std::fs::Permissions::from_mode(0o700)).unwrap();
        Self { root }
    }
    fn runner(&self) -> CodexRunner {
        CodexRunner {
            program: self.root.join("fake-codex"),
        }
    }
    fn spec(&self, task: &str) -> ExecutionSpec {
        ExecutionSpec {
            task: task.into(),
            cwd: self.root.to_str().unwrap().into(),
            sandbox: Sandbox::ReadOnly,
            timeout_seconds: 10,
            max_output_bytes: 65536,
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[tokio::test]
async fn captures_events_result_and_usage_without_verifying_goal() -> Result<()> {
    let f = Fixture::new();
    let (_tx, rx) = watch::channel(false);
    let report = f
        .runner()
        .run(&f.spec("normal"), &f.root.join("run"), rx)
        .await?;
    assert_eq!(report.outcome, ExecutionOutcome::Succeeded);
    assert_eq!(report.thread_id.as_deref(), Some("fixture-thread"));
    assert!(report.cleanup_confirmed);
    assert!(report.reason.contains("independent verification"));
    assert_eq!(report.usage.unwrap()["output_tokens"], 10);
    Ok(())
}

#[tokio::test]
async fn rejects_valid_exit_with_invalid_result() -> Result<()> {
    let f = Fixture::new();
    let (_tx, rx) = watch::channel(false);
    let report = f
        .runner()
        .run(&f.spec("invalid-result"), &f.root.join("run"), rx)
        .await?;
    assert_eq!(report.outcome, ExecutionOutcome::OutcomeUnknown);
    assert!(report.reason.contains("invalid structured result"));
    Ok(())
}

#[tokio::test]
async fn timeout_cleans_child_in_separate_session() -> Result<()> {
    let f = Fixture::new();
    let (_tx, rx) = watch::channel(false);
    let mut spec = f.spec("detached-child");
    spec.timeout_seconds = 3;
    let report = f.runner().run(&spec, &f.root.join("run"), rx).await?;
    let child: i32 = std::fs::read_to_string(f.root.join("run/child.pid"))?.parse()?;
    assert_eq!(report.outcome, ExecutionOutcome::OutcomeUnknown);
    assert!(report.reason.contains("timeout"));
    assert!(report.cleanup_confirmed);
    assert!(snapshot()
        .await?
        .get(&child)
        .is_none_or(|p| p.state.starts_with('Z')));
    Ok(())
}

#[tokio::test]
async fn explicit_cancel_cleans_child_and_output_limit_stops_job() -> Result<()> {
    let f = Fixture::new();
    let (tx, rx) = watch::channel(false);
    let child_file = f.root.join("cancel/child.pid");
    let cancel = tokio::spawn(async move {
        for _ in 0..100 {
            if child_file.exists() {
                tokio::time::sleep(Duration::from_millis(400)).await;
                tx.send(true).unwrap();
                return;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        panic!("child never started");
    });
    let report = f
        .runner()
        .run(&f.spec("detached-child"), &f.root.join("cancel"), rx)
        .await?;
    cancel.await?;
    assert!(report.reason.contains("cancelled"));
    assert!(report.cleanup_confirmed);
    let (_tx, rx) = watch::channel(false);
    let mut spec = f.spec("flood-output");
    spec.max_output_bytes = 1024;
    spec.sandbox = Sandbox::WorkspaceWrite;
    let report = f.runner().run(&spec, &f.root.join("flood"), rx).await?;
    assert!(report.reason.contains("output limit"));
    assert!(std::fs::metadata(f.root.join("flood/events.jsonl"))?.len() <= 1024);
    Ok(())
}

#[tokio::test]
async fn read_only_completion_survives_a_cut_diagnostic_frame() -> Result<()> {
    let f = Fixture::new();
    let (_tx, rx) = watch::channel(false);
    let mut spec = f.spec("long-diagnostics");
    spec.max_output_bytes = 1024;
    let report = f.runner().run(&spec, &f.root.join("run"), rx).await?;
    assert_eq!(report.outcome, ExecutionOutcome::Succeeded);
    assert!(report.output_truncated && report.cleanup_confirmed);
    assert_eq!(report.usage.unwrap()["output_tokens"], 10);
    assert!(std::fs::metadata(f.root.join("run/events.jsonl"))?.len() <= 1024);
    Ok(())
}
