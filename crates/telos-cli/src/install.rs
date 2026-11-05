//! Explicit-path installation using Codex's public MCP configuration commands.
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::process::Command;

pub(crate) const SKILL: &str = include_str!("../../../skills/hsw-telos/SKILL.md");

async fn codex(program: &Path, home: &Path, args: &[String]) -> Result<std::process::Output> {
    let mut cmd = Command::new(program);
    cmd.args(args).env("CODEX_HOME", home).kill_on_drop(true);
    Ok(tokio::time::timeout(Duration::from_secs(20), cmd.output())
        .await
        .context("Codex configuration command timed out; inspect registration before retrying")??)
}

pub(crate) async fn registration(program: &Path, home: &Path) -> Result<Option<Value>> {
    let output = codex(
        program,
        home,
        &["mcp".into(), "get".into(), "telos".into(), "--json".into()],
    )
    .await?;
    if output.status.success() {
        return Ok(Some(serde_json::from_slice(&output.stdout)?));
    }
    ensure!(
        String::from_utf8_lossy(&output.stderr).trim()
            == "Error: No MCP server named 'telos' found.",
        "cannot safely inspect Telos MCP registration (exit {:?})",
        output.status.code()
    );
    Ok(None)
}

pub(crate) fn digest(path: &Path) -> Result<String> {
    Ok(format!("{:x}", Sha256::digest(std::fs::read(path)?)))
}

fn absolute_new(path: &Path) -> Result<PathBuf> {
    ensure!(path.is_absolute(), "installation paths must be absolute");
    ensure!(
        !path.try_exists()?,
        "installation destination already exists"
    );
    let parent = path
        .parent()
        .context("destination parent missing")?
        .canonicalize()?;
    Ok(parent.join(path.file_name().context("destination name missing")?))
}

pub async fn install(prefix: &Path, home: &Path, state: &Path, program: &Path) -> Result<Value> {
    let prefix = absolute_new(prefix)?;
    ensure!(
        home.is_absolute() && state.is_absolute(),
        "Codex home and state directory must be absolute"
    );
    let home = home
        .canonicalize()
        .context("Codex home must already exist")?;
    ensure!(home.is_dir(), "Codex home must be a directory");
    ensure!(
        registration(program, &home).await?.is_none(),
        "Telos MCP registration already exists; refusing overwrite"
    );
    let skill_dir = home.join("skills/hsw-telos");
    ensure!(
        !skill_dir.try_exists()?,
        "hsw-telos skill already exists; refusing overwrite"
    );
    std::fs::create_dir_all(state)?;
    let state = state.canonicalize()?;
    ensure!(
        !state.starts_with(&prefix) && !state.starts_with(&skill_dir),
        "state must be outside removable install paths"
    );
    std::fs::create_dir(&prefix)?;
    std::fs::set_permissions(&prefix, std::fs::Permissions::from_mode(0o700))?;
    let binary = prefix.join("telos");
    let skill_file = skill_dir.join("SKILL.md");
    let mut skill_created = false;
    let args = vec![
        "--state-dir".to_owned(),
        state.to_string_lossy().into_owned(),
        "mcp".into(),
    ];
    let result: Result<Value> = async {
        std::fs::copy(std::env::current_exe()?, &binary)?;
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700))?;
        std::fs::create_dir_all(home.join("skills"))?;
        std::fs::create_dir(&skill_dir)?;
        skill_created = true;
        std::fs::write(&skill_file, SKILL)?;
        let mut command = vec!["mcp".into(),"add".into(),"telos".into(),"--".into(),binary.to_string_lossy().into_owned()];
        command.extend(args.clone());
        let output = codex(program, &home, &command).await?;
        ensure!(output.status.success(), "Codex MCP registration failed (exit {:?})", output.status.code());
        let registration = registration(program, &home).await?.context("registration missing after installation")?;
        ensure!(matches_registration(&registration, &binary, &args), "installed registration differs from requested command");
        let manifest = json!({"format":1,"version":env!("CARGO_PKG_VERSION"),"codex_home":home,"state_dir":state,
            "binary_sha256":digest(&binary)?,"skill_sha256":digest(&skill_file)?,"registration":registration});
        std::fs::write(prefix.join("install.json"), serde_json::to_vec_pretty(&manifest)?)?;
        Ok(json!({"installed":true,"prefix":prefix,"state_dir":state,"codex_home":home,
            "daemon_started":false,"desktop_discovery":"not_verified",
            "daemon_command":[binary,"--state-dir",state,"daemon","--codex",program]}))
    }.await;
    if let Err(error) = result {
        // Remove only a registration that still matches our exact command.
        let clean = match registration(program, &home).await {
            Ok(Some(value)) if matches_registration(&value, &binary, &args) => codex(
                program,
                &home,
                &["mcp".into(), "remove".into(), "telos".into()],
            )
            .await
            .map(|v| v.status.success())
            .unwrap_or(false),
            Ok(None) => true,
            _ => false,
        };
        if clean {
            if skill_created {
                let _ = std::fs::remove_file(&skill_file);
                let _ = std::fs::remove_dir(&skill_dir);
            }
            let _ = std::fs::remove_file(&binary);
            let _ = std::fs::remove_file(prefix.join("install.json"));
            let _ = std::fs::remove_dir(&prefix);
        } else {
            return Err(error.context(
                "registration cleanup uncertain; installed files retained for inspection",
            ));
        }
        return Err(error);
    }
    result
}

fn matches_registration(value: &Value, binary: &Path, args: &[String]) -> bool {
    value["transport"]["command"].as_str() == binary.to_str()
        && value["transport"]["args"] == json!(args)
        && value["transport"]["type"] == "stdio"
}

pub async fn uninstall(prefix: &Path, home: &Path, program: &Path) -> Result<Value> {
    ensure!(
        prefix.is_absolute() && home.is_absolute(),
        "installation paths must be absolute"
    );
    let prefix = prefix.canonicalize()?;
    let home = home.canonicalize()?;
    let manifest: Value = serde_json::from_slice(&std::fs::read(prefix.join("install.json"))?)?;
    ensure!(
        manifest["format"] == 1 && manifest["codex_home"] == json!(home),
        "installation manifest mismatch"
    );
    let binary = prefix.join("telos");
    let skill_dir = home.join("skills/hsw-telos");
    let skill_file = skill_dir.join("SKILL.md");
    let state = Path::new(
        manifest["state_dir"]
            .as_str()
            .context("state path missing")?,
    );
    let _service_lock = crate::service::operation_lock(state)?;
    ensure!(
        !prefix.join("service.json").exists(),
        "remove launchd service before uninstalling"
    );
    let _lock = telos_runtime::daemon::maintenance_lock(state)
        .context("stop the Telos daemon before uninstalling")?;
    ensure!(
        !prefix.join("upgrade.json").exists(),
        "resume the pending upgrade before uninstalling"
    );
    ensure!(
        manifest["binary_sha256"] == digest(&binary)?
            && manifest["skill_sha256"] == digest(&skill_file)?,
        "installed files changed; refusing to remove user modifications"
    );
    let current = registration(program, &home).await?;
    if let Some(current) = current {
        ensure!(
            current == manifest["registration"],
            "MCP registration changed; refusing to remove it"
        );
        let output = codex(
            program,
            &home,
            &["mcp".into(), "remove".into(), "telos".into()],
        )
        .await?;
        ensure!(
            output.status.success(),
            "MCP removal failed; files retained"
        );
    }
    ensure!(
        registration(program, &home).await?.is_none(),
        "MCP registration still present"
    );
    std::fs::remove_file(skill_file)?;
    // Preserve any extra files the user placed in either directory.
    let _ = std::fs::remove_dir(skill_dir);
    std::fs::remove_file(binary)?;
    std::fs::remove_file(prefix.join("install.json"))?;
    let _ = std::fs::remove_dir(&prefix);
    Ok(
        json!({"uninstalled":true,"state_preserved":manifest["state_dir"],"note":"Uninstall removes integration files; it does not stop an already running daemon."}),
    )
}
