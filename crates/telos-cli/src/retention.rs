//! Explicit collection-level retention. Never prunes live ledger/evidence records.
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::unix::{fs::OpenOptionsExt, io::AsRawFd},
    path::Path,
};

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

async fn candidate(path: &Path) -> Result<Value> {
    ensure!(
        std::fs::symlink_metadata(path)?.is_dir(),
        "not a real directory"
    );
    let mut names = std::fs::read_dir(path)?
        .take(3)
        .map(|e| Ok(e?.file_name()))
        .collect::<Result<Vec<_>>>()?;
    names.sort();
    ensure!(
        names == ["manifest.json", "state"],
        "not an exclusive backup directory"
    );
    ensure!(
        std::fs::symlink_metadata(path.join("manifest.json"))?.is_file(),
        "manifest must be regular"
    );
    let mut bytes = Vec::new();
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path.join("manifest.json"))?
        .take(32 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 32 * 1024 * 1024, "manifest too large");
    let manifest: Value = serde_json::from_slice(&bytes)?;
    let source = manifest["source_state"]
        .as_str()
        .context("source state missing")?;
    ensure!(
        Path::new(source).is_absolute(),
        "source state is not absolute"
    );
    ensure!(
        !Path::new(source).starts_with(path),
        "backup overlaps its source state"
    );
    let created = manifest["created_at"]
        .as_i64()
        .context("creation time missing")?;
    ensure!(
        created > 0 && created <= telos_store::now(),
        "invalid creation time"
    );
    crate::backup::verify(path).await?;
    Ok(
        json!({"name":path.file_name().context("name missing")?.to_str().context("non-UTF8 name")?,
        "source_state":source,"created_at":created,"manifest_sha256":hash(&bytes)}),
    )
}

async fn plan(directory: &Path, keep: usize) -> Result<Value> {
    ensure!(
        (1..=10000).contains(&keep),
        "keep must be 1..10000 per source state"
    );
    let mut groups: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    let mut skipped = BTreeMap::new();
    let mut quarantines = Vec::new();
    let mut count = 0;
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        count += 1;
        ensure!(count <= 10000, "collection exceeds 10000 entries");
        let name = entry.file_name();
        let name = name.to_str().context("collection contains non-UTF8 name")?;
        if name.starts_with(".telos-retention-") {
            if name.ends_with(".quarantine") {
                // Report names only: never follow or inspect interrupted payloads.
                quarantines.push(name.to_owned());
            }
            continue;
        }
        match candidate(&entry.path()).await {
            Ok(value) => groups
                .entry(value["source_state"].as_str().unwrap().into())
                .or_default()
                .push(value),
            Err(error) => {
                skipped.insert(name.to_owned(), error.to_string());
            }
        }
    }
    let mut retained = Vec::new();
    quarantines.sort();
    let mut remove = Vec::new();
    for mut backups in groups.into_values() {
        backups.sort_by(|a, b| {
            b["created_at"]
                .as_i64()
                .cmp(&a["created_at"].as_i64())
                .then_with(|| a["name"].as_str().cmp(&b["name"].as_str()))
        });
        for (i, backup) in backups.into_iter().enumerate() {
            if i < keep {
                retained.push(backup);
            } else {
                remove.push(backup);
            }
        }
    }
    Ok(
        json!({"format":1,"directory":directory,"keep_per_source":keep,"retained":retained,"remove":remove,"skipped":skipped,"quarantines_requiring_review":quarantines}),
    )
}

fn write_new(path: &Path, value: &Value) -> Result<()> {
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(&serde_json::to_vec_pretty(value)?)?;
    file.sync_all()?;
    File::open(path.parent().context("parent missing")?)?.sync_all()?;
    Ok(())
}

pub async fn prune(directory: &Path, keep: usize, expected: Option<&str>) -> Result<Value> {
    ensure!(
        directory.is_absolute(),
        "collection directory must be absolute"
    );
    ensure!(
        std::fs::symlink_metadata(directory)?.is_dir(),
        "collection must not be a symlink"
    );
    let directory = directory.canonicalize()?;
    // Apply processes serialize; previews are read-only and deliberately create no lock.
    let _lock = if expected.is_some() {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(directory.join(".telos-retention-lock"))?;
        ensure!(
            file.metadata()?.is_file(),
            "retention lock is not a regular file"
        );
        ensure!(
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
            "another retention operation is active"
        );
        Some(file)
    } else {
        None
    };
    let plan = plan(&directory, keep).await?;
    let digest = hash(&serde_json::to_vec(&plan)?);
    if let Some(expected) = expected {
        ensure!(
            expected == digest,
            "retention plan changed; inspect a fresh preview"
        );
        ensure!(
            plan["quarantines_requiring_review"]
                .as_array()
                .unwrap()
                .is_empty(),
            "unfinished retention quarantine requires operator review before further cleanup"
        );
        let operation = format!(
            ".telos-retention-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        );
        // A durable intent precedes any rename/delete; incomplete quarantine is never
        // automatically retried. Other writers must be stopped by the operator.
        write_new(
            &directory.join(format!("{operation}.intent.json")),
            &json!({"plan_sha256":digest,"plan":plan}),
        )?;
        let mut deleted = Vec::new();
        for (index, item) in plan["remove"].as_array().unwrap().iter().enumerate() {
            for retained in plan["retained"].as_array().unwrap() {
                ensure!(
                    candidate(&directory.join(retained["name"].as_str().unwrap())).await?
                        == *retained,
                    "retained backup changed; refusing further deletion"
                );
            }
            let original = directory.join(item["name"].as_str().unwrap());
            ensure!(
                candidate(&original).await? == *item,
                "backup changed before quarantine"
            );
            let quarantine = directory.join(format!("{operation}-{index}.quarantine"));
            crate::backup::publish_exclusive(&original, &quarantine)?;
            File::open(&directory)?.sync_all()?;
            let mut quarantined = candidate(&quarantine).await?;
            quarantined["name"] = item["name"].clone();
            ensure!(
                quarantined == *item,
                "quarantined backup changed; retained for inspection"
            );
            std::fs::remove_dir_all(&quarantine)?;
            File::open(&directory)?.sync_all()?;
            deleted.push(item["name"].clone());
            write_new(
                &directory.join(format!("{operation}-{index}.deleted.json")),
                &json!({"backup":item,"plan_sha256":digest}),
            )?;
        }
        write_new(
            &directory.join(format!("{operation}.complete.json")),
            &json!({"plan_sha256":digest,"deleted":deleted}),
        )?;
        return Ok(
            json!({"applied":true,"plan_sha256":digest,"deleted":deleted,"audit_prefix":operation,"plan":plan}),
        );
    }
    Ok(json!({"applied":false,"plan_sha256":digest,"plan":plan}))
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture(std::path::PathBuf);
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    #[tokio::test]
    async fn retention_pins_verified_inventory_and_preserves_each_source() -> Result<()> {
        let f = Fixture(std::env::temp_dir().join(
            format!("telos-retention-test-{}-{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos()),
        ));
        std::fs::create_dir(&f.0)?;
        let root = f.0.canonicalize()?;
        let collection = root.join("backups");
        std::fs::create_dir(&collection)?;
        let source = root.join("source");
        let other = root.join("other-source");
        for state in [&source, &other] {
            let store = telos_store::Store::open(state).await?;
            store.close().await;
        }
        let external = root.join("external");
        std::fs::create_dir(&external)?;
        std::fs::write(external.join("precious"), "outside backup")?;
        std::fs::create_dir_all(source.join("workspaces/fixture"))?;
        std::os::unix::fs::symlink(&external, source.join("workspaces/fixture/link"))?;
        for (name, state, age) in [
            ("old", &source, 100),
            ("new", &source, 1),
            ("other", &other, 100),
        ] {
            let path = collection.join(name);
            crate::backup::create(state, &path).await?;
            let mut manifest: Value =
                serde_json::from_slice(&std::fs::read(path.join("manifest.json"))?)?;
            manifest["created_at"] = json!(telos_store::now() - age);
            std::fs::write(path.join("manifest.json"), serde_json::to_vec(&manifest)?)?;
        }
        // A backup-like directory containing unrelated data must never be pruned.
        crate::backup::create(&source, &collection.join("extra")).await?;
        std::fs::write(collection.join("extra/personal.txt"), "retain me")?;
        crate::backup::create(&source, &collection.join("broken")).await?;
        std::fs::write(
            collection.join("broken/state/unlisted"),
            "retain invalid backup",
        )?;
        std::os::unix::fs::symlink(&source, collection.join("linked"))?;
        assert!(prune(&collection, 0, None).await.is_err());
        let preview = prune(&collection, 1, None).await?;
        assert_eq!(preview["applied"], false);
        assert_eq!(preview["plan"]["retained"].as_array().unwrap().len(), 2);
        assert_eq!(preview["plan"]["remove"].as_array().unwrap().len(), 1);
        assert_eq!(preview["plan"]["remove"][0]["name"], "old");
        assert_eq!(preview["plan"]["skipped"].as_object().unwrap().len(), 3);
        assert!(!collection.join(".telos-retention-lock").exists());
        let expected = preview["plan_sha256"].as_str().unwrap();
        let lock = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(collection.join(".telos-retention-lock"))?;
        assert_eq!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        assert!(prune(&collection, 1, Some(expected))
            .await
            .unwrap_err()
            .to_string()
            .contains("another retention operation"));
        assert!(collection.join("old/state/telos.db").exists());
        drop(lock);
        // Any inventory change rejects the pinned preview before deleting anything.
        std::fs::write(collection.join("old/state/unlisted"), "changed")?;
        assert!(prune(&collection, 1, Some(expected)).await.is_err());
        assert!(collection.join("old/state/telos.db").exists());
        std::fs::remove_file(collection.join("old/state/unlisted"))?;
        let result = prune(&collection, 1, Some(expected)).await?;
        assert_eq!(result["deleted"], json!(["old"]));
        assert!(!collection.join("old").exists());
        for name in ["new", "other"] {
            crate::backup::verify(&collection.join(name)).await?;
        }
        assert_eq!(
            std::fs::read_to_string(collection.join("extra/personal.txt"))?,
            "retain me"
        );
        assert!(collection.join("broken/state/unlisted").exists());
        assert!(source.join("telos.db").exists());
        assert_eq!(
            std::fs::read_to_string(external.join("precious"))?,
            "outside backup"
        );
        let audit = result["audit_prefix"].as_str().unwrap();
        assert!(collection.join(format!("{audit}.intent.json")).exists());
        assert!(collection.join(format!("{audit}.complete.json")).exists());
        assert!(collection.join(format!("{audit}-0.deleted.json")).exists());
        // Unfinished quarantine is outside automatic discovery/retry.
        let interrupted = collection.join(".telos-retention-interrupted.quarantine");
        std::fs::create_dir(&interrupted)?;
        std::fs::write(interrupted.join("preserve"), "uncertain")?;
        let again = prune(&collection, 1, None).await?;
        assert!(again["plan"]["remove"].as_array().unwrap().is_empty());
        assert_eq!(
            again["plan"]["quarantines_requiring_review"],
            json!([".telos-retention-interrupted.quarantine"])
        );
        assert!(prune(&collection, 1, again["plan_sha256"].as_str())
            .await
            .unwrap_err()
            .to_string()
            .contains("requires operator review"));
        // A symlink in this namespace must be reported without following it.
        std::os::unix::fs::symlink(
            root.join("missing-target"),
            collection.join(".telos-retention-link.quarantine"),
        )?;
        let linked = prune(&collection, 1, None).await?;
        assert_eq!(
            linked["plan"]["quarantines_requiring_review"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert!(interrupted.join("preserve").exists());
        Ok(())
    }
}
