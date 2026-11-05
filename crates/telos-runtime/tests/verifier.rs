use anyhow::Result;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use telos_core::*;
use telos_runtime::verifier::verify_loop;
use telos_store::Store;

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!("telos-verifier-{}", uuid::Uuid::new_v4())))
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn setup(kind: LoopKind) -> Result<(Fixture, Store, LoopRecord, String, VerificationPlan)> {
    let f = Fixture::new();
    let store = Store::open(&f.0.join("state")).await?;
    std::fs::create_dir(f.0.join("project"))?;
    store
        .register_project("fixture", &f.0.join("project"))
        .await?;
    let contract = Contract {
        goal: "deliver exact fixture bytes".into(),
        delivery_scope: "fixture".into(),
        criteria: vec![Criterion {
            id: "bytes".into(),
            description: "artifact equals approved fixture".into(),
        }],
        interval_seconds: if kind == LoopKind::Continuous {
            Some(60)
        } else {
            None
        },
        max_attempts: 2,
    };
    let record = store
        .create_loop("fixture", "fixture", kind, &contract)
        .await?;
    let cycle = store.start_cycle(&record.id, 1, "fixture").await?;
    let plan = VerificationPlan {
        test_checks: vec![],
        log_checks: vec![],
        checks: vec![FileHashCheck {
            criterion_id: "bytes".into(),
            relative_path: "artifact.txt".into(),
            expected_sha256: format!("{:x}", Sha256::digest(b"approved fixture")),
        }],
    };
    Ok((f, store, record, cycle, plan))
}

#[tokio::test]
async fn observes_missing_wrong_then_correct_artifact_before_completing() -> Result<()> {
    let (f, s, l, _, plan) = setup(LoopKind::Delivery).await?;
    assert!(verify_loop(&s, &l.id, 1).await.is_err());
    s.configure_verification(&l.id, 1, 0, &plan).await?;
    assert_eq!(verify_loop(&s, &l.id, 1).await?["verdict"], "inconclusive");
    std::fs::write(f.0.join("project/artifact.txt"), "wrong")?;
    assert_eq!(verify_loop(&s, &l.id, 1).await?["verdict"], "failed");
    assert_eq!(s.get_loop(&l.id).await?.state, LoopState::Active);
    std::fs::write(f.0.join("project/artifact.txt"), "approved fixture")?;
    let passed = verify_loop(&s, &l.id, 1).await?;
    assert_eq!(passed["verdict"], "passed");
    assert_eq!(passed["loop"]["state"], "completed");
    let evidence = s
        .get_evidence(passed["evidence_id"].as_str().unwrap(), 0, 32768)
        .await?;
    assert!(evidence["text"]
        .as_str()
        .unwrap()
        .contains("observed_sha256"));
    Ok(())
}

#[tokio::test]
async fn rejects_invalid_plans_and_stale_plan_results() -> Result<()> {
    let (_f, s, l, c, plan) = setup(LoopKind::Delivery).await?;
    let mut invalid = plan.clone();
    invalid.checks[0].relative_path = "../outside".into();
    assert!(s
        .configure_verification(&l.id, 1, 0, &invalid)
        .await
        .is_err());
    invalid.checks.clear();
    assert!(s
        .configure_verification(&l.id, 1, 0, &invalid)
        .await
        .is_err());
    s.configure_verification(&l.id, 1, 0, &plan).await?;
    assert!(s.configure_verification(&l.id, 1, 0, &plan).await.is_err());
    let evidence = s
        .add_evidence(&c, 1, "fixture", "fixture", b"old observation")
        .await?;
    s.configure_verification(&l.id, 1, 1, &plan).await?;
    let old = Verification {
        cycle_id: c,
        contract_revision: 1,
        artifact: "fixture".into(),
        environment: "fixture".into(),
        observation_complete: true,
        results: vec![CriterionResult {
            criterion_id: "bytes".into(),
            verdict: Verdict::Passed,
            evidence_ids: vec![evidence],
            explanation: "old plan result".into(),
        }],
    };
    assert!(s.verify_configured(&old, 1).await.is_err());
    assert_eq!(s.get_loop(&l.id).await?.state, LoopState::Active);
    Ok(())
}

#[tokio::test]
async fn symlink_is_error_and_continuous_success_returns_to_observation() -> Result<()> {
    let (f, s, l, _, plan) = setup(LoopKind::Continuous).await?;
    s.configure_verification(&l.id, 1, 0, &plan).await?;
    std::fs::write(f.0.join("outside.txt"), "approved fixture")?;
    std::os::unix::fs::symlink(f.0.join("outside.txt"), f.0.join("project/artifact.txt"))?;
    assert_eq!(verify_loop(&s, &l.id, 1).await?["verdict"], "error");
    std::fs::remove_file(f.0.join("project/artifact.txt"))?;
    std::fs::write(f.0.join("project/artifact.txt"), "approved fixture")?;
    assert_eq!(verify_loop(&s, &l.id, 1).await?["verdict"], "passed");
    let record = s.get_loop(&l.id).await?;
    assert_eq!(record.state, LoopState::Active);
    assert!(record.next_due_at.unwrap() > telos_store::now());
    Ok(())
}
