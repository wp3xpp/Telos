//! Export immutable ledger evidence for frontend review. Never reads the live worktree.
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::Path;
use telos_runtime::daemon::{self, Request};

async fn evidence(state: &Path, id: &str, delivery: &Value, goal: &str) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut expected_hash = None;
    loop {
        let page = daemon::call(
            state,
            &Request::GetEvidence {
                id: id.into(),
                offset: bytes.len(),
                limit: 32768,
            },
        )
        .await?;
        ensure!(
            page["integrity_verified"] == true
                && page["id"] == id
                && page["loop_id"] == goal
                && page["contract_revision"] == delivery["contract_revision"]
                && page["artifact"] == delivery["artifact"],
            "evidence binding changed"
        );
        let hash = page["sha256"]
            .as_str()
            .context("evidence hash missing")?
            .to_owned();
        if let Some(expected) = &expected_hash {
            ensure!(expected == &hash, "evidence hash changed between pages");
        } else {
            expected_hash = Some(hash);
        }
        ensure!(
            page["offset"].as_u64() == Some(bytes.len() as u64),
            "evidence offset mismatch"
        );
        let chunk: Vec<u8> = serde_json::from_value(page["bytes"].clone())?;
        bytes.extend(chunk);
        ensure!(
            bytes.len() <= 8 * 1024 * 1024,
            "evidence exceeds export bound"
        );
        if page["next_offset"].is_null() {
            ensure!(
                page["byte_length"].as_u64() == Some(bytes.len() as u64)
                    && expected_hash.as_deref() == Some(&format!("{:x}", Sha256::digest(&bytes))),
                "export integrity failure"
            );
            return Ok(bytes);
        }
        ensure!(
            page["next_offset"].as_u64() == Some(bytes.len() as u64)
                && page["offset"] != page["next_offset"],
            "invalid evidence continuation"
        );
    }
}

pub async fn export(state: &Path, id: &str, output: &Path) -> Result<Value> {
    ensure!(
        output.is_absolute(),
        "review output must be an absolute new directory"
    );
    let goal = daemon::call(state, &Request::GetLoop { id: id.into() }).await?;
    ensure!(
        goal["kind"] == "delivery",
        "review requires a delivery goal"
    );
    let revision = goal["revision"].as_i64().context("goal revision missing")?;
    let delivery = daemon::call(
        state,
        &Request::DeliveryResult {
            id: id.into(),
            revision,
        },
    )
    .await?;
    ensure!(
        delivery["available"] == true,
        "delivery has no recorded review yet"
    );
    let review_id = delivery["review_evidence_id"]
        .as_str()
        .context("review evidence missing")?;
    let review_bytes = evidence(state, review_id, &delivery, id).await?;
    let review: Value = serde_json::from_slice(&review_bytes)?;
    validate_review(&review, &delivery)?;
    let mut files = vec![
        ("review.json".to_owned(), review_bytes),
        (
            "changes.patch".to_owned(),
            review["patch"]
                .as_str()
                .context("review patch missing")?
                .as_bytes()
                .to_vec(),
        ),
    ];
    let mut index = Vec::new();
    for (i, evidence_id) in delivery["acceptance_evidence_ids"]
        .as_array()
        .context("acceptance evidence missing")?
        .iter()
        .enumerate()
    {
        ensure!(i < 128, "too many acceptance artifacts");
        let evidence_id = evidence_id.as_str().context("invalid evidence ID")?;
        let bytes = evidence(state, evidence_id, &delivery, id).await?;
        let name = format!("acceptance-{i}.bin");
        index.push(
            json!({"id":evidence_id,"file":name,"sha256":format!("{:x}",Sha256::digest(&bytes))}),
        );
        ensure!(
            files.iter().map(|(_, b)| b.len()).sum::<usize>() + bytes.len() <= 32 * 1024 * 1024,
            "review export exceeds 32MiB"
        );
        files.push((name, bytes));
    }
    let current = daemon::call(state, &Request::GetLoop { id: id.into() }).await?;
    let latest = daemon::call(
        state,
        &Request::DeliveryResult {
            id: id.into(),
            revision,
        },
    )
    .await?;
    ensure!(
        current["version"] == goal["version"]
            && current["revision"] == goal["revision"]
            && latest == delivery,
        "delivery changed during export; inspect again"
    );
    let receipt = json!({"loop_id":id,"revision":revision,"loop_state":goal["state"],
        "delivery":delivery,"acceptance_files":index,"output":output,
        "note":"Historical verified bytes, not a live worktree snapshot. Review changes.patch before applying to the pinned base. No source files were changed."});
    // create_dir refuses an existing directory or symlink; failure leaves a visibly
    // incomplete export without receipt.json, and never overwrites user files.
    std::fs::create_dir(output).context("create new review directory")?;
    for (name, bytes) in files {
        std::fs::write(output.join(name), bytes)?;
    }
    std::fs::write(
        output.join("receipt.json"),
        serde_json::to_vec_pretty(&receipt)?,
    )?;
    Ok(receipt)
}

fn validate_review(review: &Value, delivery: &Value) -> Result<()> {
    ensure!(
        review["format"] == "telos-delivery-review-v1",
        "unsupported review format"
    );
    let workspace = review["workspace_id"]
        .as_str()
        .context("review workspace missing")?;
    let snapshot = review["snapshot_sha256"]
        .as_str()
        .context("review snapshot missing")?;
    ensure!(
        delivery["artifact"].as_str() == Some(&format!("workspace:{workspace}:{snapshot}")),
        "review artifact mismatch"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_review_from_another_snapshot() {
        let review =
            json!({"format":"telos-delivery-review-v1","workspace_id":"w","snapshot_sha256":"one"});
        assert!(validate_review(&review, &json!({"artifact":"workspace:w:one"})).is_ok());
        assert!(validate_review(&review, &json!({"artifact":"workspace:w:two"})).is_err());
    }
}
