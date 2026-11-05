//! A real control-plane probe, not a worker or business-service health claim.
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::{
    fs::OpenOptions,
    io::{Read, Seek, SeekFrom, Write},
    os::unix::{
        fs::{MetadataExt, OpenOptionsExt},
        io::AsRawFd,
    },
    path::Path,
    time::Duration,
};

fn record(response: Option<&Value>, at: i64) -> Value {
    let healthy = response.is_some_and(|value| {
        value["service"] == "telos"
            && value["version"].as_str().is_some_and(|v| !v.is_empty())
            && value["pid"].as_u64().is_some_and(|pid| pid > 0)
            && value.get("mode").is_none()
    });
    json!({"timestamp":at,"kind":if healthy {"heartbeat"} else {"error"},
        "message":if healthy {"telos control interface responded; worker and business health untested"}
        else {"telos control interface unavailable or not in normal service mode"}})
}

fn append(output: &Path, observation: &Value) -> Result<()> {
    ensure!(output.is_absolute(), "observation output must be absolute");
    let parent = output
        .parent()
        .context("output parent missing")?
        .canonicalize()?;
    let path = parent.join(output.file_name().context("output name missing")?);
    let mut file = OpenOptions::new()
        .read(true)
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&path)?;
    let metadata = file.metadata()?;
    ensure!(metadata.is_file(), "observation output must be regular");
    ensure!(
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "observation output has another writer"
    );
    let _unlock = UnlockOnDrop(file.as_raw_fd());
    let current = std::fs::symlink_metadata(&path)?;
    ensure!(
        current.is_file() && current.dev() == metadata.dev() && current.ino() == metadata.ino(),
        "observation output changed while opening"
    );
    let length = file.metadata()?.len();
    ensure!(
        length < 64 * 1024 * 1024,
        "observation file limit reached; rotate explicitly"
    );
    if length > 0 {
        file.seek(SeekFrom::End(-1))?;
        let mut last = [0];
        file.read_exact(&mut last)?;
        ensure!(
            last[0] == b'\n',
            "observation file has an incomplete final record"
        );
    }
    let mut bytes = serde_json::to_vec(observation)?;
    bytes.push(b'\n');
    file.write_all(&bytes)?;
    file.sync_all()?;
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}

pub async fn observe(state: &Path, output: &Path, timeout_seconds: u64) -> Result<Value> {
    let _writer = writer_lock(output)?;
    observe_owned(state, output, timeout_seconds).await
}

struct UnlockOnDrop(std::os::fd::RawFd);
impl Drop for UnlockOnDrop {
    fn drop(&mut self) {
        unsafe { libc::flock(self.0, libc::LOCK_UN) };
    }
}

pub struct WriterLock(std::fs::File);
impl Drop for WriterLock {
    fn drop(&mut self) {
        // close alone can retain flock while a concurrent fork has inherited the
        // same open-file description, even when the descriptor is CLOEXEC.
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

pub fn writer_lock(output: &Path) -> Result<WriterLock> {
    ensure!(output.is_absolute(), "observation output must be absolute");
    let parent = output
        .parent()
        .context("output parent missing")?
        .canonicalize()?;
    let mut name = output
        .file_name()
        .context("output name missing")?
        .to_os_string();
    name.push(".collector.lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(parent.join(name))?;
    ensure!(file.metadata()?.is_file(), "collector lock must be regular");
    ensure!(
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "another collector owns this observation output"
    );
    Ok(WriterLock(file))
}

async fn observe_owned(state: &Path, output: &Path, timeout_seconds: u64) -> Result<Value> {
    ensure!(
        (1..=30).contains(&timeout_seconds),
        "probe timeout must be 1..30 seconds"
    );
    let response = tokio::time::timeout(
        Duration::from_secs(timeout_seconds),
        crate::daemon::call(state, &crate::daemon::Request::Ping),
    )
    .await
    .ok()
    .and_then(Result::ok);
    let observation = record(response.as_ref(), telos_store::now());
    append(output, &observation)?;
    Ok(
        json!({"recorded":true,"healthy":observation["kind"] == "heartbeat", "observation":observation,
        "scope":"Telos normal control interface reachability only; not worker or project health"}),
    )
}

/// Finite foreground collection. Delay after completion avoids catch-up heartbeats.
pub async fn collect(
    state: &Path,
    output: &Path,
    timeout_seconds: u64,
    interval_seconds: u64,
    samples: u64,
    mut stop: tokio::sync::watch::Receiver<bool>,
) -> Result<Value> {
    ensure!(
        (1..=1_000_000).contains(&samples),
        "samples must be 1..1000000"
    );
    ensure!(
        (1..=3600).contains(&interval_seconds),
        "interval must be 1..3600 seconds"
    );
    ensure!(
        (1..=30).contains(&timeout_seconds),
        "probe timeout must be 1..30 seconds"
    );
    let _writer = writer_lock(output)?;
    let mut recorded = 0;
    let mut healthy = 0;
    let mut last = Value::Null;
    while recorded < samples && !*stop.borrow() {
        let observation = tokio::select! {
            biased;
            _ = stop.wait_for(|value| *value) => break,
            result = observe_owned(state, output, timeout_seconds) => result?,
        };
        recorded += 1;
        healthy += u64::from(observation["healthy"] == true);
        last = observation;
        if recorded < samples {
            tokio::select! {
                biased;
                _ = stop.wait_for(|value| *value) => break,
                _ = tokio::time::sleep(Duration::from_secs(interval_seconds)) => {},
            }
        }
    }
    Ok(
        json!({"requested_samples":samples,"recorded_samples":recorded,
        "healthy_samples":healthy,"error_samples":recorded-healthy,
        "interrupted":recorded < samples,"last":last,
        "scope":"Actual control-interface samples only; window acceptance is separate"}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn owner_release_unlocks_even_while_an_inherited_description_remains_open() -> Result<()> {
        let root =
            std::env::temp_dir().join(format!("telos-inherited-lock-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root)?;
        let output = root.join("observations.jsonl");
        let held = writer_lock(&output)?;
        let inherited = held.0.try_clone()?;
        assert!(writer_lock(&output).is_err());
        drop(held);
        let next = writer_lock(&output)?;
        assert!(writer_lock(&output).is_err());
        drop(next);
        drop(inherited);
        std::fs::remove_dir_all(root)?;
        Ok(())
    }
    #[test]
    fn only_normal_complete_responses_attest_reachability() {
        let good = json!({"service":"telos","pid":1,"version":"0.1.0"});
        assert_eq!(record(Some(&good), 10)["kind"], "heartbeat");
        for response in [
            json!({}),
            json!({"service":"other","pid":1,"version":"x"}),
            json!({"service":"telos","pid":1,"version":"x","mode":"recovery_review"}),
            json!({"service":"telos","pid":0,"version":"x"}),
        ] {
            assert_eq!(record(Some(&response), 10)["kind"], "error");
        }
        assert_eq!(record(None, 10)["kind"], "error");
    }
    #[tokio::test]
    async fn finite_collection_delays_and_cancel_does_not_create_records() -> Result<()> {
        let root = std::env::temp_dir().join(format!("telos-collection-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root)?;
        let root = root.canonicalize()?;
        let state = root.join("offline-state");
        std::fs::create_dir(&state)?;
        let output = root.join("observations.jsonl");
        let (stop, cancel) = tokio::sync::watch::channel(false);
        let began = std::time::Instant::now();
        let result = collect(&state, &output, 1, 1, 2, cancel.clone()).await?;
        assert!(began.elapsed() >= Duration::from_secs(1));
        assert_eq!(result["recorded_samples"], 2);
        assert_eq!(result["error_samples"], 2);
        let bytes = std::fs::read(&output)?;
        let held = writer_lock(&output)?;
        assert!(observe(&state, &output, 1)
            .await
            .unwrap_err()
            .to_string()
            .contains("another collector"));
        assert!(collect(&state, &output, 1, 1, 2, cancel.clone())
            .await
            .unwrap_err()
            .to_string()
            .contains("another collector"));
        assert_eq!(std::fs::read(&output)?, bytes);
        drop(held);
        stop.send(true)?;
        let result = collect(&state, &output, 1, 1, 2, cancel).await?;
        assert_eq!(result["recorded_samples"], 0);
        assert_eq!(result["interrupted"], true);
        assert_eq!(std::fs::read(&output)?, bytes);
        std::fs::remove_dir_all(root)?;
        Ok(())
    }
    #[tokio::test]
    async fn live_probe_observes_shutdown_and_preserves_invalid_outputs() -> Result<()> {
        let root = std::env::temp_dir().join(format!(
            "telos-service-observation-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir(&root)?;
        let root = root.canonicalize()?;
        let state = root.join("state");
        let output = root.join("observations.jsonl");
        let (stop, shutdown) = tokio::sync::watch::channel(false);
        let service = tokio::spawn({
            let state = state.clone();
            async move {
                crate::daemon::serve(
                    &state,
                    crate::runner::CodexRunner {
                        program: "/no-worker".into(),
                    },
                    shutdown,
                )
                .await
            }
        });
        let outcome: Result<()> = async {
            tokio::time::timeout(Duration::from_secs(10), async {
                while crate::daemon::call(&state, &crate::daemon::Request::Ping)
                    .await
                    .is_err()
                {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await?;
            assert_eq!(observe(&state, &output, 2).await?["healthy"], true);
            let check = telos_core::LogWindowCheck {
                criterion_id: "control-reachability".into(),
                relative_path: "observations.jsonl".into(),
                window_seconds: 5,
                max_gap_seconds: 5,
            };
            let mut cursor = None;
            tokio::time::timeout(Duration::from_secs(15), async {
                loop {
                    observe(&state, &output, 2).await?;
                    let (next, verdict, _) = crate::log_window::sample(
                        &root,
                        &check,
                        cursor.take(),
                        telos_store::now(),
                    )?;
                    cursor = Some(next);
                    if verdict == telos_core::Verdict::Passed {
                        break;
                    }
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
                Ok::<(), anyhow::Error>(())
            })
            .await??;
            Ok(())
        }
        .await;
        let _ = stop.send(true);
        tokio::time::timeout(Duration::from_secs(10), service).await???;
        outcome?;
        assert_eq!(observe(&state, &output, 2).await?["healthy"], false);
        let records: Vec<Value> = std::fs::read_to_string(&output)?
            .lines()
            .map(serde_json::from_str)
            .collect::<std::result::Result<_, _>>()?;
        assert!(records.len() >= 7);
        assert_eq!(records[0]["kind"], "heartbeat");
        assert_eq!(records.last().unwrap()["kind"], "error");
        let check = telos_core::LogWindowCheck {
            criterion_id: "control-reachability".into(),
            relative_path: "observations.jsonl".into(),
            window_seconds: 30,
            // This final check replays the full captured history. Its initial
            // timestamp predates the five-second live sampling window; keeping
            // the live five-second poll gap here can reset the replay floor at
            // a second boundary and intentionally discard the preceding error.
            max_gap_seconds: 30,
        };
        let (_, verdict, _) = crate::log_window::sample(
            &root,
            &check,
            Some(crate::log_window::Cursor::fresh(
                records[0]["timestamp"].as_i64().unwrap(),
            )),
            telos_store::now(),
        )?;
        assert_eq!(verdict, telos_core::Verdict::Failed);
        let link = root.join("link");
        std::os::unix::fs::symlink(&output, &link)?;
        assert!(observe(&state, &link, 2).await.is_err());
        let partial = root.join("partial");
        std::fs::write(&partial, b"unfinished")?;
        assert!(observe(&state, &partial, 2).await.is_err());
        assert_eq!(std::fs::read(partial)?, b"unfinished");
        std::fs::remove_dir_all(root)?;
        Ok(())
    }
}
