//! Read-only historical coverage report. Does not set goal acceptance or infer uptime.
use anyhow::{ensure, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs::OpenOptions,
    io::{Read, Seek, SeekFrom},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
};

fn summarize(bytes: &[u8], at: i64, minimum: i64, allowed_gap: i64) -> Result<Value> {
    ensure!(
        (1..=366 * 86400).contains(&minimum),
        "minimum duration must be 1..31622400 seconds"
    );
    ensure!(
        (1..=86400).contains(&allowed_gap),
        "maximum gap must be 1..86400 seconds"
    );
    let mut first: Option<i64> = None;
    let mut last: Option<i64> = None;
    let (
        mut valid,
        mut heartbeat,
        mut errors,
        mut invalid,
        mut regressions,
        mut gaps,
        mut duplicate_times,
    ) = (0u64, 0u64, 0u64, 0u64, 0u64, 0u64, 0u64);
    let mut largest_gap = 0;
    let mut incomplete = false;
    for line in bytes.split_inclusive(|b| *b == b'\n') {
        if line.last() != Some(&b'\n') {
            incomplete = true;
            break;
        }
        let parsed = if line.len() <= 16384 {
            serde_json::from_slice::<Value>(line).ok()
        } else {
            None
        };
        let parsed = parsed.filter(|v| {
            v.as_object().is_some_and(|fields| {
                fields
                    .keys()
                    .all(|k| matches!(k.as_str(), "timestamp" | "kind" | "message"))
            }) && v["timestamp"].as_i64().is_some_and(|t| t >= 0 && t <= at)
                && matches!(v["kind"].as_str(), Some("heartbeat" | "error"))
                && v.get("message").is_none_or(Value::is_string)
        });
        let Some(value) = parsed else {
            invalid += 1;
            continue;
        };
        let timestamp = value["timestamp"].as_i64().unwrap();
        if let Some(previous) = last {
            if timestamp < previous {
                regressions += 1;
                invalid += 1;
                continue;
            }
            let gap = timestamp - previous;
            largest_gap = largest_gap.max(gap);
            gaps += u64::from(gap > allowed_gap);
            duplicate_times += u64::from(gap == 0);
        }
        first.get_or_insert(timestamp);
        last = Some(timestamp);
        valid += 1;
        if value["kind"] == "heartbeat" {
            heartbeat += 1;
        } else {
            errors += 1;
        }
    }
    let span = first.zip(last).map_or(0, |(a, b)| b - a);
    let age = last.map(|last| at - last);
    let fresh = age.is_some_and(|age| age <= allowed_gap);
    let duration_met = valid >= 2 && span >= minimum;
    let complete = duration_met && fresh && gaps == 0 && invalid == 0 && !incomplete;
    Ok(
        json!({"evaluated_at":at,"minimum_duration_seconds":minimum,"allowed_gap_seconds":allowed_gap,
        "first_timestamp":first,"last_timestamp":last,"observed_span_seconds":span,"latest_age_seconds":age,
        "valid_records":valid,"heartbeat_records":heartbeat,"error_records":errors,"invalid_records":invalid,
        "clock_regressions":regressions,"duplicate_timestamps":duplicate_times,"gap_count":gaps,
        "largest_between_record_gap_seconds":largest_gap,"incomplete_final_record":incomplete,
        "duration_met":duration_met,"latest_record_fresh":fresh,"coverage_complete":complete,
        "all_samples_healthy":valid>0 && errors==0 && invalid==0 && !incomplete,
        "scope":"Observed control-interface records only; coverage includes observed errors and is not continuous uptime or goal acceptance"}),
    )
}

pub fn report(path: &Path, minimum: i64, allowed_gap: i64) -> Result<Value> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let before = file.metadata()?;
    ensure!(
        before.is_file() && before.len() <= 64 * 1024 * 1024 + 16384,
        "observation file must be regular and within collector limit"
    );
    // Do not lock out the live collector: inspect exactly the prefix present at open.
    let mut bytes = Vec::with_capacity(before.len() as usize);
    (&mut file).take(before.len()).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 == before.len(),
        "observation file truncated while reading"
    );
    let digest = format!("{:x}", Sha256::digest(&bytes));
    file.seek(SeekFrom::Start(0))?;
    let mut second = Sha256::new();
    let mut remaining = before.len();
    let mut buffer = [0u8; 8192];
    while remaining > 0 {
        let requested = remaining.min(buffer.len() as u64) as usize;
        let count = file.read(&mut buffer[..requested])?;
        ensure!(count > 0, "observation file truncated during confirmation");
        second.update(&buffer[..count]);
        remaining -= count as u64;
    }
    let current = std::fs::symlink_metadata(path)?;
    ensure!(
        current.is_file()
            && current.dev() == before.dev()
            && current.ino() == before.ino()
            && current.len() >= before.len()
            && format!("{:x}", second.finalize()) == digest,
        "observation prefix changed during report; retry from current data"
    );
    let mut result = summarize(&bytes, telos_store::now(), minimum, allowed_gap)?;
    result["source"] = json!({"path":path,"device":before.dev(),"inode":before.ino(),"prefix_bytes":before.len(),"prefix_sha256":digest,
        "note":"Prefix was read twice consistently; appended suffix excluded. This is not authenticated evidence or a guarantee against arbitrary external rewrites."});
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn report_is_read_only_bounded_and_does_not_take_the_writers_lock() -> Result<()> {
        use std::os::fd::AsRawFd;
        let root = std::env::temp_dir().join(format!(
            "telos-report-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        std::fs::create_dir(&root)?;
        let path = root.join("observations.jsonl");
        let at = telos_store::now();
        let bytes = format!("{{\"timestamp\":{},\"kind\":\"heartbeat\"}}\n{{\"timestamp\":{},\"kind\":\"heartbeat\"}}\n",at-10,at).into_bytes();
        std::fs::write(&path, &bytes)?;
        let held = OpenOptions::new().read(true).write(true).open(&path)?;
        assert_eq!(
            unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        let result = report(&path, 10, 15)?;
        assert_eq!(result["source"]["prefix_bytes"], bytes.len() as u64);
        assert_eq!(
            result["source"]["prefix_sha256"],
            format!("{:x}", Sha256::digest(&bytes))
        );
        assert_eq!(result["coverage_complete"], true);
        assert_eq!(std::fs::read(&path)?, bytes);
        let link = root.join("link");
        std::os::unix::fs::symlink(&path, &link)?;
        assert!(report(&link, 10, 15).is_err());
        drop(held);
        std::fs::remove_dir_all(root)?;
        Ok(())
    }
    #[test]
    fn duration_coverage_and_health_are_separate() -> Result<()> {
        let bytes = b"{\"timestamp\":100,\"kind\":\"heartbeat\"}\n{\"timestamp\":105,\"kind\":\"error\"}\n{\"timestamp\":110,\"kind\":\"heartbeat\"}\n";
        let report = summarize(bytes, 111, 10, 5)?;
        assert_eq!(report["coverage_complete"], true);
        assert_eq!(report["all_samples_healthy"], false);
        assert_eq!(report["error_records"], 1);
        assert_eq!(summarize(bytes, 111, 1209600, 5)?["duration_met"], false);
        assert_eq!(summarize(bytes, 120, 10, 5)?["latest_record_fresh"], false);
        assert_eq!(summarize(bytes, 111, 10, 4)?["gap_count"], 2);
        Ok(())
    }
    #[test]
    fn malformed_future_reversed_partial_and_empty_data_never_prove_coverage() -> Result<()> {
        for bytes in [b"".as_slice(), b"{\"timestamp\":100,\"kind\":\"heartbeat\"}",
            b"{\"timestamp\":100,\"kind\":\"heartbeat\"}\n{\"timestamp\":110,\"kind\":\"heartbeat\"}\ngarbage\n",
            b"{\"timestamp\":100,\"kind\":\"heartbeat\"}\n{\"timestamp\":105,\"kind\":\"heartbeat\",\"extra\":true}\n",
            b"{\"timestamp\":100,\"kind\":\"heartbeat\"}\n{\"timestamp\":120,\"kind\":\"heartbeat\"}\n",
            b"{\"timestamp\":100,\"kind\":\"heartbeat\"}\n{\"timestamp\":99,\"kind\":\"heartbeat\"}\n{\"timestamp\":110,\"kind\":\"heartbeat\"}\n"] {
            assert_eq!(summarize(bytes, 111, 10, 20)?["coverage_complete"], false);
        }
        Ok(())
    }
}
