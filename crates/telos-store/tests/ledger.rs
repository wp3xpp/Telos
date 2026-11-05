use anyhow::Result;
use std::path::PathBuf;
use telos_core::*;
use telos_store::Store;
use uuid::Uuid;

struct Fixture {
    root: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("telos-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        Self { root }
    }
    async fn store(&self) -> Result<Store> {
        let store = Store::open(&self.root).await?;
        store.register_project("fixture", &self.root).await?;
        Ok(store)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn contract(kind: LoopKind) -> Contract {
    Contract {
        goal: "Restore the synthetic data feed".into(),
        delivery_scope: "fixture".into(),
        criteria: vec![Criterion {
            id: "freshness".into(),
            description: "Fixture feed meets freshness requirement".into(),
        }],
        interval_seconds: if kind == LoopKind::Continuous {
            Some(300)
        } else {
            None
        },
        max_attempts: 2,
    }
}

#[tokio::test]
async fn project_inspection_is_bounded_read_only_and_rejects_symlink_memory() -> Result<()> {
    let f = Fixture::new();
    let store = f.store().await?;
    assert_eq!(store.list_projects().await?[0]["id"], "fixture");
    assert!(store.inspect_project("unregistered").await.is_err());
    let missing = store.inspect_project("fixture").await?;
    assert_eq!(missing["memory_sources"][0]["status"], "missing");
    std::fs::write(f.root.join("AGENTS.md"), "Build only in fixture")?;
    std::fs::write(f.root.join("MEMORY.md"), vec![b'x'; 65537])?;
    let inspected = store.inspect_project("fixture").await?;
    assert_eq!(
        inspected["memory_sources"][0]["content"],
        "Build only in fixture"
    );
    assert_eq!(inspected["memory_sources"][1]["status"], "too_large");
    assert_eq!(inspected["execution_configured"], false);
    assert_eq!(
        std::fs::read_to_string(f.root.join("AGENTS.md"))?,
        "Build only in fixture"
    );
    std::fs::remove_file(f.root.join("MEMORY.md"))?;
    std::os::unix::fs::symlink(f.root.join("AGENTS.md"), f.root.join("MEMORY.md"))?;
    let linked = store.inspect_project("fixture").await?;
    assert_eq!(linked["memory_sources"][1]["status"], "not_regular_file");
    Ok(())
}

async fn setup(store: &Store, kind: LoopKind) -> Result<(LoopRecord, String)> {
    let l = store
        .create_loop("fixture", "Feed health", kind, &contract(kind))
        .await?;
    let cycle = store
        .start_cycle(&l.id, l.revision, "first-trigger")
        .await?;
    Ok((store.get_loop(&l.id).await?, cycle))
}

async fn proof(store: &Store, cycle: &str) -> Result<Verification> {
    let evidence = store
        .add_evidence(
            cycle,
            1,
            "fixture-artifact-v1",
            "fixture",
            b"freshness=1; sampled=true",
        )
        .await?;
    Ok(Verification {
        cycle_id: cycle.into(),
        contract_revision: 1,
        artifact: "fixture-artifact-v1".into(),
        environment: "fixture".into(),
        observation_complete: true,
        results: vec![CriterionResult {
            criterion_id: "freshness".into(),
            verdict: Verdict::Passed,
            evidence_ids: vec![evidence],
            explanation: "Synthetic verifier sampled fixture data".into(),
        }],
    })
}

#[tokio::test]
async fn evidence_pages_preserve_bytes_and_detect_whole_file_tampering() -> Result<()> {
    let f = Fixture::new();
    let s = f.store().await?;
    let (l, c) = setup(&s, LoopKind::Delivery).await?;
    let content = "证据: fixture result".as_bytes();
    let first = s
        .add_evidence(&c, 1, "artifact", "fixture", content)
        .await?;
    let second = s
        .add_evidence(&c, 1, "artifact", "fixture", b"second")
        .await?;
    let page = s.list_evidence(&l.id, None, 1).await?;
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    let next = page["next_after_id"].as_str().unwrap();
    let last = s.list_evidence(&l.id, Some(next), 1).await?;
    assert_ne!(page["items"][0]["id"], last["items"][0]["id"]);
    assert!(last["next_after_id"].is_null());
    let fragment = s.get_evidence(&first, 0, 1).await?;
    assert!(fragment["text"].is_null());
    assert_eq!(fragment["bytes"][0], content[0]);
    let mut reconstructed = vec![];
    let mut offset = 0;
    loop {
        let fragment = s.get_evidence(&first, offset, 4).await?;
        reconstructed.extend(serde_json::from_value::<Vec<u8>>(
            fragment["bytes"].clone(),
        )?);
        match fragment["next_offset"].as_u64() {
            Some(next) => offset = next as usize,
            None => break,
        }
    }
    assert_eq!(reconstructed, content);
    assert_eq!(s.get_evidence(&second, 0, 100).await?["text"], "second");
    assert!(s.get_evidence(&first, 0, 32769).await.is_err());
    assert!(s.get_evidence(&first, content.len() + 1, 1).await.is_err());
    let mut altered = content.to_vec();
    *altered.last_mut().unwrap() ^= 1;
    std::fs::write(f.root.join(format!("evidence/{first}.bin")), altered)?;
    // Even an unchanged first-byte page must fail if a later byte was modified.
    assert!(s.get_evidence(&first, 0, 1).await.is_err());
    std::fs::remove_file(f.root.join(format!("evidence/{second}.bin")))?;
    assert!(s.get_evidence(&second, 0, 1).await.is_err());
    Ok(())
}

#[tokio::test]
async fn delivery_closes_only_with_valid_evidence_and_survives_restart() -> Result<()> {
    let f = Fixture::new();
    let s = f.store().await?;
    let (l, c) = setup(&s, LoopKind::Delivery).await?;
    let p = proof(&s, &c).await?;
    assert_eq!(s.verify(&p).await?, Verdict::Passed);
    assert_eq!(s.get_loop(&l.id).await?.state, LoopState::Completed);
    assert!(
        s.verify(&p).await.is_err(),
        "terminal cycles cannot be verified again"
    );
    assert_eq!(
        s.events(&l.id).await?.last().unwrap()["kind"],
        "verification_recorded"
    );
    s.close().await;
    let reopened = f.store().await?;
    assert_eq!(reopened.get_loop(&l.id).await?.state, LoopState::Completed);
    reopened.close().await;
    Ok(())
}

#[tokio::test]
async fn continuous_returns_to_observation_and_old_trigger_is_idempotent() -> Result<()> {
    let f = Fixture::new();
    let s = f.store().await?;
    let (l, c) = setup(&s, LoopKind::Continuous).await?;
    let p = proof(&s, &c).await?;
    s.verify(&p).await?;
    let updated = s.get_loop(&l.id).await?;
    assert_eq!(updated.state, LoopState::Active);
    assert!(updated.next_due_at.unwrap() > telos_store::now());
    assert_eq!(s.start_cycle(&l.id, 1, "first-trigger").await?, c);
    assert_ne!(s.start_cycle(&l.id, 1, "next-trigger").await?, c);
    s.close().await;
    Ok(())
}

#[tokio::test]
async fn incomplete_observation_does_not_complete_and_failure_can_be_corrected() -> Result<()> {
    let f = Fixture::new();
    let s = f.store().await?;
    let (l, c) = setup(&s, LoopKind::Delivery).await?;
    let mut p = proof(&s, &c).await?;
    p.observation_complete = false;
    assert_eq!(s.verify(&p).await?, Verdict::Inconclusive);
    assert_eq!(s.get_loop(&l.id).await?.state, LoopState::Active);
    p.observation_complete = true;
    p.results[0].verdict = Verdict::Failed;
    assert_eq!(s.verify(&p).await?, Verdict::Failed);
    p.results[0].verdict = Verdict::Passed;
    assert_eq!(s.verify(&p).await?, Verdict::Passed);
    s.close().await;
    Ok(())
}

#[tokio::test]
async fn revised_contract_rejects_old_result_and_optimistic_conflict() -> Result<()> {
    let f = Fixture::new();
    let s = f.store().await?;
    let (l, c) = setup(&s, LoopKind::Delivery).await?;
    let p = proof(&s, &c).await?;
    let mut newer = l.contract.clone();
    newer.goal = "Changed goal".into();
    let revised = s.update_contract(&l.id, l.version, &newer).await?;
    assert_eq!(revised.revision, 2);
    assert!(s.verify(&p).await.is_err());
    assert!(s.start_cycle(&l.id, 2, "first-trigger").await.is_err());
    assert!(s.update_contract(&l.id, l.version, &newer).await.is_err());
    assert_eq!(s.get_loop(&l.id).await?.revision, 2);
    s.close().await;
    Ok(())
}

#[tokio::test]
async fn pause_blocks_progress_and_terminal_state_cannot_resume() -> Result<()> {
    let f = Fixture::new();
    let s = f.store().await?;
    let (l, c) = setup(&s, LoopKind::Delivery).await?;
    let p = proof(&s, &c).await?;
    let paused = s.control(&l.id, l.version, "pause").await?;
    assert!(s.start_cycle(&l.id, 1, "another").await.is_err());
    assert!(s.verify(&p).await.is_err());
    let resumed = s.control(&l.id, paused.version, "resume").await?;
    let stopped = s.control(&l.id, resumed.version, "stop").await?;
    assert!(s.control(&l.id, stopped.version, "resume").await.is_err());
    s.close().await;
    Ok(())
}

#[tokio::test]
async fn concurrent_triggers_share_one_cycle() -> Result<()> {
    let f = Fixture::new();
    let s = f.store().await?;
    let l = s
        .create_loop(
            "fixture",
            "health",
            LoopKind::Continuous,
            &contract(LoopKind::Continuous),
        )
        .await?;
    let (a, b) = tokio::join!(s.start_cycle(&l.id, 1, "a"), s.start_cycle(&l.id, 1, "b"));
    assert_eq!(a?, b?);
    assert_eq!(
        s.events(&l.id)
            .await?
            .iter()
            .filter(|e| e["kind"] == "cycle_started")
            .count(),
        1
    );
    s.close().await;
    Ok(())
}

#[tokio::test]
async fn fabricated_wrong_binding_and_tampered_evidence_are_rejected() -> Result<()> {
    let f = Fixture::new();
    let s = f.store().await?;
    let (l, c) = setup(&s, LoopKind::Delivery).await?;
    let mut p = proof(&s, &c).await?;
    p.artifact = "different-build".into();
    assert!(s.verify(&p).await.is_err());
    p.artifact = "fixture-artifact-v1".into();
    let saved = p.results[0].evidence_ids[0].clone();
    p.results[0].evidence_ids = vec!["invented".into()];
    assert!(s.verify(&p).await.is_err());
    p.results[0].evidence_ids = vec![saved.clone()];
    std::fs::write(f.root.join(format!("evidence/{saved}.bin")), b"tampered")?;
    assert!(s.verify(&p).await.is_err());
    assert_eq!(s.get_loop(&l.id).await?.state, LoopState::Active);
    assert!(!s
        .events(&l.id)
        .await?
        .iter()
        .any(|e| e["kind"] == "verification_recorded"));
    s.close().await;
    Ok(())
}

#[tokio::test]
async fn missing_or_duplicate_criteria_cannot_pass() -> Result<()> {
    let f = Fixture::new();
    let s = f.store().await?;
    let (_, c) = setup(&s, LoopKind::Delivery).await?;
    let mut p = proof(&s, &c).await?;
    p.results.push(p.results[0].clone());
    assert!(s.verify(&p).await.is_err());
    p.results.clear();
    assert_eq!(s.verify(&p).await?, Verdict::Inconclusive);
    s.close().await;
    Ok(())
}

fn spec(root: &std::path::Path) -> ExecutionSpec {
    ExecutionSpec {
        task: "Inspect fixture only".into(),
        cwd: root.to_str().unwrap().into(),
        sandbox: Sandbox::ReadOnly,
        timeout_seconds: 30,
        max_output_bytes: 65536,
    }
}

#[tokio::test]
async fn continuous_notifications_are_quiet_until_change_and_recovery() -> Result<()> {
    let f = Fixture::new();
    let s = f.store().await?;
    let (l, c) = setup(&s, LoopKind::Continuous).await?;
    s.verify(&proof(&s, &c).await?).await?;
    assert!(s.notification_status().await?.is_empty());
    let c = s.start_cycle(&l.id, 1, "next").await?;
    let mut observation = proof(&s, &c).await?;
    observation.results[0].verdict = Verdict::Failed;
    s.verify(&observation).await?;
    s.verify(&observation).await?;
    let notification = s.claim_notification(1000).await?.unwrap();
    assert_eq!(notification.body["verdict"], "failed");
    assert!(s.claim_notification(1000).await?.is_none());
    s.finish_notification(&notification, None, 1001).await?;
    observation.results[0].verdict = Verdict::Passed;
    s.verify(&observation).await?;
    let recovery = s.claim_notification(1002).await?.unwrap();
    assert_eq!(recovery.body["verdict"], "passed");
    assert_ne!(recovery.event_sequence, notification.event_sequence);
    Ok(())
}

#[tokio::test]
async fn notification_retry_survives_restart_and_rejects_expired_owner() -> Result<()> {
    let f = Fixture::new();
    let s = f.store().await?;
    let (_, c) = setup(&s, LoopKind::Delivery).await?;
    s.verify(&proof(&s, &c).await?).await?;
    let old = s.claim_notification(1000).await?.unwrap();
    s.close().await;
    let s = f.store().await?;
    let current = s.claim_notification(1031).await?.unwrap();
    assert_eq!(current.id, old.id);
    assert_ne!(current.token, old.token);
    assert!(s.finish_notification(&old, None, 1032).await.is_err());
    s.finish_notification(&current, Some("fixture adapter unavailable"), 1032)
        .await?;
    assert!(s.claim_notification(1033).await?.is_none());
    for at in [1100, 1200, 1300] {
        let retry = s.claim_notification(at).await?.unwrap();
        s.finish_notification(&retry, Some("fixture error"), at + 1)
            .await?;
    }
    assert!(s.claim_notification(2000).await?.is_none());
    assert!(s
        .notification_status()
        .await?
        .iter()
        .any(|v| v["state"] == "failed" && v["count"] == 1));
    Ok(())
}

#[tokio::test]
async fn newer_verification_supersedes_unsent_stale_notification() -> Result<()> {
    let f = Fixture::new();
    let s = f.store().await?;
    let (_, c) = setup(&s, LoopKind::Delivery).await?;
    let mut observation = proof(&s, &c).await?;
    observation.results[0].verdict = Verdict::Failed;
    s.verify(&observation).await?;
    observation.results[0].verdict = Verdict::Passed;
    s.verify(&observation).await?;
    let notice = s.claim_notification(1000).await?.unwrap();
    assert_eq!(notice.body["verdict"], "passed");
    assert!(s.claim_notification(1000).await?.is_none());
    assert!(s
        .notification_status()
        .await?
        .iter()
        .any(|v| v["state"] == "superseded"));
    Ok(())
}

fn execution_policy() -> ProjectExecutionPolicy {
    ProjectExecutionPolicy {
        enabled: true,
        sandbox: Sandbox::ReadOnly,
        timeout_seconds: 30,
        max_output_bytes: 65536,
        max_steps_per_cycle: 1,
    }
}

#[tokio::test]
async fn managed_execution_requires_current_policy_and_obeys_bounds() -> Result<()> {
    let f = Fixture::new();
    let s = f.store().await?;
    let (_, c) = setup(&s, LoopKind::Delivery).await?;
    let task = spec(&f.root);
    assert!(s
        .enqueue_managed_step(&c, 1, "one", &task, 0, 1)
        .await
        .is_err());
    let policy = execution_policy();
    s.configure_project("fixture", 0, &policy).await?;
    assert!(s.configure_project("fixture", 0, &policy).await.is_err());
    assert!(s
        .enqueue_managed_step(&c, 1, "one", &task, 0, 2)
        .await
        .is_err());
    for changed in [
        ExecutionSpec {
            sandbox: Sandbox::WorkspaceWrite,
            ..task.clone()
        },
        ExecutionSpec {
            timeout_seconds: 31,
            ..task.clone()
        },
        ExecutionSpec {
            max_output_bytes: 65537,
            ..task.clone()
        },
    ] {
        assert!(s
            .enqueue_managed_step(&c, 1, "one", &changed, 0, 1)
            .await
            .is_err());
    }
    let step = s.enqueue_managed_step(&c, 1, "one", &task, 0, 1).await?;
    assert_eq!(
        s.enqueue_managed_step(&c, 1, "one", &task, 0, 1).await?,
        step
    );
    assert!(s
        .enqueue_managed_step(&c, 1, "two", &task, 0, 1)
        .await
        .is_err());
    let lease = s.claim_next("worker", 100, 30).await?.unwrap();
    assert!(s.configure_project("fixture", 1, &policy).await.is_err());
    s.finish_step(
        &lease,
        ExecutionOutcome::Succeeded,
        &serde_json::json!({}),
        101,
    )
    .await?;
    s.configure_project("fixture", 1, &policy).await?;
    s.close().await;
    let reopened = f.store().await?;
    assert_eq!(
        reopened.project_policy("fixture").await?.unwrap()["revision"],
        2
    );
    Ok(())
}

#[tokio::test]
async fn policy_revocation_cancels_queued_work_without_dispatch() -> Result<()> {
    let f = Fixture::new();
    let s = f.store().await?;
    let (l, c) = setup(&s, LoopKind::Delivery).await?;
    let mut policy = execution_policy();
    s.configure_project("fixture", 0, &policy).await?;
    let step = s
        .enqueue_managed_step(&c, 1, "one", &spec(&f.root), 0, 1)
        .await?;
    policy.enabled = false;
    s.configure_project("fixture", 1, &policy).await?;
    assert_eq!(s.step_status(&step).await?["state"], "cancelled");
    assert_eq!(s.get_loop(&l.id).await?.state, LoopState::Blocked);
    assert!(s.claim_next("worker", 100, 30).await?.is_none());
    assert!(s
        .events(&l.id)
        .await?
        .iter()
        .any(|e| e["kind"] == "step_cancelled_policy_changed"));
    Ok(())
}

#[tokio::test]
async fn queue_is_durable_idempotent_and_single_owner() -> Result<()> {
    let f = Fixture::new();
    let s = f.store().await?;
    let (l, c) = setup(&s, LoopKind::Delivery).await?;
    let task = spec(&f.root);
    let step = s.enqueue_step(&c, 1, "first", &task, 100).await?;
    assert_eq!(s.enqueue_step(&c, 1, "first", &task, 100).await?, step);
    let mut different = task.clone();
    different.task = "different request".into();
    assert!(s
        .enqueue_step(&c, 1, "first", &different, 100)
        .await
        .is_err());
    s.close().await;
    let s = f.store().await?;
    assert!(s.claim_next("worker", 99, 30).await?.is_none());
    let (a, b) = tokio::join!(
        s.claim_next("worker-a", 100, 30),
        s.claim_next("worker-b", 100, 30)
    );
    let a = a?;
    let b = b?;
    assert_ne!(a.is_some(), b.is_some());
    let lease = a.or(b).unwrap();
    let mut forged = lease.clone();
    forged.generation += 1;
    assert!(s
        .finish_step(
            &forged,
            ExecutionOutcome::Succeeded,
            &serde_json::json!({}),
            101
        )
        .await
        .is_err());
    s.finish_step(
        &lease,
        ExecutionOutcome::Succeeded,
        &serde_json::json!({"summary":"fixture inspected"}),
        101,
    )
    .await?;
    assert_eq!(s.step_status(&step).await?["state"], "succeeded");
    assert_eq!(
        s.get_loop(&l.id).await?.state,
        LoopState::Active,
        "agent completion is not goal completion"
    );
    assert!(s
        .finish_step(
            &lease,
            ExecutionOutcome::Succeeded,
            &serde_json::json!({}),
            102
        )
        .await
        .is_err());
    s.close().await;
    Ok(())
}

#[tokio::test]
async fn expiry_blocks_instead_of_blind_retry_and_late_result_is_rejected() -> Result<()> {
    let f = Fixture::new();
    let s = f.store().await?;
    let (l, c) = setup(&s, LoopKind::Delivery).await?;
    let step = s.enqueue_step(&c, 1, "first", &spec(&f.root), 100).await?;
    let lease = s.claim_next("worker", 100, 10).await?.unwrap();
    s.heartbeat(&lease, 105, 10).await?;
    assert_eq!(s.reconcile_expired(111).await?, 0);
    assert_eq!(s.reconcile_expired(115).await?, 1);
    assert_eq!(s.reconcile_expired(116).await?, 0);
    assert_eq!(s.step_status(&step).await?["state"], "outcome_unknown");
    let blocked = s.get_loop(&l.id).await?;
    assert_eq!(blocked.state, LoopState::Blocked);
    assert!(s.control(&l.id, blocked.version, "resume").await.is_err());
    assert!(s.claim_next("replacement", 116, 30).await?.is_none());
    assert!(s
        .finish_step(
            &lease,
            ExecutionOutcome::Succeeded,
            &serde_json::json!({}),
            116
        )
        .await
        .is_err());
    s.close().await;
    Ok(())
}

#[tokio::test]
async fn paused_queue_does_not_dispatch_and_running_step_prevents_early_close() -> Result<()> {
    let f = Fixture::new();
    let s = f.store().await?;
    let (l, c) = setup(&s, LoopKind::Delivery).await?;
    s.enqueue_step(&c, 1, "first", &spec(&f.root), 100).await?;
    let p = s.control(&l.id, l.version, "pause").await?;
    assert!(s.claim_next("worker", 100, 30).await?.is_none());
    s.control(&l.id, p.version, "resume").await?;
    let lease = s.claim_next("worker", 100, 30).await?.unwrap();
    let evidence = proof(&s, &c).await?;
    assert!(s.verify(&evidence).await.is_err());
    let current = s.get_loop(&l.id).await?;
    assert!(s
        .update_contract(&l.id, current.version, &current.contract)
        .await
        .is_err());
    s.finish_step(
        &lease,
        ExecutionOutcome::Succeeded,
        &serde_json::json!({}),
        101,
    )
    .await?;
    assert_eq!(s.verify(&evidence).await?, Verdict::Passed);
    s.close().await;
    Ok(())
}

#[tokio::test]
async fn execution_cannot_escape_registered_project() -> Result<()> {
    let f = Fixture::new();
    let s = f.store().await?;
    let (_, c) = setup(&s, LoopKind::Delivery).await?;
    let mut task = spec(&f.root);
    task.cwd = "/tmp".into();
    assert!(s.enqueue_step(&c, 1, "escape", &task, 100).await.is_err());
    s.close().await;
    Ok(())
}

#[tokio::test]
async fn audit_pages_preserve_watermark_and_large_unicode_bodies() -> Result<()> {
    let f = Fixture::new();
    let s = f.store().await?;
    let c = contract(LoopKind::Delivery);
    let l = s
        .create_loop("fixture", "audit", LoopKind::Delivery, &c)
        .await?;
    for i in 0..25 {
        s.record_answer(
            &l.id,
            1,
            s.get_loop(&l.id).await?.version,
            &format!("answer-{i}"),
            &if i == 0 {
                "观".repeat(2730)
            } else {
                "fact".into()
            },
        )
        .await?;
    }
    let first = s.event_page(&l.id, 0, None, 3).await?;
    let upper = first["through_sequence"].as_i64().unwrap();
    s.record_answer(&l.id, 1, s.get_loop(&l.id).await?.version, "later", "fact")
        .await?;
    let mut page = first;
    let mut sequences = vec![];
    let mut omitted = None;
    loop {
        assert!(serde_json::to_vec(&page)?.len() < 512 * 1024);
        for item in page["items"].as_array().unwrap() {
            let seq = item["sequence"].as_i64().unwrap();
            sequences.push(seq);
            if item["body_omitted"] == true {
                omitted = Some(seq);
            }
        }
        let Some(after) = page["next_after_sequence"].as_i64() else {
            break;
        };
        page = s.event_page(&l.id, after, Some(upper), 3).await?;
    }
    let expected = s.events(&l.id).await?;
    assert_eq!(
        sequences,
        expected
            .iter()
            .filter_map(|e| e["sequence"].as_i64().filter(|s| *s <= upper))
            .collect::<Vec<_>>()
    );
    let seq = omitted.expect("large answer must be omitted from list");
    let mut bytes = vec![];
    let mut offset = 0;
    loop {
        let part = s.event_body(&l.id, seq, offset, 127).await?;
        bytes.extend(
            part["bytes"]
                .as_array()
                .unwrap()
                .iter()
                .map(|b| b.as_u64().unwrap() as u8),
        );
        let Some(next) = part["next_offset"].as_u64() else {
            break;
        };
        offset = next as usize;
    }
    let original = expected.iter().find(|e| e["sequence"] == seq).unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&bytes)?,
        original["body"]
    );
    assert!(s.event_page(&l.id, -1, None, 3).await.is_err());
    assert!(s.event_page(&l.id, 0, None, 21).await.is_err());
    assert!(s.event_body("other", seq, 0, 127).await.is_err());
    assert!(s
        .event_body(&l.id, seq, bytes.len() + 1, 127)
        .await
        .is_err());
    Ok(())
}

fn lease_plan() -> VerificationPlan {
    VerificationPlan {
        test_checks: vec![],
        checks: vec![FileHashCheck {
            criterion_id: "freshness".into(),
            relative_path: "result.txt".into(),
            expected_sha256: "a".repeat(64),
        }],
        log_checks: vec![],
    }
}

#[tokio::test]
async fn verification_ownership_blocks_project_work_and_stale_acceptance() -> Result<()> {
    let f = Fixture::new();
    let s = f.store().await?;
    let (l, cycle) = setup(&s, LoopKind::Delivery).await?;
    s.configure_verification(&l.id, 1, 0, &lease_plan()).await?;
    let (other, other_cycle) = setup(&s, LoopKind::Delivery).await?;
    s.configure_verification(&other.id, 1, 0, &lease_plan())
        .await?;
    assert!(s.acquire_verification(&l.id, 1, 2).await.is_err());
    let (a, b) = tokio::join!(
        s.acquire_verification(&l.id, 1, 1),
        s.acquire_verification(&l.id, 1, 1)
    );
    assert_ne!(a.is_ok(), b.is_ok());
    let lease = a.or(b)?;
    assert!(s.acquire_verification(&other.id, 1, 1).await.is_err());
    assert!(s
        .enqueue_step(&other_cycle, 1, "blocked", &spec(&f.root), 0)
        .await
        .is_err());
    assert!(s
        .configure_verification(&l.id, 1, 1, &lease_plan())
        .await
        .is_err());
    assert!(s
        .configure_project("fixture", 0, &execution_policy())
        .await
        .is_err());
    assert!(s
        .update_contract(&l.id, l.version, &l.contract)
        .await
        .is_err());
    let p = proof(&s, &cycle).await?;
    assert!(s.verify_configured(&p, 1).await.is_err());
    assert!(s.verify_leased(&p, 1, false, "not-owner").await.is_err());
    assert!(s
        .verify_leased(&proof(&s, &other_cycle).await?, 1, false, &lease)
        .await
        .is_err());
    assert_eq!(
        s.verify_leased(&p, 1, false, &lease).await?,
        Verdict::Passed
    );
    s.finish_verification(&lease).await?;
    assert!(s.finish_verification(&lease).await.is_err());
    s.enqueue_step(&other_cycle, 1, "allowed", &spec(&f.root), 0)
        .await?;
    assert!(s.acquire_verification(&other.id, 1, 1).await.is_err());
    let events = s.events(&l.id).await?;
    assert!(events
        .iter()
        .any(|e| e["kind"] == "verification_lease_acquired"));
    assert!(events
        .iter()
        .any(|e| e["kind"] == "verification_lease_finished"));
    s.close().await;
    Ok(())
}

#[tokio::test]
async fn interrupted_verification_remains_unknown_across_restart() -> Result<()> {
    let f = Fixture::new();
    let s = f.store().await?;
    let (l, c) = setup(&s, LoopKind::Delivery).await?;
    s.configure_verification(&l.id, 1, 0, &lease_plan()).await?;
    let lease = s.acquire_verification(&l.id, 1, 1).await?;
    s.close().await;
    let s = f.store().await?;
    assert_eq!(s.reconcile_verification_leases().await?, 1);
    assert_eq!(s.reconcile_verification_leases().await?, 0);
    assert!(s.finish_verification(&lease).await.is_err());
    assert!(s.acquire_verification(&l.id, 1, 1).await.is_err());
    assert!(s
        .verify_leased(&proof(&s, &c).await?, 1, false, &lease)
        .await
        .is_err());
    assert!(s
        .enqueue_step(&c, 1, "retry", &spec(&f.root), 0)
        .await
        .is_err());
    let paused = s.control(&l.id, l.version, "pause").await?;
    assert!(s.control(&l.id, paused.version, "resume").await.is_err());
    s.close().await;
    let s = f.store().await?;
    assert_eq!(s.reconcile_verification_leases().await?, 0);
    assert!(s.finish_verification(&lease).await.is_err());
    let events = s.events(&l.id).await?;
    assert_eq!(
        events
            .iter()
            .filter(|e| e["kind"] == "verification_outcome_unknown")
            .count(),
        1
    );
    s.close().await;
    Ok(())
}

#[tokio::test]
async fn workspace_preparation_and_verification_exclude_each_other() -> Result<()> {
    let f = Fixture::new();
    let s = f.store().await?;
    let (l, c) = setup(&s, LoopKind::Delivery).await?;
    s.configure_project(
        "fixture",
        0,
        &ProjectExecutionPolicy {
            sandbox: Sandbox::WorkspaceWrite,
            ..execution_policy()
        },
    )
    .await?;
    s.configure_verification(&l.id, 1, 0, &lease_plan()).await?;
    let lease = s.acquire_verification(&l.id, 1, 1).await?;
    let git = f.root.join("fake-git");
    let git = git.to_str().unwrap();
    assert!(s
        .reserve_workspace(&c, 1, 1, git, &"a".repeat(40))
        .await
        .is_err());
    s.finish_verification(&lease).await?;
    let (workspace, _) = s.reserve_workspace(&c, 1, 1, git, &"a".repeat(40)).await?;
    assert!(s.acquire_verification(&l.id, 1, 1).await.is_err());
    // Store-level fixture: runtime remains responsible for actual Git validation.
    s.finish_workspace(&workspace, None).await?;
    let lease = s.acquire_verification(&l.id, 1, 1).await?;
    assert!(s
        .finish_workspace(&workspace, Some("concurrent mutation"))
        .await
        .is_err());
    s.finish_verification(&lease).await?;
    s.close().await;
    Ok(())
}
