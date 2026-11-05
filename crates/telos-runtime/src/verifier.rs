//! Independent deterministic file observations. Expected values are operator-configured.
use crate::verification_adapters::TestVerificationAdapter;
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{io::Read, path::Path};
use telos_core::{
    CriterionResult, FileHashCheck, LoopState, Verdict, Verification, VerificationPlan,
};
use telos_store::Store;

fn observe(root: &Path, check: &FileHashCheck) -> Result<(Verdict, Value)> {
    let candidate = root.join(&check.relative_path);
    let metadata = match std::fs::symlink_metadata(&candidate) {
        Ok(value) => value,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok((Verdict::Inconclusive, json!({"reason":"artifact missing"})))
        }
        Err(e) => return Err(e.into()),
    };
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "artifact must be a regular file"
    );
    let path = candidate.canonicalize()?;
    ensure!(
        path.starts_with(root),
        "artifact outside registered project"
    );
    let file = std::fs::File::open(path)?;
    let before = file.metadata()?;
    ensure!(before.is_file(), "artifact must be a regular file");
    let mut bytes = Vec::new();
    (&file).take(8 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= 8 * 1024 * 1024,
        "artifact exceeds 8MiB observation limit"
    );
    let after = file.metadata()?;
    if before.len() != after.len()
        || before.modified()? != after.modified()?
        || after.len() != bytes.len() as u64
    {
        return Ok((
            Verdict::Inconclusive,
            json!({"reason":"artifact changed while observing"}),
        ));
    }
    let hash = format!("{:x}", Sha256::digest(&bytes));
    let verdict = if hash == check.expected_sha256 {
        Verdict::Passed
    } else {
        Verdict::Failed
    };
    Ok((
        verdict,
        json!({"observed_sha256":hash,"byte_length":bytes.len(),"reason":"compared independently observed bytes against configured expected hash"}),
    ))
}

pub async fn verify_loop(store: &Store, loop_id: &str, revision: i64) -> Result<Value> {
    verify_loop_at(store, loop_id, revision, telos_store::now()).await
}

pub async fn verify_loop_at(store: &Store, loop_id: &str, revision: i64, at: i64) -> Result<Value> {
    let (_tx, cancel) = tokio::sync::watch::channel(false);
    verify_with_runtime(store, loop_id, revision, at, Path::new("codex"), cancel).await
}

pub async fn verify_with_runtime(
    store: &Store,
    loop_id: &str,
    revision: i64,
    at: i64,
    codex: &Path,
    cancel: tokio::sync::watch::Receiver<bool>,
) -> Result<Value> {
    let config = store
        .verification_plan(loop_id, revision)
        .await?
        .context("independent verification plan not configured")?;
    let version = config["version"].as_i64().context("plan version missing")?;
    let configured: VerificationPlan = serde_json::from_value(config["plan"].clone())?;
    crate::verification_adapters::validate_plan(&configured)?;
    let lease = store
        .acquire_verification(loop_id, revision, version)
        .await?;
    let mut uncertain = false;
    let result = verify_owned(
        store,
        loop_id,
        revision,
        at,
        &lease,
        codex,
        cancel,
        &mut uncertain,
    )
    .await;
    if uncertain {
        store
            .mark_verification_unknown(&lease, "test execution returned without confirmed cleanup")
            .await?;
    } else {
        store.finish_verification(&lease).await?;
    }
    result
}

#[allow(clippy::too_many_arguments)]
async fn verify_owned(
    store: &Store,
    loop_id: &str,
    revision: i64,
    at: i64,
    lease: &str,
    codex: &Path,
    mut cancel: tokio::sync::watch::Receiver<bool>,
    uncertain: &mut bool,
) -> Result<Value> {
    let record = store.get_loop(loop_id).await?;
    ensure!(
        record.revision == revision && record.state == LoopState::Active,
        "stale or inactive goal"
    );
    let workspace = store.workspace_for_loop(loop_id, revision).await?;
    let config = store
        .verification_plan(loop_id, revision)
        .await?
        .context("independent verification plan not configured")?;
    let plan_version = config["version"].as_i64().context("plan version missing")?;
    let plan: VerificationPlan = serde_json::from_value(config["plan"].clone())?;
    plan.validate(&record.contract)?;
    let cycle = store
        .active_cycle(loop_id, revision)
        .await?
        .context("no active cycle to verify")?;
    let project = store.inspect_project(&record.project_id).await?;
    let root = if let Some(workspace) = &workspace {
        ensure!(
            workspace.state == "ready" && workspace.cycle_id == cycle,
            "delivery workspace not ready for this cycle"
        );
        ensure!(
            plan.log_checks.is_empty(),
            "delivery log-window acceptance requires a dedicated environment adapter"
        );
        std::path::PathBuf::from(&workspace.path)
    } else {
        std::path::PathBuf::from(project["root"].as_str().context("project path missing")?)
    };
    let acceptance_paths: Vec<String> = plan
        .checks
        .iter()
        .map(|c| c.relative_path.clone())
        .collect();
    let before = if let Some(workspace) = &workspace {
        Some(crate::workspace::snapshot(workspace, &acceptance_paths).await?)
    } else {
        None
    };
    let test_checks = plan.test_checks.clone();
    ensure!(
        test_checks.is_empty()
            || workspace.is_some()
            || (record.kind == telos_core::LoopKind::Continuous
                && test_checks
                    .iter()
                    .all(|c| c.adapter == telos_core::observation::ADAPTER)),
        "test acceptance requires an owned delivery workspace"
    );
    let log_checks = plan.log_checks.clone();
    let file_root = root.clone();
    let mut observations = tokio::task::spawn_blocking(move || {
        plan.checks
            .iter()
            .map(|check| {
                let (verdict, detail) = match observe(&file_root, check) {
                    Ok(value) => value,
                    Err(e) => (Verdict::Error, json!({"reason":e.to_string()})),
                };
                (
                    check.criterion_id.clone(),
                    verdict,
                    json!({"check":check,"detail":detail,"verdict":verdict}),
                )
            })
            .collect::<Vec<_>>()
    })
    .await?;
    for check in log_checks {
        let saved = store
            .log_cursor(&cycle, &check.criterion_id, plan_version)
            .await?;
        let cursor_version = saved
            .as_ref()
            .and_then(|v| v["version"].as_i64())
            .unwrap_or(0);
        let previous = saved
            .as_ref()
            .map(|v| {
                serde_json::from_value::<crate::log_window::Cursor>(v["body"]["cursor"].clone())
            })
            .transpose()?;
        let sampled = tokio::task::spawn_blocking({
            let root = root.clone();
            let check = check.clone();
            let previous = previous.clone();
            move || crate::log_window::sample(&root, &check, previous, at)
        })
        .await?;
        let (cursor, verdict, detail) = match sampled {
            Ok(sample) => sample,
            Err(error) => {
                let mut cursor = previous.unwrap_or_else(|| crate::log_window::Cursor::fresh(at));
                cursor.reset_window(at);
                cursor.last_poll = at;
                (cursor, Verdict::Error, json!({"reason":error.to_string()}))
            }
        };
        store
            .save_log_cursor(
                &cycle,
                &check.criterion_id,
                plan_version,
                cursor_version,
                &json!({"cursor":cursor,"verdict":verdict,"detail":detail}),
            )
            .await?;
        observations.push((
            check.criterion_id.clone(),
            verdict,
            json!({"check":check,"detail":detail,"verdict":verdict}),
        ));
    }
    let mut test_outputs = Vec::new();
    if !test_checks.is_empty() {
        ensure!(!*cancel.borrow(), "verification cancelled before tests");
        store.reserve_test_attempt(lease).await?;
        let parent = store.verification_run_parent();
        tokio::fs::create_dir_all(&parent).await?;
        for (index, check) in test_checks.iter().enumerate() {
            ensure!(!*cancel.borrow(), "verification cancelled");
            let current = store.get_loop(loop_id).await?;
            ensure!(
                current.state == LoopState::Active && current.revision == revision,
                "goal changed before test launch"
            );
            let gate = if let Some(stage) = check
                .observation
                .as_ref()
                .and_then(|p| p.after_stage.as_ref())
            {
                store
                    .observation_stage_gate(loop_id, revision, stage)
                    .await?
            } else {
                json!({"required":false})
            };
            if gate["required"] == true && gate["satisfied"] != true {
                observations.push((check.criterion_id.clone(), Verdict::Inconclusive,
                    json!({"check":check,"verdict":"inconclusive","detail":{"reason":"associated delivery awaits project stage evidence","expected_wait":true,"gate":gate}})));
                continue;
            }
            let run_dir = parent.join(format!("{lease}-{index}"));
            let adapter = crate::verification_adapters::resolve(&check.adapter)?;
            let (stop, stopped) = tokio::sync::watch::channel(false);
            let binding =
                check
                    .observation
                    .as_ref()
                    .map(|policy| telos_core::observation::Binding {
                        loop_id: loop_id.into(),
                        revision,
                        cycle_id: cycle.clone(),
                        request_id: lease.into(),
                        criterion_id: check.criterion_id.clone(),
                        source_id: policy.source_id.clone(),
                        window_start: at.saturating_sub(policy.window_seconds),
                        window_end: at,
                        not_before: gate["not_before"].as_i64(),
                        subject_id: gate["subject_id"].as_str().map(str::to_owned),
                    });
            let execute = adapter.verify(check, &root, &run_dir, codex, stopped, binding.as_ref());
            tokio::pin!(execute);
            let mut tick = tokio::time::interval(std::time::Duration::from_millis(200));
            *uncertain = true;
            let mut report = loop {
                tokio::select! {
                    result = &mut execute => break result?,
                    changed = cancel.changed() => {
                        if changed.is_err() || *cancel.borrow() { let _ = stop.send(true); }
                    },
                    _ = tick.tick() => {
                        match store.get_loop(loop_id).await {
                            Ok(current) if current.state == LoopState::Active && current.revision == revision => {},
                            _ => { let _ = stop.send(true); }
                        }
                    }
                }
            };
            *uncertain = !report.cleanup_confirmed;
            ensure!(
                !*uncertain,
                "test cleanup uncertain; verification ownership retained"
            );
            if let Some(policy) = &check.observation {
                let collection = store
                    .observation_collection_gate(
                        loop_id,
                        revision,
                        &policy.source_id,
                        at.saturating_sub(policy.window_seconds),
                    )
                    .await?;
                if collection["satisfied"] != true && report.verdict == Verdict::Passed {
                    report.verdict = Verdict::Inconclusive;
                    report.detail["reason"] =
                        json!("registered collection has backlog or an audited gap in this window");
                }
                report.detail["collection_gate"] = collection;
                report.detail["stage_gate"] = gate;
            }
            observations.push((
                check.criterion_id.clone(),
                report.verdict,
                json!({"check":check,"verdict":report.verdict,"detail":report.detail}),
            ));
            test_outputs.extend(report.streams);
        }
    }
    let observation_complete = observations
        .iter()
        .all(|(_, v, _)| matches!(v, Verdict::Passed | Verdict::Failed));
    let quiet_pending = observations.iter().all(|(_, v, detail)| {
        *v == Verdict::Passed
            || (*v == Verdict::Inconclusive && detail["detail"]["expected_wait"] == true)
    });
    let review = if let Some(workspace) = &workspace {
        Some(
            crate::workspace::review_patch(
                workspace,
                before.as_ref().context("delivery snapshot missing")?,
            )
            .await?,
        )
    } else {
        None
    };
    if let Some(workspace) = &workspace {
        let after = crate::workspace::snapshot(workspace, &acceptance_paths).await?;
        ensure!(
            before.as_ref() == Some(&after),
            "delivery artifact changed during verification"
        );
    }
    let body = json!({"adapter":if workspace.is_some(){"independent_delivery_observation"}else{"independent_project_observation"},"loop_id":loop_id,"contract_revision":revision,
        "plan_version":plan_version,"observed_at":at,"delivery_snapshot":before,"observations":observations.iter().map(|(_,_,v)|v).collect::<Vec<_>>()});
    let bytes = serde_json::to_vec(&body)?;
    let (artifact, environment) = if let Some(workspace) = &workspace {
        (
            store
                .record_delivery_snapshot(
                    workspace,
                    plan_version,
                    before.as_ref().context("delivery snapshot missing")?,
                )
                .await?,
            format!("local-workspace:{}", workspace.id),
        )
    } else {
        (
            format!("project-observations:{:x}", Sha256::digest(&bytes)),
            format!("local-project:{}", record.project_id),
        )
    };
    let mut test_evidence = Vec::new();
    for stream in &test_outputs {
        let bytes = tokio::fs::read(&stream.path).await?;
        ensure!(
            format!("{:x}", Sha256::digest(&bytes)) == stream.sha256,
            "test output changed before evidence capture"
        );
        if bytes.is_empty() {
            continue;
        }
        test_evidence.push(
            store
                .add_evidence(&cycle, revision, &artifact, &environment, &bytes)
                .await?,
        );
    }
    let review_evidence = if let (Some(workspace), Some(review)) = (&workspace, &review) {
        Some(
            store
                .record_delivery_review(workspace, &artifact, &serde_json::to_vec(review)?)
                .await?,
        )
    } else {
        None
    };
    let evidence = store
        .add_evidence(&cycle, revision, &artifact, &environment, &bytes)
        .await?;
    let results = observations
        .into_iter()
        .map(|(criterion_id, verdict, observation)| CriterionResult {
            criterion_id,
            verdict,
            evidence_ids: std::iter::once(evidence.clone())
                .chain(test_evidence.iter().cloned())
                .collect(),
            explanation: observation["detail"]["reason"]
                .as_str()
                .unwrap_or("observation recorded")
                .into(),
        })
        .collect();
    let verdict = store
        .verify_leased(
            &Verification {
                cycle_id: cycle,
                contract_revision: revision,
                artifact,
                environment,
                observation_complete,
                results,
            },
            plan_version,
            quiet_pending,
            lease,
        )
        .await?;
    Ok(
        json!({"verdict":verdict,"evidence_id":evidence,"review_evidence_id":review_evidence,"plan_version":plan_version,"loop":store.get_loop(loop_id).await?}),
    )
}
