use anyhow::Result;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use telos_core::*;
use telos_runtime::{
    daemon::{dispatch_due, start_managed},
    verifier::verify_loop,
};
use telos_store::{now, Store};

struct Fixture(PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn policy() -> ProjectExecutionPolicy {
    ProjectExecutionPolicy {
        enabled: true,
        sandbox: Sandbox::ReadOnly,
        timeout_seconds: 30,
        max_output_bytes: 65536,
        max_steps_per_cycle: 2,
    }
}

async fn setup() -> Result<(Fixture, Store, LoopRecord)> {
    let f = Fixture(std::env::temp_dir().join(format!("telos-schedule-{}", uuid::Uuid::new_v4())));
    let s = Store::open(&f.0.join("state")).await?;
    std::fs::create_dir(f.0.join("project"))?;
    std::fs::write(f.0.join("project/value"), b"good")?;
    s.register_project("fixture", &f.0.join("project")).await?;
    s.configure_project("fixture", 0, &policy()).await?;
    let l = s
        .create_loop(
            "fixture",
            "watch",
            LoopKind::Continuous,
            &Contract {
                goal: "observe fixture".into(),
                delivery_scope: "fixture".into(),
                criteria: vec![Criterion {
                    id: "value".into(),
                    description: "exact fixture bytes".into(),
                }],
                interval_seconds: Some(30),
                max_attempts: 2,
            },
        )
        .await?;
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
                expected_sha256: format!("{:x}", Sha256::digest(b"good")),
            }],
        },
    )
    .await?;
    Ok((f, s, l))
}

async fn first_pass(s: &Store, l: &LoopRecord) -> Result<i64> {
    start_managed(s, &l.id, 1, "explicit-user-start", None).await?;
    let lease = s.claim_next("fixture", now(), 30).await?.unwrap();
    s.finish_step(
        &lease,
        ExecutionOutcome::Succeeded,
        &serde_json::json!({"fixture":true}),
        now(),
    )
    .await?;
    verify_loop(s, &l.id, 1).await?;
    Ok(s.get_loop(&l.id).await?.next_due_at.unwrap())
}

#[tokio::test]
async fn only_started_goals_recur_after_pass_and_restart_without_duplicate_dispatch() -> Result<()>
{
    let (f, s, l) = setup().await?;
    assert!(s.due_schedules(now() + 1000).await?.is_empty());
    let due = first_pass(&s, &l).await?;
    assert!(s.task_context(&l.id, 1).await?["active_cycle_id"].is_null());
    assert!(s.due_schedules(due - 1).await?.is_empty());
    assert_eq!(s.due_schedules(due + 10000).await?.len(), 1);
    s.close().await;
    let s = Store::open(&f.0.join("state")).await?;
    assert_eq!(s.schedule_status(&l.id).await?["armed"], true);
    let (a, b) = tokio::join!(dispatch_due(&s, due + 10000), dispatch_due(&s, due + 10000));
    a?;
    b?;
    assert!(s.due_schedules(due + 20000).await?.is_empty());
    assert!(s.task_context(&l.id, 1).await?["active_cycle_id"].is_string());
    let events = s.events(&l.id).await?;
    assert_eq!(
        events
            .iter()
            .filter(|e| e["kind"] == "step_enqueued")
            .count(),
        2
    );
    Ok(())
}

#[tokio::test]
async fn pause_and_policy_or_contract_changes_prevent_old_schedule_dispatch() -> Result<()> {
    let (_f, s, l) = setup().await?;
    let due = first_pass(&s, &l).await?;
    let current = s.get_loop(&l.id).await?;
    let paused = s.control(&l.id, current.version, "pause").await?;
    dispatch_due(&s, due).await?;
    assert!(s.due_schedules(due).await?.is_empty());
    s.control(&l.id, paused.version, "resume").await?;
    assert_eq!(s.due_schedules(due).await?.len(), 1);
    s.configure_project("fixture", 1, &policy()).await?;
    assert_eq!(s.schedule_status(&l.id).await?["armed"], false);
    assert!(s.due_schedules(due).await?.is_empty());
    assert!(start_managed(&s, &l.id, 1, "stale-schedule", Some(1))
        .await
        .is_err());
    start_managed(&s, &l.id, 1, "explicit-rearm", None).await?;
    let current = s.get_loop(&l.id).await?;
    s.update_contract(&l.id, current.version, &current.contract)
        .await?;
    assert_eq!(s.schedule_status(&l.id).await?["armed"], false);
    assert!(s.due_schedules(due + 1000).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn unavailable_project_suspends_instead_of_polling_execution_forever() -> Result<()> {
    let (f, s, l) = setup().await?;
    let due = first_pass(&s, &l).await?;
    std::fs::rename(f.0.join("project"), f.0.join("project-away"))?;
    dispatch_due(&s, due).await?;
    assert_eq!(s.schedule_status(&l.id).await?["armed"], false);
    dispatch_due(&s, due + 60).await?;
    let events = s.events(&l.id).await?;
    assert_eq!(
        events
            .iter()
            .filter(|e| e["kind"] == "schedule_suspended")
            .count(),
        1
    );
    Ok(())
}

async fn failed_observation(f: &Fixture, s: &Store, l: &LoopRecord) -> Result<()> {
    std::fs::write(f.0.join("project/value"), b"bad")?;
    start_managed(s, &l.id, 1, "user-start", None).await?;
    let lease = s.claim_next("fixture", now(), 30).await?.unwrap();
    s.finish_step(
        &lease,
        ExecutionOutcome::Succeeded,
        &serde_json::json!({"fixture":true}),
        now(),
    )
    .await?;
    verify_loop(s, &l.id, 1).await?;
    Ok(())
}

#[tokio::test]
async fn failure_diagnosis_is_once_per_cycle_and_never_verifies_itself() -> Result<()> {
    use telos_runtime::daemon::dispatch_diagnoses;
    let (f, s, l) = setup().await?;
    failed_observation(&f, &s, &l).await?;
    assert_eq!(s.pending_diagnoses().await?.len(), 1);
    let (a, b) = tokio::join!(dispatch_diagnoses(&s), dispatch_diagnoses(&s));
    a?;
    b?;
    let lease = s.claim_next("diagnosis", now(), 30).await?.unwrap();
    assert!(lease
        .spec
        .task
        .contains("Diagnose the independently observed failure"));
    assert!(lease.spec.task.contains("observed_sha256"));
    assert_eq!(lease.spec.sandbox, Sandbox::ReadOnly);
    assert!(s.claim_next("other", now(), 30).await?.is_none());
    s.finish_step(&lease, ExecutionOutcome::Succeeded, &serde_json::json!({"result":{"summary":"cause diagnosed", "remaining_work":["proposed correction"]}}), now()).await?;
    verify_loop(&s, &l.id, 1).await?;
    assert_eq!(s.get_loop(&l.id).await?.state, LoopState::Active);
    assert!(s.pending_diagnoses().await?.is_empty());
    assert_eq!(
        s.events(&l.id)
            .await?
            .iter()
            .filter(|e| e["kind"] == "diagnosis_enqueued")
            .count(),
        1
    );
    s.close().await;
    let s = Store::open(&f.0.join("state")).await?;
    dispatch_diagnoses(&s).await?;
    assert!(s.claim_next("restart", now(), 30).await?.is_none());
    std::fs::write(f.0.join("project/value"), b"good")?;
    verify_loop(&s, &l.id, 1).await?;
    assert!(s.get_loop(&l.id).await?.next_due_at.is_some());
    Ok(())
}

#[tokio::test]
async fn diagnosis_respects_budget_and_does_not_treat_missing_data_as_failure() -> Result<()> {
    let (f, s, l) = setup().await?;
    let mut p = policy();
    p.max_steps_per_cycle = 1;
    s.configure_project("fixture", 1, &p).await?;
    failed_observation(&f, &s, &l).await?;
    assert!(s.pending_diagnoses().await?.is_empty());
    let (f, s, l) = setup().await?;
    failed_observation(&f, &s, &l).await?;
    std::fs::remove_file(f.0.join("project/value"))?;
    verify_loop(&s, &l.id, 1).await?;
    assert!(s.pending_diagnoses().await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn diagnosis_rechecks_authority_and_plan_at_enqueue() -> Result<()> {
    let (f, s, l) = setup().await?;
    failed_observation(&f, &s, &l).await?;
    let candidate = s.pending_diagnoses().await?.remove(0);
    let spec = ExecutionSpec {
        task: "diagnose".into(),
        cwd: f.0.join("project").to_str().unwrap().into(),
        sandbox: Sandbox::ReadOnly,
        timeout_seconds: 30,
        max_output_bytes: 65536,
    };
    let configured = s.verification_plan(&l.id, 1).await?.unwrap();
    let plan: VerificationPlan = serde_json::from_value(configured["plan"].clone())?;
    s.configure_verification(&l.id, 1, 1, &plan).await?;
    assert!(s.enqueue_diagnosis(&candidate, &spec).await.is_err());
    assert!(s.pending_diagnoses().await?.is_empty());
    verify_loop(&s, &l.id, 1).await?;
    let candidate = s.pending_diagnoses().await?.remove(0);
    let mut p = policy();
    p.enabled = false;
    s.configure_project("fixture", 1, &p).await?;
    assert!(s.enqueue_diagnosis(&candidate, &spec).await.is_err());
    assert!(s.pending_diagnoses().await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn queued_diagnosis_is_cancelled_when_verification_plan_changes() -> Result<()> {
    let (f, s, l) = setup().await?;
    failed_observation(&f, &s, &l).await?;
    telos_runtime::daemon::dispatch_diagnoses(&s).await?;
    let configured = s.verification_plan(&l.id, 1).await?.unwrap();
    let plan: VerificationPlan = serde_json::from_value(configured["plan"].clone())?;
    s.configure_verification(&l.id, 1, 1, &plan).await?;
    assert!(s.claim_next("stale", now(), 30).await?.is_none());
    assert!(s
        .events(&l.id)
        .await?
        .iter()
        .any(|e| e["kind"] == "diagnosis_cancelled"));
    Ok(())
}

#[tokio::test]
async fn daemon_automatically_executes_diagnosis_and_publishes_result() -> Result<()> {
    use std::{os::unix::fs::PermissionsExt, time::Duration};
    use telos_runtime::{
        daemon::{self, Request},
        runner::CodexRunner,
    };
    let (f, s, l) = setup().await?;
    failed_observation(&f, &s, &l).await?;
    s.close().await;
    let fake = f.0.join("fake-codex");
    std::fs::write(
        &fake,
        r#"#!/usr/bin/env python3
import json,pathlib,sys
prompt=sys.stdin.read()
assert 'Diagnose the independently observed failure' in prompt
assert 'observed_sha256' in prompt
result={'status':'done','summary':'Fixture contents differ from the accepted hash','evidence_paths':[],'remaining_work':['Restore the expected fixture through an authorized change']}
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
    let mut ready = false;
    for _ in 0..200 {
        if let Ok(page) = daemon::call(
            &state,
            &Request::Events {
                id: l.id.clone(),
                after_sequence: 0,
                through_sequence: None,
                limit: 20,
            },
        )
        .await
        {
            ready = page["items"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["kind"] == "diagnosis_ready");
            if ready {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    tx.send(true)?;
    service.await??;
    assert!(ready, "daemon did not publish diagnosis");
    let s = Store::open(&state).await?;
    assert_eq!(s.get_loop(&l.id).await?.state, LoopState::Active);
    assert!(s.active_cycle(&l.id, 1).await?.is_some());
    assert_eq!(std::fs::read(f.0.join("project/value"))?, b"bad");
    assert!(s.task_context(&l.id, 1).await?["recent_execution"]
        .to_string()
        .contains("Restore the expected fixture"));
    assert!(s.pending_diagnoses().await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn paused_or_uncertain_cycles_cannot_dispatch_diagnosis() -> Result<()> {
    let (f, s, l) = setup().await?;
    failed_observation(&f, &s, &l).await?;
    let candidate = s.pending_diagnoses().await?.remove(0);
    let spec = ExecutionSpec {
        task: "diagnose".into(),
        cwd: f.0.join("project").to_str().unwrap().into(),
        sandbox: Sandbox::ReadOnly,
        timeout_seconds: 30,
        max_output_bytes: 65536,
    };
    let current = s.get_loop(&l.id).await?;
    let paused = s.control(&l.id, current.version, "pause").await?;
    assert!(s.enqueue_diagnosis(&candidate, &spec).await.is_err());
    assert!(s.pending_diagnoses().await?.is_empty());
    s.control(&l.id, paused.version, "resume").await?;
    let candidate = s.pending_diagnoses().await?.remove(0);
    s.enqueue_managed_step(
        &candidate.cycle_id,
        1,
        "other-work",
        &spec,
        now(),
        candidate.policy_revision,
    )
    .await?;
    assert!(s.pending_diagnoses().await?.is_empty());
    let lease = s.claim_next("fixture", now(), 30).await?.unwrap();
    s.finish_step(
        &lease,
        ExecutionOutcome::OutcomeUnknown,
        &serde_json::json!({"reason":"fixture lost result"}),
        now(),
    )
    .await?;
    assert!(s.pending_diagnoses().await?.is_empty());
    assert!(s.enqueue_diagnosis(&candidate, &spec).await.is_err());
    Ok(())
}
