//! macOS user LaunchAgent lifecycle for an existing owned installation.
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    time::Duration,
};
use telos_runtime::daemon::{self, Request};
use tokio::process::Command;

pub(crate) fn operation_lock(state: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(state.join("service.lock"))?;
    // SAFETY: flock acts only on this owned open file descriptor.
    ensure!(
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "another service lifecycle operation is active"
    );
    Ok(file)
}
pub(crate) fn read(path: &Path) -> Result<Vec<u8>> {
    let f = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    ensure!(f.metadata()?.is_file(), "service metadata must be regular");
    let mut bytes = Vec::new();
    f.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 1024 * 1024, "service metadata too large");
    Ok(bytes)
}
fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
pub(crate) fn text<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    v[key]
        .as_str()
        .with_context(|| format!("service field missing: {key}"))
}
fn xml(s: &str) -> Result<String> {
    ensure!(
        s.chars().all(|c| !c.is_control()),
        "control character in service setting"
    );
    Ok(s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;"))
}
pub(crate) fn render(config: &Value) -> Result<String> {
    let args = config["args"]
        .as_array()
        .context("service args missing")?
        .iter()
        .map(|v| {
            Ok(format!(
                "<string>{}</string>",
                xml(v.as_str().context("invalid argument")?)?
            ))
        })
        .collect::<Result<Vec<_>>>()?
        .join("");
    let mut env = String::new();
    for (key, value) in config["environment"]
        .as_object()
        .context("service environment missing")?
    {
        env.push_str(&format!(
            "<key>{}</key><string>{}</string>",
            xml(key)?,
            xml(value.as_str().context("invalid environment value")?)?
        ));
    }
    Ok(format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict><key>Label</key><string>{}</string><key>ProgramArguments</key><array>{args}</array><key>WorkingDirectory</key><string>{}</string><key>EnvironmentVariables</key><dict>{env}</dict><key>RunAtLoad</key><true/><key>KeepAlive</key><true/><key>ThrottleInterval</key><integer>10</integer><key>ExitTimeOut</key><integer>45</integer><key>ProcessType</key><string>Background</string><key>StandardOutPath</key><string>{}</string><key>StandardErrorPath</key><string>{}</string></dict></plist>\n",xml(text(config,"label")?)?,xml(text(config,"state_dir")?)?,xml(text(config,"stdout")?)?,xml(text(config,"stderr")?)?))
}
pub(crate) fn atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("telos-pending");
    if tmp.try_exists()? {
        read(&tmp)?;
        std::fs::remove_file(&tmp)?;
    }
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)?;
    File::open(path.parent().context("parent missing")?)?.sync_all()?;
    Ok(())
}
fn installation(prefix: &Path) -> Result<(PathBuf, Value)> {
    ensure!(prefix.is_absolute(), "installation prefix must be absolute");
    let prefix = prefix.canonicalize()?;
    let manifest: Value = serde_json::from_slice(&read(&prefix.join("install.json"))?)?;
    ensure!(manifest["format"] == 1, "invalid installation format");
    ensure!(
        !prefix.join("upgrade.json").exists(),
        "finish pending upgrade before service management"
    );
    let state = Path::new(text(&manifest, "state_dir")?);
    ensure!(
        state.is_absolute() && state.canonicalize()? == state && !state.starts_with(&prefix),
        "invalid service state root"
    );
    ensure!(
        crate::install::digest(&prefix.join("telos"))? == manifest["binary_sha256"],
        "installed binary changed"
    );
    Ok((prefix, manifest))
}
fn config(prefix: &Path, manifest: &Value, codex: &Path, notifications: bool) -> Result<Value> {
    ensure!(codex.is_absolute(), "Codex executable must be absolute");
    let codex = codex.canonicalize()?;
    ensure!(codex.is_file(), "Codex executable missing");
    let home = PathBuf::from(std::env::var_os("HOME").context("HOME missing")?).canonicalize()?;
    let state = PathBuf::from(text(manifest, "state_dir")?);
    let label = format!(
        "local.telos.{}",
        &hash(state.as_os_str().as_encoded_bytes())[..16]
    );
    // SAFETY: getuid has no arguments or memory preconditions.
    let uid = unsafe { libc::getuid() };
    let plist = home
        .join("Library/LaunchAgents")
        .join(format!("{label}.plist"));
    let mut args = vec![
        json!(prefix.join("telos")),
        json!("--state-dir"),
        json!(state),
        json!("daemon"),
        json!("--codex"),
        json!(codex),
    ];
    if notifications {
        args.push(json!("--notifications"));
    }
    Ok(
        json!({"format":1,"label":label,"target":format!("gui/{uid}/{label}"),"domain":format!("gui/{uid}"),"plist":plist,"state_dir":state,"args":args,"environment":{"HOME":home,"CODEX_HOME":manifest["codex_home"],"PATH":"/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin"},"stdout":state.join("service-logs/stdout.log"),"stderr":state.join("service-logs/stderr.log")}),
    )
}
fn load(prefix: &Path, manifest: &Value) -> Result<Value> {
    let saved: Value = serde_json::from_slice(&read(&prefix.join("service.json"))?)?;
    let args = saved["args"]
        .as_array()
        .context("saved service args missing")?;
    ensure!(
        args.len() == 6 || args.len() == 7,
        "invalid saved service args"
    );
    let codex = Path::new(args[5].as_str().context("saved Codex path missing")?);
    let expected = config(prefix, manifest, codex, args.len() == 7)?;
    ensure!(
        saved == expected,
        "service manifest changed; refusing lifecycle action"
    );
    Ok(saved)
}
pub(crate) async fn ctl(args: &[&str]) -> Result<std::process::Output> {
    let out = tokio::time::timeout(
        Duration::from_secs(10),
        Command::new("/bin/launchctl")
            .args(args)
            .env("LC_ALL", "C")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("launchctl timed out; inspect state before retrying")??;
    ensure!(
        out.stdout.len() + out.stderr.len() <= 128 * 1024,
        "launchctl response too large"
    );
    Ok(out)
}
pub(crate) async fn query(config: &Value) -> Result<Option<Value>> {
    let output = ctl(&["print", text(config, "target")?]).await?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        ensure!(
            output.status.code() == Some(113)
                && stderr.contains(&format!(
                    "Could not find service \"{}\"",
                    text(config, "label")?
                )),
            "cannot establish launchd state: {stderr}"
        );
        return Ok(None);
    }
    let stdout = String::from_utf8(output.stdout)?;
    let field = |key: &str| {
        stdout
            .lines()
            .map(str::trim)
            .find_map(|line| line.strip_prefix(&format!("{key} = ")))
            .map(str::to_owned)
    };
    ensure!(
        field("path").as_deref() == Some(text(config, "plist")?),
        "loaded service belongs to a different plist"
    );
    ensure!(
        field("program").as_deref() == config["args"][0].as_str(),
        "loaded service program changed"
    );
    Ok(Some(
        json!({"state":field("state"),"pid":field("pid").and_then(|p|p.parse::<u32>().ok()),"last_exit_code":field("last exit code")}),
    ))
}
pub(crate) fn verify_plist(config: &Value) -> Result<()> {
    let bytes = read(Path::new(text(config, "plist")?))?;
    ensure!(
        bytes == render(config)?.as_bytes(),
        "LaunchAgent was modified; preserving it"
    );
    Ok(())
}
async fn health(config: &Value) -> Result<Value> {
    let loaded = query(config).await?;
    let ping = tokio::time::timeout(
        Duration::from_secs(2),
        daemon::call(Path::new(text(config, "state_dir")?), &Request::Ping),
    )
    .await
    .ok()
    .and_then(Result::ok);
    let healthy = loaded
        .as_ref()
        .and_then(|v| v["pid"].as_u64())
        .is_some_and(|pid| ping.as_ref().and_then(|v| v["pid"].as_u64()) == Some(pid));
    Ok(
        json!({"configured":true,"target":config["target"],"plist":config["plist"],"launchd":loaded,"daemon":ping,"healthy":healthy}),
    )
}
pub async fn status(prefix: &Path) -> Result<Value> {
    ensure!(
        cfg!(target_os = "macos"),
        "LaunchAgent services require macOS"
    );
    let (prefix, manifest) = installation(prefix)?;
    if !prefix.join("service.json").exists() {
        return Ok(json!({"configured":false}));
    }
    let saved = load(&prefix, &manifest)?;
    verify_plist(&saved)?;
    health(&saved).await
}
pub async fn install(prefix: &Path, codex: &Path, notifications: bool) -> Result<Value> {
    ensure!(
        cfg!(target_os = "macos"),
        "LaunchAgent services require macOS"
    );
    let (prefix, manifest) = installation(prefix)?;
    let state = Path::new(text(&manifest, "state_dir")?);
    let _operation = operation_lock(state)?;
    let desired = config(&prefix, &manifest, codex, notifications)?;
    let owned = prefix.join("service.json").exists();
    if owned {
        ensure!(
            load(&prefix, &manifest)? == desired,
            "service settings differ; remove existing service first"
        );
    }
    let loaded = query(&desired).await?;
    if loaded.is_some() {
        ensure!(
            owned,
            "service label is already loaded without owned metadata"
        );
        verify_plist(&desired)?;
        return health(&desired).await;
    }
    let lock = daemon::maintenance_lock(state)
        .context("stop foreground daemon before installing service")?;
    let plist = Path::new(text(&desired, "plist")?);
    if plist.exists() {
        ensure!(owned, "LaunchAgent path already exists");
        verify_plist(&desired)?;
    }
    let parent = plist.parent().context("agent directory missing")?;
    std::fs::create_dir_all(parent)?;
    ensure!(
        parent.canonicalize()? == parent,
        "LaunchAgents directory must not be symlinked"
    );
    let logs = state.join("service-logs");
    std::fs::create_dir_all(&logs)?;
    ensure!(
        logs.canonicalize()? == logs,
        "service log directory must not be symlinked"
    );
    std::fs::set_permissions(&logs, std::fs::Permissions::from_mode(0o700))?;
    for name in ["stdout.log", "stderr.log"] {
        let file = OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(logs.join(name))?;
        ensure!(file.metadata()?.is_file(), "service log must be regular");
    }
    // Persist ownership before registering with launchd; failures remain inspectable.
    if !owned {
        atomic(
            &prefix.join("service.json"),
            &serde_json::to_vec_pretty(&desired)?,
        )?;
    }
    if !plist.exists() {
        atomic(plist, render(&desired)?.as_bytes())?;
    }
    drop(lock);
    let out = ctl(&[
        "bootstrap",
        text(&desired, "domain")?,
        text(&desired, "plist")?,
    ])
    .await?;
    ensure!(
        out.status.success(),
        "launchd bootstrap failed; registration/files preserved for inspection: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let status = health(&desired).await?;
        if status["healthy"] == true {
            return Ok(status);
        }
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    anyhow::bail!("service registered but health not confirmed; inspect service status and logs")
}
pub async fn remove(prefix: &Path) -> Result<Value> {
    ensure!(
        cfg!(target_os = "macos"),
        "LaunchAgent services require macOS"
    );
    let (prefix, manifest) = installation(prefix)?;
    let state = Path::new(text(&manifest, "state_dir")?);
    let _operation = operation_lock(state)?;
    if !prefix.join("service.json").exists() {
        return Ok(json!({"configured":false,"removed":false}));
    }
    let saved = load(&prefix, &manifest)?;
    let plist = Path::new(text(&saved, "plist")?);
    if plist.exists() {
        verify_plist(&saved)?;
    }
    if query(&saved).await?.is_some() {
        let out = ctl(&["bootout", text(&saved, "target")?]).await?;
        ensure!(
            out.status.success(),
            "launchd bootout failed; ownership preserved: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    for _ in 0..150 {
        if query(&saved).await?.is_none() {
            if let Ok(_lock) = daemon::maintenance_lock(state) {
                if plist.exists() {
                    verify_plist(&saved)?;
                    std::fs::remove_file(plist)?;
                    File::open(plist.parent().unwrap())?.sync_all()?;
                }
                std::fs::remove_file(prefix.join("service.json"))?;
                File::open(&prefix)?.sync_all()?;
                return Ok(
                    json!({"removed":true,"state_preserved":true,"logs_preserved":true,"target":saved["target"]}),
                );
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    anyhow::bail!("service stop not confirmed; ownership and data preserved")
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "telos-launchd-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&root).unwrap();
            Self(root.canonicalize().unwrap())
        }
        fn bundle(&self) -> Result<(PathBuf, Value)> {
            let prefix = self.0.join("install & <fixture>");
            let state = self.0.join("state");
            let home = self.0.join("codex-home");
            for p in [&prefix, &state, &home] {
                std::fs::create_dir(p)?;
            }
            std::fs::write(prefix.join("telos"), b"fixture executable")?;
            let manifest = json!({"format":1,"state_dir":state,"codex_home":home,"binary_sha256":crate::install::digest(&prefix.join("telos"))?});
            std::fs::write(prefix.join("install.json"), serde_json::to_vec(&manifest)?)?;
            Ok((prefix, manifest))
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn plist_roundtrips_literal_arguments_and_contains_no_inherited_credentials() -> Result<()> {
        let f = Fixture::new();
        let (prefix, manifest) = f.bundle()?;
        let cfg = config(&prefix, &manifest, Path::new("/bin/echo"), true)?;
        let plist = f.0.join("fixture.plist");
        std::fs::write(&plist, render(&cfg)?)?;
        let parsed = std::process::Command::new("/usr/bin/plutil")
            .args(["-convert", "json", "-o", "-"])
            .arg(&plist)
            .output()?;
        ensure!(
            parsed.status.success(),
            "plist parse failed: {}",
            String::from_utf8_lossy(&parsed.stderr)
        );
        let data: Value = serde_json::from_slice(&parsed.stdout)?;
        assert_eq!(data["ProgramArguments"], cfg["args"]);
        assert_eq!(data["KeepAlive"], true);
        assert_eq!(data["RunAtLoad"], true);
        let env = data["EnvironmentVariables"].as_object().unwrap();
        assert_eq!(env.len(), 3);
        assert!(
            env.contains_key("HOME") && env.contains_key("CODEX_HOME") && env.contains_key("PATH")
        );
        assert!(!render(&cfg)?.contains("<fixture>"));
        assert!(xml("bad\nvalue").is_err());
        Ok(())
    }
    #[test]
    fn ownership_checks_preserve_modified_plists_and_reject_symlink_metadata() -> Result<()> {
        let f = Fixture::new();
        let (prefix, manifest) = f.bundle()?;
        let mut cfg = config(&prefix, &manifest, Path::new("/bin/echo"), false)?;
        cfg["plist"] = json!(f.0.join("fixture.plist"));
        let path = Path::new(text(&cfg, "plist")?);
        atomic(path, render(&cfg)?.as_bytes())?;
        verify_plist(&cfg)?;
        std::fs::write(path, b"user edited")?;
        assert!(verify_plist(&cfg).is_err());
        assert_eq!(std::fs::read(path)?, b"user edited");
        let link = f.0.join("link");
        std::os::unix::fs::symlink(path, &link)?;
        assert!(read(&link).is_err());
        Ok(())
    }
    #[tokio::test]
    async fn registered_service_blocks_upgrade_and_bundle_removal() -> Result<()> {
        let f = Fixture::new();
        let (prefix, manifest) = f.bundle()?;
        std::fs::write(prefix.join("service.json"), b"owned service marker")?;
        let home = Path::new(text(&manifest, "codex_home")?);
        let upgrade = crate::upgrade::upgrade(&prefix, home, Path::new("/bin/false"))
            .await
            .unwrap_err();
        assert!(upgrade.to_string().contains("remove launchd service"));
        let uninstall = crate::install::uninstall(&prefix, home, Path::new("/bin/false"))
            .await
            .unwrap_err();
        assert!(uninstall.to_string().contains("remove launchd service"));
        assert!(prefix.join("telos").is_file());
        assert!(prefix.join("install.json").is_file());
        Ok(())
    }
    #[test]
    fn lifecycle_operations_are_exclusive() -> Result<()> {
        let f = Fixture::new();
        let one = operation_lock(&f.0)?;
        assert!(operation_lock(&f.0).is_err());
        drop(one);
        let _two = operation_lock(&f.0)?;
        Ok(())
    }
}
