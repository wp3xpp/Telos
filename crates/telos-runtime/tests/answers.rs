use anyhow::Result;
use serde_json::{json, Value};
use std::{os::unix::fs::PermissionsExt, path::PathBuf, time::Duration};
use telos_core::*;
use telos_runtime::{
    daemon::{self, Request},
    runner::CodexRunner,
};
use telos_store::{now, Store};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!("telos-answers-{}", uuid::Uuid::new_v4())))
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn contract() -> Contract {
    Contract {
        goal: "Inspect with supplied context".into(),
        delivery_scope: "fixture".into(),
        criteria: vec![Criterion {
            id: "inspect".into(),
            description: "inspect fixture".into(),
        }],
        interval_seconds: None,
        max_attempts: 2,
    }
}

#[tokio::test]
async fn answers_are_idempotent_revision_bound_and_do_not_resolve_uncertainty() -> Result<()> {
    let f = Fixture::new();
    let s = Store::open(&f.0).await?;
    s.register_project("fixture", &f.0).await?;
    let l = s
        .create_loop("fixture", "fixture", LoopKind::Delivery, &contract())
        .await?;
    let answer = s
        .record_answer(&l.id, 1, l.version, "one", "Provided fact")
        .await?;
    assert_eq!(
        s.record_answer(&l.id, 1, l.version, "one", "Provided fact")
            .await?["answer_id"],
        answer["answer_id"]
    );
    assert!(s
        .record_answer(&l.id, 1, l.version, "one", "Different fact")
        .await
        .is_err());
    assert!(s
        .record_answer(&l.id, 1, l.version, "two", "Next fact")
        .await
        .is_err());
    let cycle = s.start_cycle(&l.id, 1, "fixture").await?;
    s.enqueue_step(
        &cycle,
        1,
        "fixture",
        &ExecutionSpec {
            task: "fixture".into(),
            cwd: f.0.to_string_lossy().into(),
            sandbox: Sandbox::ReadOnly,
            timeout_seconds: 10,
            max_output_bytes: 65536,
        },
        now(),
    )
    .await?;
    let lease = s.claim_next("fixture", now(), 30).await?.unwrap();
    s.finish_step(
        &lease,
        ExecutionOutcome::OutcomeUnknown,
        &json!({"reason":"interrupted"}),
        now(),
    )
    .await?;
    let blocked = s.get_loop(&l.id).await?;
    let result = s
        .record_answer(
            &l.id,
            1,
            blocked.version,
            "two",
            "User supplied more context",
        )
        .await?;
    assert_eq!(result["state"], "blocked");
    assert!(s
        .control(&l.id, result["version"].as_i64().unwrap(), "resume")
        .await
        .is_err());
    assert_eq!(
        s.task_context(&l.id, 1).await?["answers"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    s.close().await;
    let s = Store::open(&f.0).await?;
    assert_eq!(
        s.task_context(&l.id, 1).await?["recent_execution"][0]["summary"],
        "interrupted"
    );
    Ok(())
}

#[tokio::test]
async fn revised_contract_does_not_inherit_old_answers() -> Result<()> {
    let f = Fixture::new();
    let s = Store::open(&f.0).await?;
    s.register_project("fixture", &f.0).await?;
    let l = s
        .create_loop("fixture", "fixture", LoopKind::Delivery, &contract())
        .await?;
    let a = s
        .record_answer(&l.id, 1, l.version, "one", "Old fact")
        .await?;
    s.update_contract(&l.id, a["version"].as_i64().unwrap(), &contract())
        .await?;
    assert!(s.task_context(&l.id, 1).await.is_err());
    assert_eq!(s.task_context(&l.id, 2).await?["answers"], json!([]));
    assert!(s
        .record_answer(&l.id, 1, 1, "stale", "Old answer")
        .await
        .is_err());
    Ok(())
}

#[tokio::test]
async fn daemon_resumes_with_user_answer_and_previous_blocker_in_task_context() -> Result<()> {
    let f = Fixture::new();
    std::fs::create_dir_all(f.0.join("project"))?;
    let fake = f.0.join("fake-codex");
    std::fs::write(
        &fake,
        r#"#!/usr/bin/env python3
import json,pathlib,sys
prompt=sys.stdin.read()
ready='correction-token' in prompt and 'Please supply the fixture selector' in prompt
result={'status':'done' if ready else 'blocked','summary':'Used answer and history' if ready else 'Missing fixture selector','evidence_paths':[],'remaining_work':[] if ready else ['Please supply the fixture selector']}
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
    for _ in 0..100 {
        if daemon::call(&state, &Request::Ping).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    daemon::call(
        &state,
        &Request::RegisterProject {
            id: "fixture".into(),
            root: f.0.join("project"),
        },
    )
    .await?;
    daemon::call(
        &state,
        &Request::ConfigureProject {
            id: "fixture".into(),
            expected_revision: 0,
            policy: ProjectExecutionPolicy {
                enabled: true,
                sandbox: Sandbox::ReadOnly,
                timeout_seconds: 10,
                max_output_bytes: 65536,
                max_steps_per_cycle: 2,
            },
        },
    )
    .await?;
    let l = daemon::call(
        &state,
        &Request::CreateLoop {
            project_id: "fixture".into(),
            name: "fixture".into(),
            kind: LoopKind::Delivery,
            contract: contract(),
        },
    )
    .await?;
    let id = l["id"].as_str().unwrap().to_owned();
    let first = daemon::call(
        &state,
        &Request::StartLoop {
            id: id.clone(),
            revision: 1,
            request_key: "first".into(),
        },
    )
    .await?;
    async fn wait_step(state: &std::path::Path, id: &str) -> Result<Value> {
        for _ in 0..200 {
            let v = daemon::call(state, &Request::StepStatus { id: id.into() }).await?;
            if v["state"] != "ready" && v["state"] != "running" {
                return Ok(v);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        anyhow::bail!("fixture step timeout")
    }
    assert_eq!(
        wait_step(&state, first["step_id"].as_str().unwrap()).await?["state"],
        "failed"
    );
    let blocked = daemon::call(&state, &Request::GetLoop { id: id.clone() }).await?;
    let a = daemon::call(
        &state,
        &Request::AnswerLoop {
            id: id.clone(),
            revision: 1,
            expected_version: blocked["version"].as_i64().unwrap(),
            request_key: "answer".into(),
            text: "Use correction-token".into(),
        },
    )
    .await?;
    assert_eq!(a["state"], "blocked");
    let replay = daemon::call(
        &state,
        &Request::StartLoop {
            id: id.clone(),
            revision: 1,
            request_key: "first".into(),
        },
    )
    .await?;
    assert_eq!(replay["step_id"], first["step_id"]);
    daemon::call(
        &state,
        &Request::Control {
            id: id.clone(),
            expected_version: a["version"].as_i64().unwrap(),
            action: "resume".into(),
        },
    )
    .await?;
    let second = daemon::call(
        &state,
        &Request::StartLoop {
            id: id.clone(),
            revision: 1,
            request_key: "with-answer".into(),
        },
    )
    .await?;
    let done = wait_step(&state, second["step_id"].as_str().unwrap()).await?;
    assert_eq!(done["state"], "succeeded");
    assert_eq!(
        done["result"]["result"]["summary"],
        "Used answer and history"
    );
    assert_eq!(
        daemon::call(&state, &Request::GetLoop { id }).await?["state"],
        "active"
    );
    tx.send(true)?;
    service.await??;
    Ok(())
}
