//! Separate owned LaunchAgent for control-plane observations, with no Codex access.
use crate::service::{atomic, ctl, query, read, render, text, verify_plist};
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::Duration,
};

fn config(directory: &Path, state: &Path, interval: u64, timeout: u64) -> Result<Value> {
    ensure!(
        (1..=3600).contains(&interval) && (1..=30).contains(&timeout),
        "invalid collector interval/timeout"
    );
    ensure!(
        state.is_absolute() && !state.starts_with(directory) && !directory.starts_with(state),
        "collector and monitored state must be separate"
    );
    let user_home =
        PathBuf::from(std::env::var_os("HOME").context("HOME missing")?).canonicalize()?;
    let uid = unsafe { libc::getuid() };
    let label = format!(
        "local.telos.collector.{:x}",
        Sha256::digest(directory.as_os_str().as_encoded_bytes())
    );
    let plist = user_home
        .join("Library/LaunchAgents")
        .join(format!("{label}.plist"));
    Ok(
        json!({"format":1,"label":label,"target":format!("gui/{uid}/{label}"),"domain":format!("gui/{uid}"),
        "plist":plist,"state_dir":directory,"monitored_state":state,"interval_seconds":interval,"timeout_seconds":timeout,
        "args":[directory.join("telos"),"--state-dir",state,"observe-service","--output",directory.join("observations.jsonl"),
            "--samples","1000000","--interval-seconds",interval.to_string(),"--timeout-seconds",timeout.to_string()],
        "environment":{},"stdout":directory.join("stdout.log"),"stderr":directory.join("stderr.log")}),
    )
}

fn root(directory: &Path) -> Result<PathBuf> {
    ensure!(
        cfg!(target_os = "macos"),
        "collector LaunchAgents require macOS"
    );
    ensure!(
        directory.is_absolute() && std::fs::symlink_metadata(directory)?.is_dir(),
        "collector directory must be absolute and not a symlink"
    );
    directory.canonicalize().map_err(Into::into)
}

fn load(directory: &Path) -> Result<Value> {
    let saved: Value = serde_json::from_slice(&read(&directory.join("collector.json"))?)?;
    ensure!(saved["format"] == 1, "unknown collector manifest");
    let cfg = &saved["config"];
    ensure!(
        *cfg == config(
            directory,
            Path::new(text(cfg, "monitored_state")?),
            cfg["interval_seconds"]
                .as_u64()
                .context("interval missing")?,
            cfg["timeout_seconds"].as_u64().context("timeout missing")?
        )?,
        "collector settings changed; preserving files"
    );
    ensure!(
        std::fs::symlink_metadata(directory.join("telos"))?.is_file(),
        "collector binary must be regular"
    );
    ensure!(
        crate::install::digest(&directory.join("telos"))? == saved["binary_sha256"],
        "collector binary changed; preserving files"
    );
    Ok(cfg.clone())
}

fn latest(directory: &Path) -> Result<(u64, Option<Value>)> {
    let path = directory.join("observations.jsonl");
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    ensure!(metadata.is_file(), "collector observations must be regular");
    let length = metadata.len();
    file.seek(SeekFrom::Start(length.saturating_sub(16384)))?;
    let mut bytes = Vec::new();
    file.take(16384).read_to_end(&mut bytes)?;
    let record = if bytes.last() == Some(&b'\n') {
        bytes[..bytes.len() - 1]
            .rsplit(|b| *b == b'\n')
            .next()
            .and_then(|line| serde_json::from_slice(line).ok())
    } else {
        None
    };
    Ok((length, record))
}

async fn health(directory: &Path, cfg: &Value) -> Result<Value> {
    let loaded = query(cfg).await?;
    let (bytes, observation) = latest(directory)?;
    let now = telos_store::now();
    let fresh = observation
        .as_ref()
        .and_then(|v| v["timestamp"].as_i64())
        .is_some_and(|at| {
            at <= now
                && now - at
                    <= (cfg["interval_seconds"].as_u64().unwrap()
                        + cfg["timeout_seconds"].as_u64().unwrap()
                        + 5) as i64
        });
    let running = loaded
        .as_ref()
        .is_some_and(|v| v["state"] == "running" && v["pid"].as_u64().is_some());
    Ok(
        json!({"configured":true,"target":cfg["target"],"plist":cfg["plist"],"launchd":loaded,"running":running,
        "fresh_observation":fresh,"observation":observation,"observed_bytes":bytes,
        "scope":"Collector process and last observation only; not a verified healthy window"}),
    )
}

pub async fn status(directory: &Path) -> Result<Value> {
    let directory = root(directory)?;
    let cfg = load(&directory)?;
    if Path::new(text(&cfg, "plist")?).try_exists()? {
        verify_plist(&cfg)?;
    }
    health(&directory, &cfg).await
}

pub fn report(directory: &Path, minimum: i64, max_gap: i64) -> Result<Value> {
    let directory = root(directory)?;
    load(&directory)?;
    crate::observation_report::report(&directory.join("observations.jsonl"), minimum, max_gap)
}

pub async fn install(directory: &Path, state: &Path, interval: u64, timeout: u64) -> Result<Value> {
    ensure!(
        cfg!(target_os = "macos"),
        "collector LaunchAgents require macOS"
    );
    ensure!(
        directory.is_absolute(),
        "collector directory must be absolute"
    );
    let state = state.canonicalize()?;
    let proposed = directory
        .parent()
        .context("collector parent missing")?
        .canonicalize()?
        .join(
            directory
                .file_name()
                .context("collector directory name missing")?,
        );
    // Reject overlap before creating any files in monitored state.
    config(&proposed, &state, interval, timeout)?;
    if !directory.try_exists()? {
        std::fs::create_dir(directory)?;
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))?;
    }
    let directory = root(directory)?;
    let desired = config(&directory, &state, interval, timeout)?;
    let owned = directory.join("collector.json").try_exists()?;
    if !owned {
        ensure!(
            std::fs::read_dir(&directory)?.next().is_none(),
            "new collector directory must be empty; partial files preserved"
        );
    }
    let _operation = crate::service::operation_lock(&directory)?;
    if owned {
        ensure!(
            load(&directory)? == desired,
            "collector settings differ; use a new directory"
        );
    }
    let loaded = query(&desired).await?;
    if loaded.is_some() {
        ensure!(
            owned,
            "collector label already registered without ownership"
        );
        verify_plist(&desired)?;
        return health(&directory, &desired).await;
    }
    let plist = Path::new(text(&desired, "plist")?);
    if plist.try_exists()? {
        ensure!(owned, "collector plist already exists");
        verify_plist(&desired)?;
    }
    if !owned {
        let binary = directory.join("telos");
        std::fs::copy(std::env::current_exe()?, &binary)?;
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700))?;
        File::open(&binary)?.sync_all()?;
        for name in ["observations.jsonl", "stdout.log", "stderr.log"] {
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(directory.join(name))?
                .sync_all()?;
        }
        atomic(
            &directory.join("collector.json"),
            &serde_json::to_vec_pretty(
                &json!({"format":1,"config":desired,"binary_sha256":crate::install::digest(&binary)?}),
            )?,
        )?;
    }
    for name in ["observations.jsonl", "stdout.log", "stderr.log"] {
        ensure!(
            std::fs::symlink_metadata(directory.join(name))?.is_file(),
            "collector output changed; preserving it"
        );
    }
    let writer =
        telos_runtime::service_observation::writer_lock(&directory.join("observations.jsonl"))?;
    let initial_bytes = latest(&directory)?.0;
    let parent = plist.parent().context("LaunchAgents parent missing")?;
    std::fs::create_dir_all(parent)?;
    ensure!(
        parent.canonicalize()? == parent,
        "LaunchAgents directory must not be symlinked"
    );
    if !plist.try_exists()? {
        atomic(plist, render(&desired)?.as_bytes())?;
    }
    drop(writer);
    let result = ctl(&[
        "bootstrap",
        text(&desired, "domain")?,
        text(&desired, "plist")?,
    ])
    .await?;
    ensure!(
        result.status.success(),
        "collector bootstrap failed; files preserved: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    for _ in 0..100 {
        let result = health(&directory, &desired).await?;
        if result["running"] == true
            && result["fresh_observation"] == true
            && result["observed_bytes"].as_u64().unwrap() > initial_bytes
        {
            return Ok(result);
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    anyhow::bail!("collector registered but new observation not confirmed; inspect status/logs")
}

pub async fn remove(directory: &Path) -> Result<Value> {
    let directory = root(directory)?;
    let _operation = crate::service::operation_lock(&directory)?;
    let cfg = load(&directory)?;
    let plist = Path::new(text(&cfg, "plist")?);
    if plist.try_exists()? {
        verify_plist(&cfg)?;
    }
    if query(&cfg).await?.is_some() {
        let result = ctl(&["bootout", text(&cfg, "target")?]).await?;
        ensure!(
            result.status.success(),
            "collector bootout failed; ownership preserved"
        );
    }
    for _ in 0..100 {
        if query(&cfg).await?.is_none() {
            if let Ok(_writer) = telos_runtime::service_observation::writer_lock(
                &directory.join("observations.jsonl"),
            ) {
                if plist.try_exists()? {
                    verify_plist(&cfg)?;
                    std::fs::remove_file(plist)?;
                    File::open(plist.parent().unwrap())?.sync_all()?;
                }
                return Ok(
                    json!({"removed":true,"target":cfg["target"],"observations_preserved":true,"installation_files_preserved":true}),
                );
            }
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    anyhow::bail!("collector stop not confirmed; files preserved")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn overlapping_state_is_rejected_without_creating_a_directory() -> Result<()> {
        let directory = std::env::temp_dir().join(format!(
            "telos-collector-overlap-{}-{}",
            std::process::id(),
            telos_store::now()
        ));
        std::fs::create_dir(&directory)?;
        let root = directory.canonicalize()?;
        let target = root.join("must-not-create");
        assert!(install(&target, &root, 5, 3).await.is_err());
        assert!(!target.exists());
        std::fs::remove_dir(directory)?;
        Ok(())
    }
    #[test]
    fn collector_config_has_no_codex_or_inherited_environment() -> Result<()> {
        let cfg = config(Path::new("/tmp/collector"), Path::new("/tmp/state"), 5, 3)?;
        assert_eq!(cfg["environment"], json!({}));
        assert_eq!(cfg["args"][3], "observe-service");
        assert!(cfg["label"]
            .as_str()
            .unwrap()
            .starts_with("local.telos.collector."));
        assert!(!render(&cfg)?.contains("CODEX_HOME"));
        assert!(config(Path::new("/tmp/state"), Path::new("/tmp/state/inner"), 5, 3).is_err());
        Ok(())
    }
}
