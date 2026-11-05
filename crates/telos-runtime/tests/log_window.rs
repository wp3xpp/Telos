use anyhow::Result;
use serde_json::json;
use std::{io::Write, path::PathBuf};
use telos_core::*;
use telos_runtime::{log_window::sample, verifier::verify_loop_at};
use telos_store::Store;

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!("telos-log-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&p).unwrap();
        Self(p.canonicalize().unwrap())
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn check() -> LogWindowCheck {
    LogWindowCheck {
        criterion_id: "health".into(),
        relative_path: "events.jsonl".into(),
        window_seconds: 10,
        max_gap_seconds: 6,
    }
}
fn append(f: &Fixture, at: i64, kind: &str) -> Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(f.0.join("events.jsonl"))?;
    writeln!(file, "{}", json!({"timestamp":at,"kind":kind}))?;
    Ok(())
}

#[test]
fn log_errors_are_consumed_once_and_recovery_requires_new_full_window() -> Result<()> {
    let f = Fixture::new();
    let check = check();
    append(&f, 100, "heartbeat")?;
    let (c, v, _) = sample(&f.0, &check, None, 100)?;
    assert_eq!(v, Verdict::Inconclusive);
    append(&f, 105, "heartbeat")?;
    let (c, _, _) = sample(&f.0, &check, Some(c), 105)?;
    append(&f, 110, "heartbeat")?;
    let (c, v, _) = sample(&f.0, &check, Some(c), 110)?;
    assert_eq!(v, Verdict::Passed);
    append(&f, 111, "error")?;
    let (c, v, _) = sample(&f.0, &check, Some(c), 111)?;
    assert_eq!(v, Verdict::Failed);
    assert_eq!(c.errors, 1);
    let (c, v, _) = sample(&f.0, &check, Some(c), 112)?;
    assert_eq!(v, Verdict::Failed);
    assert_eq!(c.errors, 1);
    append(&f, 113, "heartbeat")?;
    let (c, v, _) = sample(&f.0, &check, Some(c), 113)?;
    assert_eq!(v, Verdict::Inconclusive);
    append(&f, 118, "heartbeat")?;
    let (c, _, _) = sample(&f.0, &check, Some(c), 118)?;
    append(&f, 123, "heartbeat")?;
    let (_, v, _) = sample(&f.0, &check, Some(c), 123)?;
    assert_eq!(v, Verdict::Passed);
    Ok(())
}

#[test]
fn missing_heartbeat_partial_records_rotation_and_truncation_never_pass() -> Result<()> {
    let f = Fixture::new();
    let check = check();
    let (_, v, _) = sample(&f.0, &check, None, 100)?;
    assert_eq!(v, Verdict::Inconclusive);
    std::fs::write(
        f.0.join("events.jsonl"),
        r#"{"timestamp":100,"kind":"heartbeat"}"#,
    )?;
    let (c, v, _) = sample(&f.0, &check, None, 100)?;
    assert_eq!(c.offset, 0);
    assert_eq!(v, Verdict::Inconclusive);
    std::fs::OpenOptions::new()
        .append(true)
        .open(f.0.join("events.jsonl"))?
        .write_all(b"\n")?;
    let (c, _, _) = sample(&f.0, &check, Some(c), 101)?;
    let (c, v, _) = sample(&f.0, &check, Some(c), 120)?;
    assert_eq!(v, Verdict::Inconclusive);
    assert!(c.window_start.is_none());
    std::fs::rename(f.0.join("events.jsonl"), f.0.join("old.log"))?;
    append(&f, 121, "heartbeat")?;
    let (c, v, _) = sample(&f.0, &check, Some(c), 121)?;
    assert_eq!(v, Verdict::Inconclusive);
    assert_eq!(c.window_start, Some(121));
    std::fs::write(
        f.0.join("events.jsonl"),
        format!("{}\n", json!({"timestamp":122,"kind":"heartbeat"})),
    )?;
    let (c, v, _) = sample(&f.0, &check, Some(c), 122)?;
    assert_eq!(v, Verdict::Inconclusive);
    assert_eq!(c.window_start, Some(122));
    std::fs::OpenOptions::new()
        .append(true)
        .open(f.0.join("events.jsonl"))?
        .write_all(b"bad json\n")?;
    let (_, v, _) = sample(&f.0, &check, Some(c), 123)?;
    assert_eq!(v, Verdict::Error);
    Ok(())
}

#[test]
fn an_error_followed_by_heartbeat_in_one_read_is_still_reported() -> Result<()> {
    let f = Fixture::new();
    let check = check();
    append(&f, 100, "heartbeat")?;
    let (cursor, _, _) = sample(&f.0, &check, None, 100)?;
    append(&f, 101, "error")?;
    append(&f, 102, "heartbeat")?;
    let (cursor, verdict, _) = sample(&f.0, &check, Some(cursor), 103)?;
    assert_eq!(verdict, Verdict::Failed);
    let (_, verdict, _) = sample(&f.0, &check, Some(cursor), 104)?;
    assert_eq!(verdict, Verdict::Inconclusive);
    Ok(())
}

#[tokio::test]
async fn log_cursor_persists_and_only_complete_window_closes_goal() -> Result<()> {
    let f = Fixture::new();
    let s = Store::open(&f.0.join("state")).await?;
    s.register_project("fixture", &f.0).await?;
    let l = s
        .create_loop(
            "fixture",
            "logs",
            LoopKind::Delivery,
            &Contract {
                goal: "observe healthy window".into(),
                delivery_scope: "fixture".into(),
                criteria: vec![Criterion {
                    id: "health".into(),
                    description: "continuous heartbeats without errors".into(),
                }],
                interval_seconds: None,
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
            checks: vec![],
            log_checks: vec![check()],
        },
    )
    .await?;
    let cycle = s.start_cycle(&l.id, 1, "fixture").await?;
    append(&f, 100, "heartbeat")?;
    assert_eq!(
        verify_loop_at(&s, &l.id, 1, 100).await?["verdict"],
        "inconclusive"
    );
    let cursor = s.log_cursor(&cycle, "health", 1).await?.unwrap();
    assert!(cursor["body"]["cursor"]["offset"].as_u64().unwrap() > 0);
    assert!(s
        .save_log_cursor(&cycle, "health", 1, 0, &cursor["body"])
        .await
        .is_err());
    s.close().await;
    let s = Store::open(&f.0.join("state")).await?;
    append(&f, 105, "heartbeat")?;
    assert_eq!(
        verify_loop_at(&s, &l.id, 1, 105).await?["verdict"],
        "inconclusive"
    );
    assert!(
        s.notification_status().await?.is_empty(),
        "normal window warmup must remain quiet"
    );
    append(&f, 110, "heartbeat")?;
    assert_eq!(
        verify_loop_at(&s, &l.id, 1, 110).await?["verdict"],
        "passed"
    );
    assert_eq!(s.get_loop(&l.id).await?.state, LoopState::Completed);
    Ok(())
}
