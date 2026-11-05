//! Offline, resumable file upgrade. Does not migrate the database or edit MCP config.
use crate::install::{digest, registration, SKILL};
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::Write,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn regular(path: &Path) -> Result<()> {
    let m = std::fs::symlink_metadata(path)?;
    ensure!(
        m.is_file() && !m.file_type().is_symlink(),
        "upgrade requires regular files: {}",
        path.display()
    );
    Ok(())
}
fn sync_parent(path: &Path) -> Result<()> {
    File::open(path.parent().context("parent missing")?)?.sync_all()?;
    Ok(())
}
fn atomic(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    let tmp = path.with_file_name(format!(
        "{}.upgrade-tmp",
        path.file_name()
            .context("file name missing")?
            .to_string_lossy()
    ));
    if tmp.try_exists()? {
        regular(&tmp)?;
        std::fs::remove_file(&tmp)?;
    }
    let mut file = OpenOptions::new().create_new(true).write(true).open(&tmp)?;
    file.set_permissions(std::fs::Permissions::from_mode(mode))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::rename(&tmp, path)?;
    sync_parent(path)
}
fn read_json(path: &Path) -> Result<Value> {
    regular(path)?;
    ensure!(
        std::fs::metadata(path)?.len() <= 1024 * 1024,
        "upgrade metadata too large"
    );
    Ok(serde_json::from_slice(&std::fs::read(path)?)?)
}
// Destination, staging file, prior hash, target hash, target permissions.
type UpgradeEntry = (PathBuf, PathBuf, String, String, u32);

fn entries(prefix: &Path, home: &Path, journal: &Value) -> Result<Vec<UpgradeEntry>> {
    Ok(vec![
        (
            prefix.join("telos"),
            prefix.join("upgrade-binary"),
            journal["before"]["binary_sha256"]
                .as_str()
                .context("old binary hash missing")?
                .into(),
            journal["after"]["binary_sha256"]
                .as_str()
                .context("new binary hash missing")?
                .into(),
            0o700,
        ),
        (
            home.join("skills/hsw-telos/SKILL.md"),
            prefix.join("upgrade-skill"),
            journal["before"]["skill_sha256"]
                .as_str()
                .context("old skill hash missing")?
                .into(),
            journal["after"]["skill_sha256"]
                .as_str()
                .context("new skill hash missing")?
                .into(),
            0o600,
        ),
        (
            prefix.join("install.json"),
            prefix.join("upgrade-manifest"),
            journal["before_manifest_sha256"]
                .as_str()
                .context("old manifest hash missing")?
                .into(),
            hash(&serde_json::to_vec_pretty(&journal["after"])?),
            0o600,
        ),
    ])
}

fn finish(prefix: &Path, home: &Path, journal: &Value) -> Result<()> {
    let entries = entries(prefix, home, journal)?;
    // Validate every destination before replacing any. Mixed old/new is a recoverable state.
    for (path, stage, old, new, _) in &entries {
        regular(path)?;
        let actual = digest(path)?;
        ensure!(
            &actual == old || &actual == new,
            "installed file changed; upgrade preserves user edits: {}",
            path.display()
        );
        if stage.try_exists()? {
            regular(stage)?;
            ensure!(
                digest(stage)? == *new,
                "staged upgrade file changed: {}",
                stage.display()
            );
        }
    }
    for (path, stage, _, new, mode) in &entries {
        if digest(path)? != *new {
            regular(stage)
                .context("upgrade staging incomplete; rerun the original source build")?;
            atomic(path, &std::fs::read(stage)?, *mode)?;
        }
    }
    for (path, _, _, new, _) in &entries {
        ensure!(digest(path)? == *new, "upgrade verification failed");
    }
    for (_, stage, _, _, _) in &entries {
        if stage.try_exists()? {
            std::fs::remove_file(stage)?;
        }
    }
    // Journal is removed last. A crash during cleanup is resumed without recreating files.
    sync_parent(&prefix.join("upgrade.json"))?;
    std::fs::remove_file(prefix.join("upgrade.json"))?;
    sync_parent(&prefix.join("upgrade.json"))
}

pub async fn upgrade(prefix: &Path, home: &Path, program: &Path) -> Result<Value> {
    ensure!(
        prefix.is_absolute() && home.is_absolute(),
        "installation paths must be absolute"
    );
    let prefix = prefix.canonicalize()?;
    let home = home.canonicalize()?;
    let journal_path = prefix.join("upgrade.json");
    let resumed = journal_path.try_exists()?;
    let existing = if resumed {
        Some(read_json(&journal_path)?)
    } else {
        None
    };
    let before = match &existing {
        Some(j) => j["before"].clone(),
        None => read_json(&prefix.join("install.json"))?,
    };
    ensure!(
        before["format"] == 1 && before["codex_home"] == json!(home),
        "installation manifest mismatch"
    );
    let state = Path::new(before["state_dir"].as_str().context("state path missing")?);
    ensure!(
        state.is_absolute() && !state.starts_with(&prefix),
        "invalid state path"
    );
    let _service_lock = crate::service::operation_lock(state)?;
    ensure!(
        !prefix.join("service.json").exists(),
        "remove launchd service before upgrading"
    );
    let _lock = telos_runtime::daemon::maintenance_lock(state)
        .context("stop the Telos daemon before upgrading")?;
    ensure!(
        registration(program, &home).await? == Some(before["registration"].clone()),
        "MCP registration changed; upgrade refused"
    );
    let journal = if let Some(j) = existing {
        ensure!(
            j["format"] == 1
                && j["prefix"] == json!(prefix)
                && j["after"]["codex_home"] == json!(home)
                && j["after"]["state_dir"] == before["state_dir"]
                && j["after"]["registration"] == before["registration"],
            "upgrade journal mismatch"
        );
        j
    } else {
        let binary = prefix.join("telos");
        let skill = home.join("skills/hsw-telos/SKILL.md");
        regular(&binary)?;
        regular(&skill)?;
        ensure!(
            digest(&binary)? == before["binary_sha256"]
                && digest(&skill)? == before["skill_sha256"],
            "installed files changed; refusing to overwrite user modifications"
        );
        for name in ["upgrade-binary", "upgrade-skill", "upgrade-manifest"] {
            ensure!(
                !prefix.join(name).try_exists()?,
                "unowned upgrade staging file exists"
            );
        }
        let mut after = before.clone();
        after["version"] = json!(env!("CARGO_PKG_VERSION"));
        after["binary_sha256"] = json!(digest(&std::env::current_exe()?)?);
        after["skill_sha256"] = json!(hash(SKILL.as_bytes()));
        let j = json!({"format":1,"prefix":prefix,"before":before,"after":after,"before_manifest_sha256":digest(&prefix.join("install.json"))?});
        atomic(&journal_path, &serde_json::to_vec_pretty(&j)?, 0o600)?;
        j
    };
    let entries = entries(&prefix, &home, &journal)?;
    let all_installed = entries
        .iter()
        .all(|(p, _, _, new, _)| digest(p).is_ok_and(|h| h == *new));
    if !all_installed {
        for (index, (_, stage, _, new, mode)) in entries.iter().enumerate() {
            if stage.try_exists()? {
                continue;
            }
            let bytes = match index {
                0 => std::fs::read(std::env::current_exe()?)?,
                1 => SKILL.as_bytes().to_vec(),
                _ => serde_json::to_vec_pretty(&journal["after"])?,
            };
            ensure!(
                hash(&bytes) == *new,
                "pending upgrade requires its original source build to finish staging"
            );
            atomic(stage, &bytes, *mode)?;
        }
    }
    finish(&prefix, &home, &journal)?;
    Ok(
        json!({"upgraded":true,"resumed":resumed,"prefix":prefix,"state_dir":state,"binary_sha256":journal["after"]["binary_sha256"],"daemon_started":false,"database_migrated":false,"desktop_discovery":"not_verified"}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture(PathBuf);
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn setup() -> Result<(Fixture, PathBuf, PathBuf, Value)> {
        let root = std::env::temp_dir().join(format!(
            "telos-upgrade-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        let f = Fixture(root.clone());
        let prefix = root.join("install");
        let home = root.join("home");
        std::fs::create_dir_all(&prefix)?;
        std::fs::create_dir_all(home.join("skills/hsw-telos"))?;
        let before = json!({"binary_sha256":hash(b"old binary"),"skill_sha256":hash(b"old skill")});
        let after = json!({"binary_sha256":hash(b"new binary"),"skill_sha256":hash(b"new skill")});
        let journal = json!({"before":before,"after":after,"before_manifest_sha256":hash(&serde_json::to_vec_pretty(&before)?)});
        std::fs::write(prefix.join("telos"), b"old binary")?;
        std::fs::write(home.join("skills/hsw-telos/SKILL.md"), b"old skill")?;
        std::fs::write(
            prefix.join("install.json"),
            serde_json::to_vec_pretty(&before)?,
        )?;
        std::fs::write(prefix.join("upgrade.json"), serde_json::to_vec(&journal)?)?;
        std::fs::write(prefix.join("upgrade-binary"), b"new binary")?;
        std::fs::write(prefix.join("upgrade-skill"), b"new skill")?;
        std::fs::write(
            prefix.join("upgrade-manifest"),
            serde_json::to_vec_pretty(&after)?,
        )?;
        Ok((f, prefix, home, journal))
    }
    #[test]
    fn resume_after_each_replacement_and_during_cleanup() -> Result<()> {
        for completed in 0..=3 {
            let (_f, prefix, home, j) = setup()?;
            let entries = entries(&prefix, &home, &j)?;
            for (path, stage, _, _, mode) in entries.iter().take(completed) {
                atomic(path, &std::fs::read(stage)?, *mode)?;
            }
            if completed == 3 {
                std::fs::remove_file(prefix.join("upgrade-binary"))?;
            }
            finish(&prefix, &home, &j)?;
            for (path, _, _, new, _) in entries {
                assert_eq!(digest(&path)?, new);
            }
            assert!(!prefix.join("upgrade.json").exists());
        }
        Ok(())
    }
    #[test]
    fn edited_destinations_and_corrupt_staging_are_preserved() -> Result<()> {
        for staged in [false, true] {
            let (_f, prefix, home, j) = setup()?;
            let edited = if staged {
                prefix.join("upgrade-skill")
            } else {
                home.join("skills/hsw-telos/SKILL.md")
            };
            std::fs::write(&edited, b"user edit")?;
            assert!(finish(&prefix, &home, &j).is_err());
            assert_eq!(std::fs::read(&edited)?, b"user edit");
            assert_eq!(std::fs::read(prefix.join("telos"))?, b"old binary");
            assert!(prefix.join("upgrade.json").exists());
        }
        Ok(())
    }
    #[test]
    fn maintenance_requires_lock_even_without_a_listening_socket() -> Result<()> {
        let (f, _, _, _) = setup()?;
        let lock = telos_runtime::daemon::maintenance_lock(&f.0)?;
        assert!(telos_runtime::daemon::maintenance_lock(&f.0).is_err());
        drop(lock);
        let _lock = telos_runtime::daemon::maintenance_lock(&f.0)?;
        Ok(())
    }
}
