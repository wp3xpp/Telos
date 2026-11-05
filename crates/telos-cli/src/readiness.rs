use anyhow::{Context, Result};
use serde_json::Value;

pub fn print(report: &Value) -> Result<()> {
    let config = &report["configuration"];
    let missing = config["missing"]
        .as_array()
        .context("missing configuration status")?;
    let collection = &report["collection"];
    let receipts = collection["receipts"]
        .as_array()
        .context("missing collection status")?;
    let missing_sources = collection["missing_sources"]
        .as_array()
        .context("missing source status")?;
    let inspection = &report["inspection"];
    println!(
        "Loop {} (revision {}, {})",
        report["loop_id"].as_str().context("missing Loop ID")?,
        report["revision"],
        report["loop_state"].as_str().unwrap_or("unknown")
    );
    if missing.is_empty() {
        println!(
            "Configuration: policy r{}, verification plan v{}",
            config["policy_revision"], config["plan_version"]
        );
    } else {
        let details: Vec<_> = missing
            .iter()
            .map(|item| match item.as_str() {
                Some("execution_policy_missing") => "execution policy missing",
                Some("execution_policy_disabled") => "execution policy disabled",
                Some("verification_plan_missing") => "verification plan missing",
                _ => "unknown configuration gap",
            })
            .collect();
        println!("Configuration: {}", details.join(", "));
    }
    if receipts.is_empty() {
        println!("Collection: no recorded evidence");
    } else {
        println!(
            "Collection: evidence recorded for {} source(s)",
            receipts.len()
        );
        for receipt in receipts {
            let gap = if receipt["gap_reason"].is_null() {
                ""
            } else {
                ", gap recorded"
            };
            println!(
                "  {}: event {}, backlog {} bytes{gap}",
                receipt["source_id"].as_str().unwrap_or("unknown"),
                receipt["event_sequence"],
                receipt["backlog_bytes"]
            );
        }
    }
    for source in missing_sources {
        println!(
            "  {}: no recorded evidence",
            source.as_str().unwrap_or("unknown")
        );
    }
    if inspection["armed"] == true {
        if let Some(due) = inspection["next_due_at"].as_i64() {
            println!("Inspection: armed, next due at Unix timestamp {due}");
        } else {
            println!("Inspection: armed, awaiting cycle completion");
        }
    } else {
        println!("Inspection: not scheduled");
    }
    println!(
        "First verification: {}",
        report["first_verification"]["verdict"]
            .as_str()
            .unwrap_or("no recorded verdict")
    );
    Ok(())
}
