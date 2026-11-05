use anyhow::Result;
use serde_json::{json, Value};
use std::{os::unix::fs::PermissionsExt, path::PathBuf, time::Duration};
use telos_core::*;
use telos_runtime::{
    daemon::{self, Request},
    runner::CodexRunner,
};
use telos_store::Store;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::watch;
use uuid::Uuid;

struct Fixture {
    root: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("telos-daemon-{}", Uuid::new_v4()));
        std::fs::create_dir_all(root.join("project")).unwrap();
        let script = root.join("fake-codex");
        std::fs::write(&script,r#"#!/usr/bin/env python3
import json,pathlib,sys
sys.stdin.read()
out=pathlib.Path(sys.argv[sys.argv.index('-o')+1])
out.write_text(json.dumps({'status':'done','summary':'fixture completed','evidence_paths':[],'remaining_work':[]}))
print(json.dumps({'type':'thread.started','thread_id':'daemon-fixture'}))
print(json.dumps({'type':'turn.completed','usage':{'output_tokens':3}}))
"#).unwrap();
        std::fs::set_permissions(script, std::fs::Permissions::from_mode(0o700)).unwrap();
        Self { root }
    }
    fn state(&self) -> PathBuf {
        self.root.join("state")
    }
    fn runner(&self) -> CodexRunner {
        CodexRunner {
            program: self.root.join("fake-codex"),
        }
    }
    fn spec(&self) -> ExecutionSpec {
        ExecutionSpec {
            task: "read fixture".into(),
            cwd: self.root.join("project").to_str().unwrap().into(),
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
fn contract() -> Contract {
    Contract {
        goal: "inspect fixture".into(),
        delivery_scope: "fixture".into(),
        criteria: vec![Criterion {
            id: "inspect".into(),
            description: "inspect fixture evidence".into(),
        }],
        interval_seconds: None,
        max_attempts: 2,
    }
}
async fn wait_ready(root: &std::path::Path) -> Result<()> {
    for _ in 0..100 {
        if daemon::call(root, &Request::Ping).await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    anyhow::bail!("daemon did not start")
}

#[tokio::test]
async fn private_service_runs_queue_records_evidence_and_excludes_second_owner() -> Result<()> {
    let f = Fixture::new();
    let state = f.state();
    let runner = f.runner();
    let (tx, rx) = watch::channel(false);
    let service = tokio::spawn({
        let state = state.clone();
        let runner = runner.clone();
        async move { daemon::serve(&state, runner, rx).await }
    });
    wait_ready(&state).await?;
    let (_second_tx, second_rx) = watch::channel(false);
    assert!(daemon::serve(&state, runner, second_rx).await.is_err());
    daemon::call(
        &state,
        &Request::RegisterProject {
            id: "fixture".into(),
            root: f.root.join("project"),
        },
    )
    .await?;
    let record = daemon::call(
        &state,
        &Request::CreateLoop {
            project_id: "fixture".into(),
            name: "fixture".into(),
            kind: LoopKind::Delivery,
            contract: contract(),
        },
    )
    .await?;
    let id = record["id"].as_str().unwrap().to_owned();
    assert!(daemon::call(
        &state,
        &Request::StartLoop {
            id: id.clone(),
            revision: 1,
            request_key: "one".into()
        }
    )
    .await
    .is_err());
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
                max_steps_per_cycle: 1,
            },
        },
    )
    .await?;
    let queued = daemon::call(
        &state,
        &Request::StartLoop {
            id: id.clone(),
            revision: 1,
            request_key: "one".into(),
        },
    )
    .await?;
    let step = queued["step_id"].as_str().unwrap().to_owned();
    let mut final_status = Value::Null;
    for _ in 0..150 {
        final_status = daemon::call(&state, &Request::StepStatus { id: step.clone() }).await?;
        if final_status["state"] == "succeeded" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(final_status["state"], "succeeded", "{final_status}");
    assert_eq!(
        daemon::call(&state, &Request::GetLoop { id: id.clone() }).await?["state"],
        "active"
    );
    let events = daemon::call(
        &state,
        &Request::Events {
            id: id.clone(),
            after_sequence: 0,
            through_sequence: None,
            limit: 20,
        },
    )
    .await?;
    assert!(events["items"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v["kind"] == "evidence_recorded"));
    let (client, server) = tokio::io::duplex(65536);
    let (read, write) = tokio::io::split(server);
    let bridge = tokio::spawn({
        let state = state.clone();
        async move { telos_runtime::mcp::serve(&state, read, write).await }
    });
    let mut client = BufReader::new(client);
    async fn rpc(client: &mut BufReader<tokio::io::DuplexStream>, request: Value) -> Result<Value> {
        client
            .get_mut()
            .write_all(format!("{request}\n").as_bytes())
            .await?;
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(5), client.read_line(&mut line)).await??;
        Ok(serde_json::from_str(&line)?)
    }
    let early = rpc(
        &mut client,
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
    )
    .await?;
    assert_eq!(early["error"]["code"], -32002);
    let init = rpc(&mut client, json!({"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":"2099-01-01","capabilities":{},"clientInfo":{"name":"test","version":"1"}}})).await?;
    assert_eq!(init["result"]["protocolVersion"], "2025-11-25");
    client
        .get_mut()
        .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
        .await?;
    let listed = rpc(
        &mut client,
        json!({"jsonrpc":"2.0","id":3,"method":"tools/list"}),
    )
    .await?;
    assert_eq!(listed["result"]["tools"].as_array().unwrap().len(), 24);
    assert!(listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .all(|tool| !tool["name"]
            .as_str()
            .unwrap()
            .contains("reconcile_environment")));
    assert!(listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .all(|tool| !tool["name"]
            .as_str()
            .unwrap()
            .contains("environment_adapter")));
    let schedule=rpc(&mut client,json!({"jsonrpc":"2.0","id":37,"method":"tools/call","params":{"name":"loop_schedule","arguments":{"id":id}}})).await?;
    assert_eq!(schedule["result"]["isError"], false);
    let schedule: Value =
        serde_json::from_str(schedule["result"]["content"][0]["text"].as_str().unwrap())?;
    assert_eq!(schedule["armed_scope"], "managed_execution_only");
    let context=rpc(&mut client,json!({"jsonrpc":"2.0","id":38,"method":"tools/call","params":{"name":"loop_context","arguments":{"id":id,"revision":1}}})).await?;
    assert_eq!(context["result"]["isError"], false);
    let context: Value =
        serde_json::from_str(context["result"]["content"][0]["text"].as_str().unwrap())?;
    assert!(context["project_observations"]["items"]
        .as_array()
        .unwrap()
        .is_empty());
    let association=rpc(&mut client,json!({"jsonrpc":"2.0","id":39,"method":"tools/call","params":{"name":"loop_associate_remediation","arguments":{"id":id,"revision":1,"diagnosis_step_id":"missing","delivery_loop_id":id,"delivery_revision":1,"request_key":"invalid-link"}}})).await?;
    assert_eq!(association["result"]["isError"], true);
    let audit = rpc(&mut client, json!({"jsonrpc":"2.0","id":28,"method":"tools/call","params":{"name":"loop_events","arguments":{"id":id,"limit":1}}})).await?;
    assert_eq!(audit["result"]["isError"], false);
    let audit: Value =
        serde_json::from_str(audit["result"]["content"][0]["text"].as_str().unwrap())?;
    assert_eq!(audit["items"].as_array().unwrap().len(), 1);
    let body = rpc(&mut client, json!({"jsonrpc":"2.0","id":29,"method":"tools/call","params":{"name":"event_body","arguments":{"id":id,"sequence":audit["items"][0]["sequence"],"offset":0,"limit":127}}})).await?;
    assert_eq!(body["result"]["isError"], false);
    let workspace = rpc(&mut client, json!({"jsonrpc":"2.0","id":32,"method":"tools/call","params":{"name":"loop_workspace","arguments":{"id":id,"revision":1}}})).await?;
    assert_eq!(workspace["result"]["isError"], false);
    assert_eq!(workspace["result"]["content"][0]["text"], "null");
    let verification = rpc(&mut client, json!({"jsonrpc":"2.0","id":34,"method":"tools/call","params":{"name":"loop_verification","arguments":{"id":id}}})).await?;
    assert_eq!(verification["result"]["isError"], false);
    let ownership: Value = serde_json::from_str(
        verification["result"]["content"][0]["text"]
            .as_str()
            .unwrap(),
    )?;
    assert_eq!(
        ownership["external_environment"]["new_execution_blocked"],
        false
    );
    assert_eq!(
        ownership["external_environment"]["uncertain_session_count"],
        0
    );

    let reconcile = rpc(&mut client, json!({"jsonrpc":"2.0","id":35,"method":"tools/call","params":{"name":"reconcile_verification","arguments":{"lease_id":"untrusted"}}})).await?;
    assert_eq!(reconcile["error"]["code"], -32602);
    let delivery = rpc(&mut client, json!({"jsonrpc":"2.0","id":33,"method":"tools/call","params":{"name":"loop_delivery","arguments":{"id":id,"revision":1}}})).await?;
    assert_eq!(delivery["result"]["isError"], false);
    let delivery: Value =
        serde_json::from_str(delivery["result"]["content"][0]["text"].as_str().unwrap())?;
    assert_eq!(delivery["available"], false);
    let evidence_list = rpc(&mut client, json!({"jsonrpc":"2.0","id":30,"method":"tools/call","params":{"name":"loop_evidence","arguments":{"id":id,"limit":10}}})).await?;
    assert_eq!(evidence_list["result"]["isError"], false);
    let evidence_list: Value = serde_json::from_str(
        evidence_list["result"]["content"][0]["text"]
            .as_str()
            .unwrap(),
    )?;
    let evidence = rpc(&mut client, json!({"jsonrpc":"2.0","id":31,"method":"tools/call","params":{"name":"evidence_get","arguments":{"id":evidence_list["items"][0]["id"],"offset":0,"limit":32768}}})).await?;
    assert_eq!(evidence["result"]["isError"], false);
    let evidence: Value =
        serde_json::from_str(evidence["result"]["content"][0]["text"].as_str().unwrap())?;
    assert_eq!(evidence["integrity_verified"], true);
    let report: Value = serde_json::from_str(evidence["text"].as_str().unwrap())?;
    assert_eq!(report["outcome"], "succeeded");
    let get = rpc(&mut client, json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"loop_get","arguments":{"id":id}}})).await?;
    assert_eq!(get["result"]["isError"], false);
    let record: Value =
        serde_json::from_str(get["result"]["content"][0]["text"].as_str().unwrap())?;
    assert_eq!(record["id"], id);
    let pause = rpc(&mut client, json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"loop_pause","arguments":{"id":id,"expected_version":record["version"]}}})).await?;
    assert_eq!(pause["result"]["isError"], false);
    let paused: Value =
        serde_json::from_str(pause["result"]["content"][0]["text"].as_str().unwrap())?;
    assert_eq!(paused["state"], "paused");
    let stale = rpc(&mut client, json!({"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"loop_resume","arguments":{"id":id,"expected_version":record["version"]}}})).await?;
    assert_eq!(stale["result"]["isError"], true);
    let resume = rpc(&mut client, json!({"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"loop_resume","arguments":{"id":id,"expected_version":paused["version"]}}})).await?;
    assert_eq!(resume["result"]["isError"], false);
    let denied = rpc(&mut client, json!({"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"enqueue","arguments":{}}})).await?;
    assert_eq!(denied["error"]["code"], -32602);
    drop(client);
    bridge.await??;
    tx.send(true)?;
    service.await??;
    assert!(!daemon::socket_path(&state)?.exists());
    Ok(())
}

#[tokio::test]
async fn restart_marks_even_unexpired_work_uncertain_instead_of_redispatch() -> Result<()> {
    let f = Fixture::new();
    let state = f.state();
    let store = Store::open(&state).await?;
    store
        .register_project("fixture", &f.root.join("project"))
        .await?;
    let record = store
        .create_loop("fixture", "fixture", LoopKind::Delivery, &contract())
        .await?;
    let cycle = store.start_cycle(&record.id, 1, "one").await?;
    let step = store.enqueue_step(&cycle, 1, "one", &f.spec(), 0).await?;
    let lease = store
        .claim_next("previous-daemon", telos_store::now(), 3600)
        .await?
        .unwrap();
    assert_eq!(lease.step_id, step);
    store.close().await;
    let (tx, rx) = watch::channel(false);
    let runner = f.runner();
    let service = tokio::spawn({
        let state = state.clone();
        async move { daemon::serve(&state, runner, rx).await }
    });
    wait_ready(&state).await?;
    let status = daemon::call(&state, &Request::StepStatus { id: step }).await?;
    assert_eq!(status["state"], "outcome_unknown");
    assert_eq!(status["attempt"], 1);
    assert_eq!(
        daemon::call(&state, &Request::GetLoop { id: record.id }).await?["state"],
        json!("blocked")
    );
    tx.send(true)?;
    service.await??;
    Ok(())
}

#[tokio::test]
async fn recovery_gate_prevents_process_recovery_and_database_initialization() -> Result<()> {
    let f = Fixture::new();
    let state = f.state();
    std::fs::create_dir(&state)?;
    std::fs::write(state.join("restore-required.json"), "incomplete marker")?;
    // A malformed marker must still block; it must not be silently treated as absent.
    let (_tx, rx) = watch::channel(false);
    let error = daemon::serve(&state, f.runner(), rx).await.unwrap_err();
    assert!(error.to_string().contains("requires reconciliation"));
    assert!(!state.join("telos.db").exists());
    std::fs::remove_file(state.join("restore-required.json"))?;
    std::os::unix::fs::symlink(state.join("absent"), state.join("restore-required.json"))?;
    let (_tx, rx) = watch::channel(false);
    assert!(daemon::serve(&state, f.runner(), rx)
        .await
        .unwrap_err()
        .to_string()
        .contains("requires reconciliation"));
    assert!(!state.join("telos.db").exists());
    Ok(())
}

#[tokio::test]
async fn prepared_database_blocks_startup_even_without_filesystem_marker() -> Result<()> {
    let f = Fixture::new();
    let state = f.state();
    let store = Store::open(&state).await?;
    store
        .prepare_recovery(&"a".repeat(64), &"b".repeat(64), &state)
        .await?;
    store.close().await;
    let (_tx, rx) = watch::channel(false);
    let error = daemon::serve(&state, f.runner(), rx).await.unwrap_err();
    assert!(error
        .to_string()
        .contains("prepared recovery requires reconciliation"));
    assert!(!state.join("restore-required.json").exists());
    Ok(())
}

#[tokio::test]
async fn recovery_review_serves_mcp_evidence_without_writes_or_external_work() -> Result<()> {
    let f = Fixture::new();
    let state = f.state();
    let store = Store::open(&state).await?;
    store
        .register_project("fixture", &f.root.join("project"))
        .await?;
    let goal = store
        .create_loop("fixture", "recovery", LoopKind::Delivery, &contract())
        .await?;
    let cycle = store.start_cycle(&goal.id, 1, "fixture").await?;
    let evidence = store
        .add_evidence(&cycle, 1, "artifact", "environment", b"restored evidence")
        .await?;
    store
        .prepare_recovery(&"a".repeat(64), &"b".repeat(64), &state)
        .await?;
    let material = store
        .record_recovery_review_evidence(
            "fixture",
            &"a".repeat(64),
            "synthetic-review",
            b"Synthetic recovery operator material",
        )
        .await?;
    let review = telos_core::RecoveryProjectReview {
        project_id: "fixture".into(),
        manifest_sha256: "a".repeat(64),
        decision_id: "synthetic-review".into(),
        operator: "fixture".into(),
        statement: "Synthetic fixture, no real external review".into(),
        reviewed_external_state: true,
        evidence: vec![material.clone()],
    };
    store.release_recovery_project(&review).await?;
    store.close().await;
    std::fs::write(
        state.join("restore-required.json"),
        "marker intentionally not parsed by read-only service",
    )?;
    std::fs::create_dir_all(state.join("runs/old"))?;
    std::fs::write(
        state.join("runs/old/processes.json"),
        "must not be interpreted as process cleanup",
    )?;
    let before = std::fs::read(state.join("telos.db"))?;
    let (tx, rx) = watch::channel(false);
    let service = tokio::spawn({
        let state = state.clone();
        async move { daemon::serve_recovery_review(&state, rx).await }
    });
    wait_ready(&state).await?;
    let ping = daemon::call(&state, &Request::Ping).await?;
    assert_eq!(ping["mode"], "recovery_review");
    assert_eq!(ping["execution_enabled"], false);
    let projects = daemon::call(&state, &Request::ListProjects).await?;
    assert_eq!(projects[0]["recovery_hold"]["state"], "cleared");
    assert_eq!(
        projects[0]["recovery_hold"]["proof"],
        serde_json::to_value(&review)?
    );
    for request in [
        Request::ReconcileEnvironment {
            review: telos_core::EnvironmentReconciliation {
                loop_id: goal.id.clone(),
                revision: 1,
                session_id: "fixture".into(),
                environment_sha256: "a".repeat(64),
                adapter_sha256: "b".repeat(64),
                expected_sequence: 1,
                expected_version: 1,
                decision_id: "must-not-mutate".into(),
                provider_observed_at: telos_store::now(),
                provider_lease_owner: None,
                provider_operation_active: false,
                remote_operations_stopped: true,
                local_owners_retired: true,
                provider_protocol_reconciled: true,
                operator_note: "recovery-review must reject before inspection".into(),
            },
        },
        Request::InspectProject {
            id: "fixture".into(),
        },
        Request::LoopContext {
            id: goal.id.clone(),
            revision: 1,
        },
        Request::StartLoop {
            id: goal.id.clone(),
            revision: 1,
            request_key: "must-not-run".into(),
        },
        Request::VerifyLoop {
            id: goal.id.clone(),
            revision: 1,
        },
        Request::Control {
            id: goal.id.clone(),
            expected_version: 2,
            action: "stop".into(),
        },
    ] {
        assert!(daemon::call(&state, &request)
            .await
            .unwrap_err()
            .to_string()
            .contains("read-only"));
    }
    let (client, server) = tokio::io::duplex(65536);
    let (read, write) = tokio::io::split(server);
    let bridge = tokio::spawn({
        let state = state.clone();
        async move { telos_runtime::mcp::serve(&state, read, write).await }
    });
    let mut client = BufReader::new(client);
    for (request, expect_error) in [
        (
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"review-test","version":"1"}}}),
            false,
        ),
        (
            json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"evidence_get","arguments":{"id":evidence,"offset":0,"limit":100}}}),
            false,
        ),
        (
            json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"evidence_get","arguments":{"id":material.id,"offset":0,"limit":100}}}),
            false,
        ),
        (
            json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"loop_start","arguments":{"id":goal.id,"revision":1,"request_key":"denied"}}}),
            true,
        ),
    ] {
        client
            .get_mut()
            .write_all(format!("{request}\n").as_bytes())
            .await?;
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(5), client.read_line(&mut line)).await??;
        let response: Value = serde_json::from_str(&line)?;
        if request["id"] == 1 {
            client
                .get_mut()
                .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
                .await?;
        } else {
            assert_eq!(response["result"]["isError"], expect_error);
        }
        if request["id"] == 4 {
            let body: Value =
                serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap())?;
            assert_eq!(body["text"], "Synthetic recovery operator material");
            assert_eq!(body["integrity_verified"], true);
            assert_eq!(body["kind"], "recovery_operator_material");
        }
        if request["id"] == 2 {
            let body: Value =
                serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap())?;
            assert_eq!(body["text"], "restored evidence");
            assert_eq!(body["integrity_verified"], true);
        }
    }
    drop(client);
    bridge.await??;
    tx.send(true)?;
    service.await??;
    assert_eq!(std::fs::read(state.join("telos.db"))?, before);
    assert!(!state.join("telos.db-wal").exists());
    let readonly = Store::open_recovery_review(&state).await?;
    assert!(readonly
        .register_project("must-not-write", &f.root)
        .await
        .is_err());
    readonly.close().await;
    assert_eq!(std::fs::read(state.join("telos.db"))?, before);

    assert_eq!(
        std::fs::read_to_string(state.join("runs/old/processes.json"))?,
        "must not be interpreted as process cleanup"
    );
    Ok(())
}

#[tokio::test]
async fn recovery_review_rejects_unprepared_state_without_migrating() -> Result<()> {
    let f = Fixture::new();
    let state = f.state();
    let store = Store::open(&state).await?;
    store.close().await;
    let before = std::fs::read(state.join("telos.db"))?;
    let (_tx, rx) = watch::channel(false);
    assert!(daemon::serve_recovery_review(&state, rx)
        .await
        .unwrap_err()
        .to_string()
        .contains("prepared recovery image"));
    assert_eq!(std::fs::read(state.join("telos.db"))?, before);
    Ok(())
}

#[tokio::test]
async fn activation_authorization_alone_does_not_bypass_recovery_startup_gate() -> Result<()> {
    let f = Fixture::new();
    let state = f.state();
    let store = Store::open(&state).await?;
    store
        .prepare_recovery(&"a".repeat(64), &"b".repeat(64), &state)
        .await?;
    let decision = telos_core::RecoveryActivation {
        manifest_sha256: "a".repeat(64),
        source_receipt_sha256: "c".repeat(64),
        target_state: f.root.join("future-target/state").to_string_lossy().into(),
        decision_id: "synthetic".into(),
        operator: "fixture".into(),
        statement: "Synthetic fixture only; not a real operator decision".into(),
        source_instance_retired: true,
        exclusive_access_confirmed: true,
    };
    store.authorize_recovery_activation(&decision, &json!({"checks_passed":true,"blockers":[],"source_receipt_sha256":decision.source_receipt_sha256,"inventory":{"preparation":{"manifest_sha256":decision.manifest_sha256}}})).await?;
    store.close().await;
    let (_tx, rx) = watch::channel(false);
    assert!(daemon::serve(&state, f.runner(), rx)
        .await
        .unwrap_err()
        .to_string()
        .contains("prepared recovery requires reconciliation"));
    Ok(())
}
