use anyhow::Result;
use std::{
    path::{Path, PathBuf},
    process::Command,
};
use telos_core::*;
use telos_runtime::workspace::prepare;
use telos_store::Store;

struct Fixture(PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "-c",
            "user.name=Telos Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgSign=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().into()
}
async fn setup(write: bool) -> Result<(Fixture, Store, LoopRecord)> {
    let f = Fixture(std::env::temp_dir().join(format!("telos-workspace-{}", uuid::Uuid::new_v4())));
    let project = f.0.join("project");
    std::fs::create_dir_all(&project)?;
    git(&project, &["init", "--quiet"]);
    std::fs::write(project.join("value"), b"committed")?;
    std::fs::write(project.join("AGENTS.md"), b"Read fixture instructions.")?;
    git(&project, &["add", "."]);
    git(&project, &["commit", "-qm", "fixture baseline"]);
    let s = Store::open(&f.0.join("state")).await?;
    s.register_project("fixture", &project).await?;
    s.configure_project(
        "fixture",
        0,
        &ProjectExecutionPolicy {
            enabled: true,
            sandbox: if write {
                Sandbox::WorkspaceWrite
            } else {
                Sandbox::ReadOnly
            },
            timeout_seconds: 30,
            max_output_bytes: 65536,
            max_steps_per_cycle: 3,
        },
    )
    .await?;
    let l = s
        .create_loop(
            "fixture",
            "delivery",
            LoopKind::Delivery,
            &Contract {
                goal: "change fixture".into(),
                delivery_scope: "fixture worktree".into(),
                criteria: vec![Criterion {
                    id: "value".into(),
                    description: "fixture tests pass".into(),
                }],
                interval_seconds: None,
                max_attempts: 3,
            },
        )
        .await?;
    Ok((f, s, l))
}
#[tokio::test]
async fn worktree_pins_committed_base_preserves_dirty_source_and_survives_restart() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let (f, s, l) = setup(true).await?;
    let project = f.0.join("project");
    let base = git(&project, &["rev-parse", "HEAD"]);
    std::fs::write(project.join("value"), b"user unfinished edit")?;
    let hook = project.join(".git/hooks/post-checkout");
    std::fs::write(
        &hook,
        format!("#!/bin/sh\ntouch '{}'\n", f.0.join("hook-ran").display()),
    )?;
    std::fs::set_permissions(hook, std::fs::Permissions::from_mode(0o700))?;
    let workspace = prepare(&s, &l.id, 1, 1, "HEAD").await?;
    assert_eq!(workspace.state, "ready");
    assert_eq!(workspace.base_commit, base);
    assert_eq!(
        std::fs::read(Path::new(&workspace.path).join("value"))?,
        b"committed"
    );
    assert_eq!(
        std::fs::read(project.join("value"))?,
        b"user unfinished edit"
    );
    assert!(!f.0.join("hook-ran").exists());
    assert!(git(&project, &["worktree", "list", "--porcelain"]).contains(&workspace.path));
    assert_eq!(s.get_loop(&l.id).await?.state, LoopState::Active);
    assert!(s
        .claim_next("fixture", telos_store::now(), 30)
        .await?
        .is_none());
    git(&project, &["add", "value"]);
    git(&project, &["commit", "-qm", "later source commit"]);
    s.close().await;
    let s = Store::open(&f.0.join("state")).await?;
    let retry = prepare(&s, &l.id, 1, 1, "HEAD").await?;
    assert_eq!(retry.id, workspace.id);
    assert_eq!(retry.base_commit, base);
    assert_eq!(
        s.events(&l.id)
            .await?
            .iter()
            .filter(|e| e["kind"] == "workspace_reserved")
            .count(),
        1
    );
    Ok(())
}
#[tokio::test]
async fn workspace_requires_write_authority_and_rejects_checkout_filters() -> Result<()> {
    let (f, s, l) = setup(false).await?;
    assert!(prepare(&s, &l.id, 1, 1, "HEAD").await.is_err());
    assert!(s.workspace_for_loop(&l.id, 1).await?.is_none());
    assert!(!f.0.join("state/workspaces").exists());
    let (f, s, l) = setup(true).await?;
    git(
        &f.0.join("project"),
        &["config", "filter.fixture.smudge", "false"],
    );
    assert!(prepare(&s, &l.id, 1, 1, "HEAD").await.is_err());
    assert!(s.workspace_for_loop(&l.id, 1).await?.is_none());
    Ok(())
}
#[tokio::test]
async fn interrupted_creation_is_only_adopted_after_inspection_and_never_recreated() -> Result<()> {
    let (f, s, l) = setup(true).await?;
    let project = f.0.join("project");
    let base = git(&project, &["rev-parse", "HEAD"]);
    let dir = project.join(".git").canonicalize()?;
    let cycle = s.start_cycle(&l.id, 1, "delivery-workspace").await?;
    let (w, _) = s
        .reserve_workspace(&cycle, 1, 1, dir.to_str().unwrap(), &base)
        .await?;
    assert!(prepare(&s, &l.id, 1, 1, "HEAD").await.is_err());
    assert!(!Path::new(&w.path).exists());
    assert_eq!(
        s.workspace_for_loop(&l.id, 1).await?.unwrap().state,
        "uncertain"
    );
    // Simulate Git finishing before a crash prevented the ledger acknowledgment.
    git(&project, &["worktree", "add", "--detach", &w.path, &base]);
    let recovered = prepare(&s, &l.id, 1, 1, "HEAD").await?;
    assert_eq!(recovered.state, "ready");
    assert_eq!(recovered.id, w.id);
    std::fs::write(Path::new(&w.path).join("user-work"), b"keep this")?;
    assert!(prepare(&s, &l.id, 1, 1, "HEAD").await.is_err());
    assert_eq!(
        std::fs::read(Path::new(&w.path).join("user-work"))?,
        b"keep this"
    );
    Ok(())
}
#[tokio::test]
async fn stale_policy_or_contract_cannot_mark_workspace_ready() -> Result<()> {
    let (f, s, l) = setup(true).await?;
    let project = f.0.join("project");
    let cycle = s.start_cycle(&l.id, 1, "delivery-workspace").await?;
    let (w, _) = s
        .reserve_workspace(
            &cycle,
            1,
            1,
            project.join(".git").canonicalize()?.to_str().unwrap(),
            &git(&project, &["rev-parse", "HEAD"]),
        )
        .await?;
    let current = s.get_loop(&l.id).await?;
    s.update_contract(&l.id, current.version, &current.contract)
        .await?;
    assert!(s.finish_workspace(&w, None).await.is_err());
    assert!(prepare(&s, &l.id, 1, 1, "HEAD").await.is_err());
    assert!(s
        .reserve_workspace(&cycle, 2, 1, &w.repository_git_dir, &w.base_commit)
        .await
        .is_err());
    let revised = prepare(&s, &l.id, 2, 1, "HEAD").await?;
    assert_ne!(revised.id, w.id);
    assert_eq!(revised.contract_revision, 2);
    Ok(())
}

#[tokio::test]
async fn write_steps_require_exact_owned_directory_and_persist_binding() -> Result<()> {
    use telos_runtime::daemon::start_managed;
    let (f, s, l) = setup(true).await?;
    assert!(start_managed(&s, &l.id, 1, "write", None).await.is_err());
    let w = prepare(&s, &l.id, 1, 1, "HEAD").await?;
    let spec = ExecutionSpec {
        task: "write".into(),
        cwd: f.0.join("project").to_str().unwrap().into(),
        sandbox: Sandbox::WorkspaceWrite,
        timeout_seconds: 30,
        max_output_bytes: 65536,
    };
    assert!(s
        .enqueue_managed_step(&w.cycle_id, 1, "bad-root", &spec, telos_store::now(), 1)
        .await
        .is_err());
    let started = start_managed(&s, &l.id, 1, "write", None).await?;
    let retry = start_managed(&s, &l.id, 1, "write", None).await?;
    assert_eq!(started["step_id"], retry["step_id"]);
    let lease = s
        .claim_next("fixture", telos_store::now(), 30)
        .await?
        .unwrap();
    assert_eq!(lease.workspace_id.as_deref(), Some(w.id.as_str()));
    assert_eq!(lease.spec.cwd, w.path);
    assert!(lease.spec.task.contains(&w.base_commit));
    assert!(lease.spec.task.contains("Read fixture instructions."));
    let fake = Verification {
        cycle_id: w.cycle_id,
        contract_revision: 1,
        artifact: "project-observations:old".into(),
        environment: "local-project:fixture".into(),
        observation_complete: true,
        results: vec![],
    };
    assert!(s
        .verify(&fake)
        .await
        .unwrap_err()
        .to_string()
        .contains("artifact-bound acceptance"));
    Ok(())
}

#[tokio::test]
async fn daemon_writes_only_assigned_fixture_and_rejects_changed_git_identity() -> Result<()> {
    use sha2::{Digest, Sha256};
    use std::{os::unix::fs::PermissionsExt, time::Duration};
    use telos_runtime::{
        daemon::{self, start_managed, Request},
        runner::CodexRunner,
        verifier::verify_loop,
    };
    for invalidate in [false, true] {
        let (f, s, l) = setup(true).await?;
        let w = prepare(&s, &l.id, 1, 1, "HEAD").await?;
        s.configure_verification(
            &l.id,
            1,
            0,
            &VerificationPlan {
                test_checks: vec![],
                log_checks: vec![],
                checks: vec![FileHashCheck {
                    criterion_id: "value".into(),
                    relative_path: "value".into(),
                    expected_sha256: format!("{:x}", Sha256::digest(b"committed")),
                }],
            },
        )
        .await?;
        std::fs::write(Path::new(&w.path).join("value"), b"worktree differs")?;
        assert_eq!(verify_loop(&s, &l.id, 1).await?["verdict"], "failed");
        let started = start_managed(&s, &l.id, 1, "write", None).await?;
        if invalidate {
            git(Path::new(&w.path), &["checkout", "-b", "unexpected-branch"]);
        }
        s.close().await;
        let fake = f.0.join("fake-codex");
        std::fs::write(
            &fake,
            r#"#!/usr/bin/env python3
import json,pathlib,sys
prompt=sys.stdin.read()
assert sys.argv[sys.argv.index('--sandbox')+1]=='workspace-write'
assert 'assigned detached worktree only' in prompt
assert 'Original project memory excerpts' in prompt
cwd=pathlib.Path(sys.argv[sys.argv.index('-C')+1])
assert cwd.name.startswith('workspace-')
(cwd/'value').write_text('changed by fixture worker')
result={'status':'done','summary':'Updated assigned worktree','evidence_paths':['value'],'remaining_work':['Independent delivery acceptance still required']}
pathlib.Path(sys.argv[sys.argv.index('-o')+1]).write_text(json.dumps(result))
print(json.dumps({'type':'turn.completed','usage':{'output_tokens':0}}))
"#,
        )?;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700))?;
        let state = f.0.join("state");
        let (tx, rx) = tokio::sync::watch::channel(false);
        let service = tokio::spawn({
            let state = state.clone();
            async move { daemon::serve(&state, CodexRunner { program: fake }, rx).await }
        });
        let mut final_status = serde_json::Value::Null;
        for _ in 0..200 {
            if let Ok(v) = daemon::call(
                &state,
                &Request::StepStatus {
                    id: started["step_id"].as_str().unwrap().into(),
                },
            )
            .await
            {
                if v["state"] == "succeeded" || v["state"] == "failed" {
                    final_status = v;
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        tx.send(true)?;
        tokio::time::timeout(Duration::from_secs(10), service).await???;
        assert_eq!(
            final_status["state"],
            if invalidate { "failed" } else { "succeeded" }
        );
        assert_eq!(final_status["workspace_id"], w.id);
        assert_eq!(std::fs::read(f.0.join("project/value"))?, b"committed");
        assert_eq!(
            std::fs::read(Path::new(&w.path).join("value"))?,
            if invalidate {
                b"worktree differs".as_slice()
            } else {
                b"changed by fixture worker".as_slice()
            }
        );
        let s = Store::open(&state).await?;
        assert_ne!(s.get_loop(&l.id).await?.state, LoopState::Completed);
        if invalidate {
            assert!(final_status["result"]["reason"]
                .as_str()
                .unwrap()
                .contains("workspace preflight"));
        }
    }
    Ok(())
}

#[tokio::test]
async fn delivery_attempts_obey_contract_even_when_project_budget_is_larger() -> Result<()> {
    let (_f, s, l) = setup(true).await?;
    let mut contract = l.contract.clone();
    contract.max_attempts = 1;
    let l = s.update_contract(&l.id, l.version, &contract).await?;
    prepare(&s, &l.id, l.revision, 1, "HEAD").await?;
    telos_runtime::daemon::start_managed(&s, &l.id, l.revision, "first", None).await?;
    let lease = s
        .claim_next("fixture", telos_store::now(), 30)
        .await?
        .unwrap();
    s.finish_step(
        &lease,
        ExecutionOutcome::Succeeded,
        &serde_json::json!({"fixture":true}),
        telos_store::now(),
    )
    .await?;
    let error = telos_runtime::daemon::start_managed(&s, &l.id, l.revision, "second", None)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("contract attempt budget"));
    Ok(())
}

#[tokio::test]
async fn independent_acceptance_reads_worktree_and_binds_the_visible_artifact() -> Result<()> {
    use sha2::{Digest, Sha256};
    use telos_runtime::{verifier::verify_loop, workspace::snapshot};
    let (f, s, l) = setup(true).await?;
    let w = prepare(&s, &l.id, 1, 1, "HEAD").await?;
    let root = Path::new(&w.path);
    s.configure_verification(
        &l.id,
        1,
        0,
        &VerificationPlan {
            test_checks: vec![],
            log_checks: vec![],
            checks: vec![FileHashCheck {
                criterion_id: "value".into(),
                relative_path: "value".into(),
                expected_sha256: format!("{:x}", Sha256::digest(b"delivered")),
            }],
        },
    )
    .await?;
    assert_eq!(verify_loop(&s, &l.id, 1).await?["verdict"], "failed");
    let before = snapshot(&w, &[]).await?;
    std::fs::write(root.join("new-source"), b"new file")?;
    assert_ne!(before, snapshot(&w, &[]).await?);
    std::fs::write(root.join("value"), b"delivered")?;
    let verified = verify_loop(&s, &l.id, 1).await?;
    assert_eq!(verified["verdict"], "passed");
    assert_eq!(s.get_loop(&l.id).await?.state, LoopState::Completed);
    assert_eq!(std::fs::read(f.0.join("project/value"))?, b"committed");
    let evidence = s
        .get_evidence(verified["evidence_id"].as_str().unwrap(), 0, 32768)
        .await?;
    let body: serde_json::Value = serde_json::from_str(evidence["text"].as_str().unwrap())?;
    assert_eq!(body["adapter"], "independent_delivery_observation");
    assert_eq!(body["delivery_snapshot"]["workspace_id"], w.id);
    assert!(body["delivery_snapshot"]["files"]
        .as_array()
        .unwrap()
        .iter()
        .any(|f| f["path"] == "new-source"));
    Ok(())
}

#[tokio::test]
async fn delivery_snapshot_cannot_be_reused_after_another_step_is_enqueued() -> Result<()> {
    use telos_runtime::{daemon::start_managed, workspace::snapshot};
    let (_f, s, l) = setup(true).await?;
    let w = prepare(&s, &l.id, 1, 1, "HEAD").await?;
    s.configure_verification(
        &l.id,
        1,
        0,
        &VerificationPlan {
            test_checks: vec![],
            log_checks: vec![],
            checks: vec![FileHashCheck {
                criterion_id: "value".into(),
                relative_path: "value".into(),
                expected_sha256: "a".repeat(64),
            }],
        },
    )
    .await?;
    let artifact = s
        .record_delivery_snapshot(&w, 1, &snapshot(&w, &[]).await?)
        .await?;
    start_managed(&s, &l.id, 1, "new-attempt", None).await?;
    let verification = Verification {
        cycle_id: w.cycle_id.clone(),
        contract_revision: 1,
        artifact,
        environment: format!("local-workspace:{}", w.id),
        observation_complete: false,
        results: vec![],
    };
    assert!(s
        .verify_configured(&verification, 1)
        .await
        .unwrap_err()
        .to_string()
        .contains("stale or mismatched"));
    assert!(s
        .record_delivery_snapshot(&w, 1, &snapshot(&w, &[]).await?)
        .await
        .is_err());
    Ok(())
}

#[tokio::test]
async fn snapshot_scope_handles_ignored_files_and_does_not_follow_symlink_parents() -> Result<()> {
    use telos_runtime::workspace::snapshot;
    let (f, s, l) = setup(true).await?;
    let w = prepare(&s, &l.id, 1, 1, "HEAD").await?;
    let root = Path::new(&w.path);
    std::fs::write(root.join(".gitignore"), b"build/\n")?;
    let before = snapshot(&w, &[]).await?;
    std::fs::create_dir(root.join("build"))?;
    std::fs::write(root.join("build/cache"), b"ignored artifact")?;
    assert_eq!(before, snapshot(&w, &[]).await?);
    assert_ne!(before, snapshot(&w, &["build/cache".into()]).await?);
    std::fs::create_dir(f.0.join("outside"))?;
    std::fs::write(f.0.join("outside/private"), b"never-read-fixture")?;
    std::os::unix::fs::symlink(f.0.join("outside"), root.join("link"))?;
    let links = snapshot(&w, &[]).await?;
    assert!(!links.to_string().contains("never-read-fixture"));
    assert!(snapshot(&w, &["link/private".into()]).await.is_err());
    assert!(snapshot(&w, &[".git/config".into()]).await.is_err());
    Ok(())
}

#[tokio::test]
async fn delivery_patch_reconstructs_tracked_new_binary_and_symlink_changes() -> Result<()> {
    use telos_runtime::workspace::{review_patch, snapshot};
    let (f, s, l) = setup(true).await?;
    let w = prepare(&s, &l.id, 1, 1, "HEAD").await?;
    let root = Path::new(&w.path);
    std::fs::write(root.join("value"), b"changed\r\nlast\r\n")?;
    git(root, &["rm", "--cached", "value"]);
    std::fs::remove_file(root.join("AGENTS.md"))?;
    std::fs::write(root.join("new file"), b"new\n")?;
    std::fs::write(root.join("binary"), [0, 1, 2, 255, 0])?;
    std::os::unix::fs::symlink("value", root.join("link"))?;
    let manifest = snapshot(&w, &[]).await?;
    let review = review_patch(&w, &manifest).await?;
    assert_eq!(manifest, snapshot(&w, &[]).await?);
    let reconstructed = f.0.join("review-copy");
    git(
        &f.0.join("project"),
        &[
            "worktree",
            "add",
            "--detach",
            reconstructed.to_str().unwrap(),
            &w.base_commit,
        ],
    );
    let patch = f.0.join("review.patch");
    std::fs::write(&patch, review["patch"].as_str().unwrap())?;
    git(
        &reconstructed,
        &["apply", "--check", patch.to_str().unwrap()],
    );
    git(&reconstructed, &["apply", patch.to_str().unwrap()]);
    assert_eq!(
        std::fs::read(reconstructed.join("value"))?,
        b"changed\r\nlast\r\n"
    );
    assert_eq!(std::fs::read(reconstructed.join("new file"))?, b"new\n");
    assert_eq!(
        std::fs::read(reconstructed.join("binary"))?,
        [0, 1, 2, 255, 0]
    );
    assert_eq!(
        std::fs::read_link(reconstructed.join("link"))?,
        PathBuf::from("value")
    );
    assert!(!reconstructed.join("AGENTS.md").exists());
    assert_eq!(std::fs::read(f.0.join("project/value"))?, b"committed");
    Ok(())
}

#[tokio::test]
async fn accepted_delivery_report_survives_later_live_worktree_edits() -> Result<()> {
    use sha2::{Digest, Sha256};
    let (_f, s, l) = setup(true).await?;
    let w = prepare(&s, &l.id, 1, 1, "HEAD").await?;
    assert_eq!(s.delivery_result(&l.id, 1).await?["available"], false);
    s.configure_verification(
        &l.id,
        1,
        0,
        &VerificationPlan {
            test_checks: vec![],
            log_checks: vec![],
            checks: vec![FileHashCheck {
                criterion_id: "value".into(),
                relative_path: "value".into(),
                expected_sha256: format!("{:x}", Sha256::digest(b"accepted")),
            }],
        },
    )
    .await?;
    std::fs::write(Path::new(&w.path).join("value"), b"accepted")?;
    telos_runtime::verifier::verify_loop(&s, &l.id, 1).await?;
    let report = s.delivery_result(&l.id, 1).await?;
    assert_eq!(report["verdict"], "passed");
    let evidence = s
        .get_evidence(report["review_evidence_id"].as_str().unwrap(), 0, 32768)
        .await?;
    let review: serde_json::Value = serde_json::from_str(evidence["text"].as_str().unwrap())?;
    assert!(review["patch"].as_str().unwrap().contains("+accepted"));
    assert!(report["artifact"]
        .as_str()
        .unwrap()
        .ends_with(review["snapshot_sha256"].as_str().unwrap()));
    std::fs::write(Path::new(&w.path).join("value"), b"later user edit")?;
    assert_eq!(report, s.delivery_result(&l.id, 1).await?);
    assert_eq!(
        evidence,
        s.get_evidence(report["review_evidence_id"].as_str().unwrap(), 0, 32768)
            .await?
    );
    Ok(())
}

fn test_plan(script: &str) -> VerificationPlan {
    VerificationPlan {
        checks: vec![],
        log_checks: vec![],
        test_checks: vec![TestCheck {
            observation: None,
            adapter: default_test_adapter(),
            criterion_id: "value".into(),
            command: TestCommand {
                program: "/bin/sh".into(),
                args: vec!["-c".into(), script.into()],
                readable_roots: vec![],
                timeout_seconds: 10,
                max_output_bytes: 4096,
            },
        }],
    }
}
fn test_wrapper(f: &Fixture) -> Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let path = f.0.join("test-codex");
    std::fs::write(
        &path,
        r#"#!/usr/bin/env python3
import os,pathlib,sys
assert sys.argv[1]=='sandbox'
pathlib.Path(__file__).with_suffix('.started').write_text('started')
args=sys.argv[sys.argv.index('--')+1:]
os.execv(args[0],args)
"#,
    )?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
    Ok(path)
}

#[tokio::test]
async fn structured_verifier_keeps_zero_exit_failed_assertion_failed_then_accepts_matching_artifact(
) -> Result<()> {
    let (f, s, l) = setup(true).await?;
    let w = prepare(&s, &l.id, 1, 1, "HEAD").await?;
    let wrapper = test_wrapper(&f)?;
    let mut plan = test_plan(
        r#"if test "$(cat value)" = approved; then printf '{"criterion_id":"value","verdict":"passed","summary":"value matched"}'; else printf '{"criterion_id":"value","verdict":"failed","summary":"value did not match"}'; fi"#,
    );
    plan.test_checks[0].adapter = "local.sandbox_json".into();
    s.configure_verification(&l.id, 1, 0, &plan).await?;
    let (_tx, rx) = tokio::sync::watch::channel(false);
    let failed = telos_runtime::verifier::verify_with_runtime(
        &s,
        &l.id,
        1,
        telos_store::now(),
        &wrapper,
        rx.clone(),
    )
    .await?;
    assert_eq!(failed["verdict"], "failed");
    assert_eq!(s.get_loop(&l.id).await?.state, LoopState::Active);
    let evidence = s
        .get_evidence(failed["evidence_id"].as_str().unwrap(), 0, 32768)
        .await?;
    let observation: serde_json::Value = serde_json::from_str(evidence["text"].as_str().unwrap())?;
    assert_eq!(
        observation["observations"][0]["detail"]["report"]["exit_code"],
        0
    );
    assert_eq!(
        observation["observations"][0]["detail"]["assertion"]["verdict"],
        "failed"
    );
    std::fs::write(Path::new(&w.path).join("value"), b"approved")?;
    let accepted = telos_runtime::verifier::verify_with_runtime(
        &s,
        &l.id,
        1,
        telos_store::now(),
        &wrapper,
        rx,
    )
    .await?;
    assert_eq!(accepted["verdict"], "passed");
    assert_eq!(s.get_loop(&l.id).await?.state, LoopState::Completed);
    assert_eq!(std::fs::read(f.0.join("project/value"))?, b"committed");
    assert!(s.delivery_result(&l.id, 1).await?["review_evidence_id"].is_string());
    Ok(())
}

#[tokio::test]
async fn structured_verifier_rejects_missing_wrong_criterion_and_nonzero_passing_json() -> Result<()>
{
    let (f, s, l) = setup(true).await?;
    prepare(&s, &l.id, 1, 1, "HEAD").await?;
    let wrapper = test_wrapper(&f)?;
    let (_tx, rx) = tokio::sync::watch::channel(false);
    for (version,(script,expected)) in [
        ("printf 'not-json'","error"),
        (r#"printf '{"criterion_id":"other","verdict":"passed","summary":"wrong criterion"}'"#,"error"),
        (r#"printf '{"criterion_id":"value","verdict":"passed","summary":"claimed success"}'; exit 7"#,"failed"),
    ].into_iter().enumerate() {
        let mut plan=test_plan(script);plan.test_checks[0].adapter="local.sandbox_json".into();
        s.configure_verification(&l.id,1,version as i64,&plan).await?;
        let result=telos_runtime::verifier::verify_with_runtime(&s,&l.id,1,telos_store::now(),&wrapper,rx.clone()).await?;
        assert_ne!(result["verdict"],"passed");
        assert_eq!(s.get_loop(&l.id).await?.state,LoopState::Active);
        let context=s.task_context(&l.id,1).await?;
        assert_eq!(context["latest_verification"]["results"][0]["verdict"],expected);
        assert!(s.verification_status(&l.id).await?["active_verification"].is_null());
    }
    Ok(())
}

#[tokio::test]
async fn unregistered_verifier_is_refused_before_lease_or_process_dispatch() -> Result<()> {
    let (f, s, l) = setup(true).await?;
    prepare(&s, &l.id, 1, 1, "HEAD").await?;
    let wrapper = test_wrapper(&f)?;
    let mut plan = test_plan("exit 0");
    plan.test_checks[0].adapter = "unregistered.fixture".into();
    assert!(telos_runtime::verification_adapters::validate_plan(&plan).is_err());
    // Direct Store configuration is an internal trusted path. Runtime checks
    // registration again before acquiring a verifier even for such saved plans.
    s.configure_verification(&l.id, 1, 0, &plan).await?;
    let (_tx, rx) = tokio::sync::watch::channel(false);
    assert!(telos_runtime::verifier::verify_with_runtime(
        &s,
        &l.id,
        1,
        telos_store::now(),
        &wrapper,
        rx
    )
    .await
    .is_err());
    assert!(s.verification_status(&l.id).await?["active_verification"].is_null());
    assert!(!wrapper.with_extension("started").exists());
    Ok(())
}

#[tokio::test]
async fn test_acceptance_records_real_command_failure_then_pass_and_output_evidence() -> Result<()>
{
    let (f, s, l) = setup(true).await?;
    let w = prepare(&s, &l.id, 1, 1, "HEAD").await?;
    let wrapper = test_wrapper(&f)?;
    s.configure_verification(
        &l.id,
        1,
        0,
        &test_plan("cat value; test \"$(cat value)\" = approved"),
    )
    .await?;
    let (_tx, rx) = tokio::sync::watch::channel(false);
    let fail = telos_runtime::verifier::verify_with_runtime(
        &s,
        &l.id,
        1,
        telos_store::now(),
        &wrapper,
        rx.clone(),
    )
    .await?;
    assert_eq!(fail["verdict"], "failed");
    assert_eq!(s.get_loop(&l.id).await?.state, LoopState::Active);
    std::fs::write(Path::new(&w.path).join("value"), "approved")?;
    let pass = telos_runtime::verifier::verify_with_runtime(
        &s,
        &l.id,
        1,
        telos_store::now(),
        &wrapper,
        rx,
    )
    .await?;
    assert_eq!(pass["verdict"], "passed");
    assert_eq!(s.get_loop(&l.id).await?.state, LoopState::Completed);
    assert_eq!(std::fs::read(f.0.join("project/value"))?, b"committed");
    let evidence = s.list_evidence(&l.id, None, 100).await?;
    let mut found_stdout = false;
    for item in evidence["items"].as_array().unwrap() {
        let body = s
            .get_evidence(item["id"].as_str().unwrap(), 0, 32768)
            .await?;
        if body["text"] == "approved" {
            found_stdout = true;
        }
    }
    assert!(found_stdout, "stdout must be saved as immutable evidence");
    let events = s.events(&l.id).await?;
    assert_eq!(
        events
            .iter()
            .filter(|e| e["kind"] == "test_verification_reserved")
            .count(),
        2
    );
    Ok(())
}

#[tokio::test]
async fn tests_cannot_accept_changed_sources_or_exceed_persisted_attempt_budget() -> Result<()> {
    let (f, s, l) = setup(true).await?;
    prepare(&s, &l.id, 1, 1, "HEAD").await?;
    let wrapper = test_wrapper(&f)?;
    s.configure_verification(&l.id, 1, 0, &test_plan("printf changed > value; exit 0"))
        .await?;
    let (_tx, rx) = tokio::sync::watch::channel(false);
    let error = telos_runtime::verifier::verify_with_runtime(
        &s,
        &l.id,
        1,
        telos_store::now(),
        &wrapper,
        rx.clone(),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("artifact changed"));
    s.configure_verification(&l.id, 1, 1, &test_plan("exit 7"))
        .await?;
    for _ in 0..2 {
        assert_eq!(
            telos_runtime::verifier::verify_with_runtime(
                &s,
                &l.id,
                1,
                telos_store::now(),
                &wrapper,
                rx.clone()
            )
            .await?["verdict"],
            "failed"
        );
    }
    // A plan edit and process restart must not reset the cycle's test budget.
    s.configure_verification(&l.id, 1, 2, &test_plan("exit 0"))
        .await?;
    s.close().await;
    let s = Store::open(&f.0.join("state")).await?;
    let error = telos_runtime::verifier::verify_with_runtime(
        &s,
        &l.id,
        1,
        telos_store::now(),
        &wrapper,
        rx,
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("budget exhausted"));
    assert_eq!(s.get_loop(&l.id).await?.state, LoopState::Active);
    Ok(())
}

#[tokio::test]
async fn unknown_test_launch_preserves_ownership_instead_of_repeating() -> Result<()> {
    let (f, s, l) = setup(true).await?;
    prepare(&s, &l.id, 1, 1, "HEAD").await?;
    s.configure_verification(&l.id, 1, 0, &test_plan("exit 0"))
        .await?;
    let (_tx, rx) = tokio::sync::watch::channel(false);
    assert!(telos_runtime::verifier::verify_with_runtime(
        &s,
        &l.id,
        1,
        telos_store::now(),
        &f.0.join("missing-codex"),
        rx
    )
    .await
    .is_err());
    assert!(s.acquire_verification(&l.id, 1, 1).await.is_err());
    assert!(s
        .events(&l.id)
        .await?
        .iter()
        .any(|e| e["kind"] == "verification_outcome_unknown"));
    assert_eq!(s.get_loop(&l.id).await?.state, LoopState::Active);
    Ok(())
}

#[tokio::test]
async fn pausing_a_goal_cancels_its_running_test_and_releases_known_ownership() -> Result<()> {
    let (f, s, l) = setup(true).await?;
    prepare(&s, &l.id, 1, 1, "HEAD").await?;
    let wrapper = test_wrapper(&f)?;
    s.configure_verification(&l.id, 1, 0, &test_plan("sleep 90"))
        .await?;
    let (_tx, rx) = tokio::sync::watch::channel(false);
    let task = tokio::spawn({
        let s = s.clone();
        let id = l.id.clone();
        let wrapper = wrapper.clone();
        async move {
            telos_runtime::verifier::verify_with_runtime(
                &s,
                &id,
                1,
                telos_store::now(),
                &wrapper,
                rx,
            )
            .await
        }
    });
    tokio::time::timeout(std::time::Duration::from_secs(8), async {
        while !wrapper.with_extension("started").exists() {
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        }
    })
    .await?;
    let current = s.get_loop(&l.id).await?;
    let paused = s.control(&l.id, current.version, "pause").await?;
    assert!(task.await?.is_err());
    let resumed = s.control(&l.id, paused.version, "resume").await?;
    assert_eq!(resumed.state, LoopState::Active);
    let lease = s.acquire_verification(&l.id, 1, 1).await?;
    s.finish_verification(&lease).await?;
    Ok(())
}

#[tokio::test]
async fn daemon_shutdown_drains_inflight_test_request_and_keeps_cleanup_known() -> Result<()> {
    use telos_runtime::{
        daemon::{self, Request},
        runner::CodexRunner,
    };
    let (f, s, l) = setup(true).await?;
    prepare(&s, &l.id, 1, 1, "HEAD").await?;
    let wrapper = test_wrapper(&f)?;
    s.configure_verification(&l.id, 1, 0, &test_plan("sleep 90"))
        .await?;
    s.close().await;
    let state = f.0.join("state");
    let (stop, shutdown) = tokio::sync::watch::channel(false);
    let daemon_task = tokio::spawn({
        let state = state.clone();
        let wrapper = wrapper.clone();
        async move { daemon::serve(&state, CodexRunner { program: wrapper }, shutdown).await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(8), async {
        while daemon::call(&state, &Request::Ping).await.is_err() {
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        }
    })
    .await?;
    let request = tokio::spawn({
        let state = state.clone();
        let id = l.id.clone();
        async move { daemon::call(&state, &Request::VerifyLoop { id, revision: 1 }).await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(8), async {
        while !wrapper.with_extension("started").exists() {
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        }
    })
    .await?;
    stop.send(true)?;
    let result = tokio::time::timeout(std::time::Duration::from_secs(8), request).await???;
    assert_eq!(result["verdict"], "error");
    tokio::time::timeout(std::time::Duration::from_secs(8), daemon_task).await???;
    let s = Store::open(&state).await?;
    assert_eq!(s.reconcile_verification_leases().await?, 0);
    assert_eq!(s.get_loop(&l.id).await?.state, LoopState::Active);
    let lease = s.acquire_verification(&l.id, 1, 1).await?;
    s.finish_verification(&lease).await?;
    s.close().await;
    Ok(())
}

#[tokio::test]
async fn unknown_tests_recover_only_with_complete_cleanup_evidence_and_keep_budget() -> Result<()> {
    use telos_runtime::{
        process::{snapshot, ProcessSet},
        test_runner::TestRunner,
        verification_recovery::reconcile,
    };
    let (f, s, l) = setup(true).await?;
    let w = prepare(&s, &l.id, 1, 1, "HEAD").await?;
    let plan = test_plan("echo failed; exit 7");
    s.configure_verification(&l.id, 1, 0, &plan).await?;
    let lease = s.acquire_verification(&l.id, 1, 1).await?;
    s.reserve_test_attempt(&lease).await?;
    let parent = s.verification_run_parent();
    std::fs::create_dir_all(&parent)?;
    let dir = parent.join(format!("{lease}-0"));
    let (_tx, rx) = tokio::sync::watch::channel(false);
    let report = TestRunner {
        codex: test_wrapper(&f)?,
    }
    .run(&plan.test_checks[0].command, Path::new(&w.path), &dir, rx)
    .await?;
    assert!(report.cleanup_confirmed);
    // Simulate interruption after the cleanup report but before ledger finalization.
    s.mark_verification_unknown(&lease, "interrupted after report")
        .await?;
    assert_eq!(
        s.verification_status(&l.id).await?["active_verification"]["lease_id"],
        lease
    );
    let original = std::fs::read(dir.join("stdout.log"))?;
    std::fs::write(dir.join("stdout.log"), "tampered")?;
    assert!(reconcile(&s, &lease)
        .await
        .unwrap_err()
        .to_string()
        .contains("integrity"));
    std::fs::write(dir.join("stdout.log"), original)?;
    let journal = std::fs::read(dir.join("processes.json"))?;
    let mut alive = ProcessSet::new(std::process::id() as i32);
    alive.observe(&snapshot().await?);
    alive.persist(&dir.join("processes.json")).await?;
    assert!(reconcile(&s, &lease)
        .await
        .unwrap_err()
        .to_string()
        .contains("still alive"));
    std::fs::write(dir.join("processes.json"), journal)?;
    let report_file = std::fs::read(dir.join("report.json"))?;
    std::fs::remove_file(dir.join("report.json"))?;
    assert!(reconcile(&s, &lease).await.is_err());
    std::fs::write(dir.join("report.json"), report_file)?;
    let resolved = reconcile(&s, &lease).await?;
    assert_eq!(resolved["disposition"], "discarded");
    assert_eq!(resolved["automatic_retry"], false);
    assert!(s.verification_status(&l.id).await?["active_verification"].is_null());
    assert_eq!(s.get_loop(&l.id).await?.state, LoopState::Active);
    assert!(reconcile(&s, &lease).await.is_err());
    for _ in 0..2 {
        let next = s.acquire_verification(&l.id, 1, 1).await?;
        s.reserve_test_attempt(&next).await?;
        s.finish_verification(&next).await?;
    }
    let next = s.acquire_verification(&l.id, 1, 1).await?;
    assert!(s
        .reserve_test_attempt(&next)
        .await
        .unwrap_err()
        .to_string()
        .contains("budget exhausted"));
    s.finish_verification(&next).await?;
    assert_eq!(
        s.events(&l.id)
            .await?
            .iter()
            .filter(|e| e["kind"] == "verification_reconciled")
            .count(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn interrupted_observation_without_test_launch_can_be_discarded() -> Result<()> {
    let (_f, s, l) = setup(true).await?;
    prepare(&s, &l.id, 1, 1, "HEAD").await?;
    s.configure_verification(&l.id, 1, 0, &test_plan("exit 0"))
        .await?;
    let lease = s.acquire_verification(&l.id, 1, 1).await?;
    s.mark_verification_unknown(&lease, "interrupted before test reservation")
        .await?;
    telos_runtime::verification_recovery::reconcile(&s, &lease).await?;
    assert_eq!(s.get_loop(&l.id).await?.state, LoopState::Active);
    let next = s.acquire_verification(&l.id, 1, 1).await?;
    s.reserve_test_attempt(&next).await?;
    s.mark_verification_unknown(&next, "missing run journal")
        .await?;
    assert!(telos_runtime::verification_recovery::reconcile(&s, &next)
        .await
        .is_err());
    assert_eq!(
        s.verification_status(&l.id).await?["active_verification"]["state"],
        "outcome_unknown"
    );
    Ok(())
}

#[tokio::test]
async fn delivery_automatically_repairs_failed_tests_and_stops_at_budget() -> Result<()> {
    use std::{os::unix::fs::PermissionsExt, time::Duration};
    use telos_runtime::{
        daemon::{self, Request},
        runner::CodexRunner,
    };
    for succeeds in [true, false] {
        let (f, s, l) = setup(true).await?;
        let w = prepare(&s, &l.id, 1, 1, "HEAD").await?;
        s.configure_verification(
            &l.id,
            1,
            0,
            &test_plan("cat value; test \"$(cat value)\" = approved"),
        )
        .await?;
        let fake = f.0.join("repair-codex");
        std::fs::write(
            &fake,
            format!(
                r#"#!/usr/bin/env python3
import os,pathlib,sys,json
if sys.argv[1]=='sandbox':
    args=sys.argv[sys.argv.index('--')+1:]
    os.execv(args[0],args)
args=sys.argv
prompt=sys.stdin.read()
root=pathlib.Path(args[args.index('-C')+1])
repair='Repair the independently verified failure' in prompt
if repair:
    assert 'Independent failure evidence excerpts:' in prompt
    assert 'committed' in prompt or 'wrong' in prompt
(root/'value').write_text('approved' if repair and {} else 'wrong')
pathlib.Path(args[args.index('-o')+1]).write_text(json.dumps({{'status':'done','summary':'fixture edit','evidence_paths':[],'remaining_work':[]}}))
print(json.dumps({{'type':'turn.completed','usage':{{'output_tokens':0}}}}))
"#,
                if succeeds { "True" } else { "False" }
            ),
        )?;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700))?;
        daemon::start_managed(&s, &l.id, 1, "user-start", None).await?;
        s.close().await;
        let state = f.0.join("state");
        let (stop, shutdown) = tokio::sync::watch::channel(false);
        let service = tokio::spawn({
            let state = state.clone();
            async move { daemon::serve(&state, CodexRunner { program: fake }, shutdown).await }
        });
        let outcome: Result<()> = async {
            let mut failed_verifications = std::collections::BTreeSet::new();
            tokio::time::timeout(Duration::from_secs(30), async {
                loop {
                    if let Ok(goal) =
                        daemon::call(&state, &Request::GetLoop { id: l.id.clone() }).await
                    {
                        if succeeds && goal["state"] == "completed" {
                            break;
                        }
                        if !succeeds {
                            let delivery = daemon::call(
                                &state,
                                &Request::DeliveryResult {
                                    id: l.id.clone(),
                                    revision: 1,
                                },
                            )
                            .await?;
                            if delivery["verdict"] == "failed" {
                                failed_verifications.insert(
                                    delivery["verification_id"].as_str().unwrap().to_owned(),
                                );
                                let verification = daemon::call(
                                    &state,
                                    &Request::VerificationStatus { id: l.id.clone() },
                                )
                                .await?;
                                if failed_verifications.len() == 3
                                    && verification["active_verification"].is_null()
                                {
                                    break;
                                }
                            }
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Ok::<(), anyhow::Error>(())
            })
            .await??;
            Ok(())
        }
        .await;
        let _ = stop.send(true);
        tokio::time::timeout(Duration::from_secs(10), service).await???;
        let s = Store::open(&state).await?;
        let events = s.events(&l.id).await?;
        if let Err(error) = outcome {
            anyhow::bail!(
                "repair scenario succeeds={succeeds}: {error}; events={}; context={}",
                serde_json::to_string(&events)?,
                s.task_context(&l.id, 1).await?
            );
        }
        assert_eq!(
            events
                .iter()
                .filter(|e| e["kind"] == "repair_enqueued")
                .count(),
            if succeeds { 1 } else { 2 }
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| e["kind"] == "test_verification_reserved")
                .count(),
            if succeeds { 2 } else { 3 }
        );
        assert_eq!(
            s.get_loop(&l.id).await?.state,
            if succeeds {
                LoopState::Completed
            } else {
                LoopState::Active
            }
        );
        assert!(s.pending_repairs().await?.is_empty());
        assert_eq!(std::fs::read(f.0.join("project/value"))?, b"committed");
        assert_eq!(
            std::fs::read(Path::new(&w.path).join("value"))?,
            if succeeds {
                b"approved".as_slice()
            } else {
                b"wrong".as_slice()
            }
        );
        s.close().await;
    }
    Ok(())
}

#[tokio::test]
async fn repair_rechecks_authority_versions_snapshot_and_duplicate_enqueue() -> Result<()> {
    use sha2::{Digest, Sha256};
    use telos_runtime::{daemon::start_managed, repair::dispatch, verifier::verify_loop};
    let (_f, s, l) = setup(true).await?;
    let w = prepare(&s, &l.id, 1, 1, "HEAD").await?;
    let plan = VerificationPlan {
        test_checks: vec![],
        log_checks: vec![],
        checks: vec![FileHashCheck {
            criterion_id: "value".into(),
            relative_path: "value".into(),
            expected_sha256: format!("{:x}", Sha256::digest(b"approved")),
        }],
    };
    s.configure_verification(&l.id, 1, 0, &plan).await?;
    assert_eq!(verify_loop(&s, &l.id, 1).await?["verdict"], "failed");
    assert!(
        s.pending_repairs().await?.is_empty(),
        "verification alone must not arm writing"
    );
    start_managed(&s, &l.id, 1, "user-start", None).await?;
    let lease = s
        .claim_next("fixture", telos_store::now(), 30)
        .await?
        .unwrap();
    s.finish_step(
        &lease,
        ExecutionOutcome::Succeeded,
        &serde_json::json!({"reason":"synthetic prior execution"}),
        telos_store::now(),
    )
    .await?;
    verify_loop(&s, &l.id, 1).await?;
    assert_eq!(
        s.task_context(&l.id, 1).await?["progress"]["repair_pending"],
        true
    );
    let candidate = s.pending_repairs().await?.remove(0);
    let spec = ExecutionSpec {
        task: "repair fixture".into(),
        cwd: w.path.clone(),
        sandbox: Sandbox::WorkspaceWrite,
        timeout_seconds: 30,
        max_output_bytes: 65536,
    };
    let current = s.get_loop(&l.id).await?;
    let paused = s.control(&l.id, current.version, "pause").await?;
    assert!(s.pending_repairs().await?.is_empty());
    assert!(s.enqueue_repair(&candidate, &spec).await.is_err());
    s.control(&l.id, paused.version, "resume").await?;
    s.configure_verification(&l.id, 1, 1, &plan).await?;
    assert!(s.enqueue_repair(&candidate, &spec).await.is_err());
    assert!(s.pending_repairs().await?.is_empty());
    verify_loop(&s, &l.id, 1).await?;
    std::fs::write(Path::new(&w.path).join("value"), "user change")?;
    dispatch(&s).await?;
    dispatch(&s).await?;
    assert!(s.pending_repairs().await?.is_empty());
    assert_eq!(
        s.events(&l.id)
            .await?
            .iter()
            .filter(|e| e["kind"] == "repair_deferred")
            .count(),
        1
    );
    verify_loop(&s, &l.id, 1).await?;
    let candidate = s.pending_repairs().await?.remove(0);
    let (a, b) = tokio::join!(
        s.enqueue_repair(&candidate, &spec),
        s.enqueue_repair(&candidate, &spec)
    );
    assert_ne!(a.is_ok(), b.is_ok());
    let step = a.or(b)?;
    let progress = s.task_context(&l.id, 1).await?["progress"].clone();
    assert_eq!(progress["execution_in_flight"], true);
    assert_eq!(progress["repair_pending"], false);
    assert!(s.repair_preflight(&step).await?.is_some());
    let repair = s
        .claim_next("fixture", telos_store::now(), 30)
        .await?
        .unwrap();
    s.finish_step(&repair, ExecutionOutcome::Succeeded,
        &serde_json::json!({"reason":"synthetic correction, independent verification still pending"}), telos_store::now()).await?;
    assert_eq!(
        s.task_context(&l.id, 1).await?["progress"]["verification_pending"],
        true
    );
    s.configure_verification(&l.id, 1, 2, &plan).await?;
    assert!(s.repair_preflight(&step).await.is_err());
    assert!(s.pending_repairs().await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn copied_git_pointer_is_not_a_registered_recovery_worktree() -> Result<()> {
    let (f, store, goal) = setup(true).await?;
    let workspace = prepare(&store, &goal.id, 1, 1, "HEAD").await?;
    telos_runtime::workspace::validate_identity(&workspace).await?;
    let original = Path::new(&workspace.path);
    let copy = f.0.join("copied-worktree");
    std::fs::create_dir(&copy)?;
    std::fs::copy(original.join(".git"), copy.join(".git"))?;
    std::fs::copy(original.join("value"), copy.join("value"))?;
    let mut candidate = workspace.clone();
    candidate.path = copy.to_string_lossy().into();
    let error = telos_runtime::workspace::validate_identity(&candidate)
        .await
        .unwrap_err();
    assert!(error
        .to_string()
        .contains("registration points to a different checkout"));
    telos_runtime::workspace::validate_identity(&workspace).await?;
    assert!(
        git(&f.0.join("project"), &["worktree", "list", "--porcelain"]).contains(&workspace.path)
    );
    store.close().await;
    Ok(())
}

#[tokio::test]
async fn recovered_workspace_adoption_rebinds_files_and_invalidates_old_plan() -> Result<()> {
    use sha2::{Digest, Sha256};
    let (f, store, goal) = setup(true).await?;
    let workspace = prepare(&store, &goal.id, 1, 1, "HEAD").await?;
    let plan = VerificationPlan {
        checks: vec![FileHashCheck {
            criterion_id: "value".into(),
            relative_path: "value".into(),
            expected_sha256: "a".repeat(64),
        }],
        test_checks: vec![],
        log_checks: vec![],
    };
    store.configure_verification(&goal.id, 1, 0, &plan).await?;
    std::fs::write(
        Path::new(&workspace.path).join("value"),
        b"preserved unfinished work",
    )?;
    let observed = telos_runtime::workspace::snapshot(&workspace, &["value".into()]).await?;
    store
        .record_delivery_snapshot(&workspace, 1, &observed)
        .await?;
    let manifest = "a".repeat(64);
    store
        .prepare_recovery(&manifest, &"b".repeat(64), &f.0.join("state"))
        .await?;
    let evidence = store
        .record_recovery_review_evidence(
            "fixture",
            &manifest,
            "synthetic-adoption",
            b"Synthetic exclusive-workspace review; no real project approval",
        )
        .await?;
    // Simulate an independently performed Git relocation; product adoption must not repair Git.
    let candidate = f.0.join("relocated-worktree");
    git(
        &f.0.join("project"),
        &[
            "worktree",
            "move",
            &workspace.path,
            candidate.to_str().unwrap(),
        ],
    );
    let candidate = candidate.canonicalize()?;
    let mut adoption = RecoveryWorkspaceAdoption {
        review: RecoveryProjectReview {
            project_id: "fixture".into(),
            manifest_sha256: manifest,
            decision_id: "synthetic-adoption".into(),
            operator: "fixture".into(),
            statement: "Synthetic fixture only".into(),
            reviewed_external_state: true,
            evidence: vec![evidence],
        },
        workspace_id: workspace.id.clone(),
        candidate_path: candidate.to_string_lossy().into(),
        expected_snapshot_sha256: format!("{:x}", Sha256::digest(serde_json::to_vec(&observed)?)),
        exclusive_access_confirmed: false,
    };
    assert!(
        telos_runtime::recovery_preparation::adopt_workspace(&store, &adoption)
            .await
            .is_err()
    );
    adoption.exclusive_access_confirmed = true;
    std::fs::write(candidate.join("value"), b"changed after inspection")?;
    assert!(
        telos_runtime::recovery_preparation::adopt_workspace(&store, &adoption)
            .await
            .is_err()
    );
    assert_eq!(
        store.workspace_for_loop(&goal.id, 1).await?.unwrap().state,
        "uncertain"
    );
    std::fs::write(candidate.join("value"), b"preserved unfinished work")?;
    let result = telos_runtime::recovery_preparation::adopt_workspace(&store, &adoption).await?;
    assert_eq!(result["execution_started"], false);
    let recovered = store.workspace_for_loop(&goal.id, 1).await?.unwrap();
    assert_eq!(recovered.path, adoption.candidate_path);
    assert_eq!(recovered.state, "ready");
    assert_eq!(
        store.verification_plan(&goal.id, 1).await?.unwrap()["version"],
        2
    );
    assert_eq!(store.get_loop(&goal.id).await?.state, LoopState::Paused);
    assert_eq!(
        store.recovery_project_status("fixture").await?.unwrap()["state"],
        "held"
    );
    assert!(store
        .claim_next("must-not-run", telos_store::now(), 30)
        .await?
        .is_none());
    assert_eq!(
        std::fs::read(candidate.join("value"))?,
        b"preserved unfinished work"
    );
    assert_eq!(git(&f.0.join("project"), &["status", "--porcelain"]), "");
    assert!(
        telos_runtime::recovery_preparation::adopt_workspace(&store, &adoption)
            .await
            .is_err()
    );
    store.close().await;
    Ok(())
}
