use anyhow::{Context, Result};
use std::{os::unix::fs::PermissionsExt, path::PathBuf, time::Duration};
use telos_core::{TestCommand, Verdict};
use telos_runtime::{process::snapshot, test_runner::TestRunner};
use tokio::sync::watch;

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("telos-tests-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir(root.join("project")).unwrap();
        let wrapper = root.join("fake-codex");
        std::fs::write(
            &wrapper,
            r#"#!/usr/bin/env python3
import os,sys,pathlib
assert sys.argv[1:5]==['sandbox','--include-managed-config','-P','telos-tests']
assert not any(k in os.environ for k in ['OPENAI_API_KEY','HTTP_PROXY','HTTPS_PROXY'])
assert (pathlib.Path(os.environ['CODEX_HOME'])/'config.toml').is_file()
args=sys.argv[sys.argv.index('--')+1:]
os.execv(args[0],args)
"#,
        )
        .unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
        Self(root)
    }
    fn runner(&self) -> TestRunner {
        TestRunner {
            codex: self.0.join("fake-codex"),
        }
    }
    fn spec(&self, script: &str) -> TestCommand {
        TestCommand {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), script.into()],
            readable_roots: vec![],
            timeout_seconds: 3,
            max_output_bytes: 4096,
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn test_exit_status_and_literal_arguments_are_recorded_independently() -> Result<()> {
    let f = Fixture::new();
    let (_tx, rx) = watch::channel(false);
    let spec = TestCommand {
        program: "/bin/echo".into(),
        args: vec!["$(touch should-not-exist)".into()],
        ..f.spec("")
    };
    let report = f
        .runner()
        .run(&spec, &f.0.join("project"), &f.0.join("pass"), rx.clone())
        .await?;
    assert_eq!(report.verdict, Verdict::Passed);
    assert_eq!(report.exit_code, Some(0));
    assert!(!f.0.join("project/should-not-exist").exists());
    assert_eq!(
        std::fs::read_to_string(report.run_dir.join("stdout.log"))?,
        "$(touch should-not-exist)\n"
    );
    let report = f
        .runner()
        .run(
            &f.spec("echo failing >&2; exit 7"),
            &f.0.join("project"),
            &f.0.join("fail"),
            rx,
        )
        .await?;
    assert_eq!(report.verdict, Verdict::Failed);
    assert_eq!(report.exit_code, Some(7));
    assert!(report.cleanup_confirmed);
    assert_eq!(report.program_sha256.len(), 64);
    assert_eq!(report.profile_sha256.len(), 64);
    Ok(())
}
#[tokio::test]
async fn timeout_and_output_overflow_never_pass_and_children_are_cleaned() -> Result<()> {
    let f = Fixture::new();
    let (_tx, rx) = watch::channel(false);
    let mut spec = f.spec("sleep 90 & echo $! > child.pid; wait");
    spec.timeout_seconds = 3;
    let report = f
        .runner()
        .run(
            &spec,
            &f.0.join("project"),
            &f.0.join("timeout"),
            rx.clone(),
        )
        .await?;
    assert_eq!(report.verdict, Verdict::Error);
    assert!(report.reason.contains("timeout"));
    assert!(report.cleanup_confirmed);
    let pid: i32 = std::fs::read_to_string(f.0.join("project/child.pid"))
        .with_context(|| format!("child PID missing: {report:?}"))?
        .trim()
        .parse()?;
    assert!(snapshot()
        .await?
        .get(&pid)
        .is_none_or(|p| p.state.starts_with('Z')));
    let report = f
        .runner()
        .run(
            &f.spec("while true; do echo aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa; done"),
            &f.0.join("project"),
            &f.0.join("overflow"),
            rx,
        )
        .await?;
    assert_eq!(report.verdict, Verdict::Error);
    assert!(report.reason.contains("output limit"));
    assert!(std::fs::metadata(report.run_dir.join("stdout.log"))?.len() <= 4096);
    Ok(())
}
#[tokio::test]
async fn explicit_cancellation_returns_error_and_invalid_commands_do_not_launch() -> Result<()> {
    let f = Fixture::new();
    let (tx, rx) = watch::channel(false);
    let runner = f.runner();
    let spec = f.spec("sleep 90");
    let dir = f.0.join("cancel");
    let project = f.0.join("project");
    let execute = runner.run(&spec, &project, &dir, rx);
    tokio::pin!(execute);
    let trigger = async {
        tokio::time::sleep(Duration::from_millis(500)).await;
        tx.send(true).unwrap();
    };
    let (report, _) = tokio::join!(execute, trigger);
    let report = report?;
    assert_eq!(report.verdict, Verdict::Error);
    assert!(report.reason.contains("cancelled"));
    assert!(report.cleanup_confirmed);
    let mut invalid = f.spec("");
    invalid.program = "relative-program".into();
    assert!(invalid.validate().is_err());
    invalid = f.spec("");
    invalid.readable_roots = vec!["relative-root".into()];
    assert!(invalid.validate().is_err());
    Ok(())
}

#[tokio::test]
async fn writable_workspace_cannot_contain_test_evidence() -> Result<()> {
    let f = Fixture::new();
    let (_tx, rx) = watch::channel(false);
    let project = f.0.join("project");
    let output = project.join("evidence");
    let result = f
        .runner()
        .run(&f.spec("exit 0"), &project, &output, rx)
        .await;
    assert!(result.unwrap_err().to_string().contains("outside"));
    assert!(!output.exists());
    Ok(())
}
