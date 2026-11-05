//! Process identity tracking for Codex and independently grouped MCP children.
//! A worktree is not an OS containment boundary. The runtime reports any cleanup
//! failure and never reuses a numeric PID without checking its recorded identity.
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path, time::Duration};
use tokio::process::Command;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Identity {
    pub pid: i32,
    pub parent: i32,
    pub started: String,
    pub state: String,
}

pub async fn snapshot() -> Result<BTreeMap<i32, Identity>> {
    let mut cmd = Command::new("ps");
    cmd.args(["-axo", "pid=,ppid=,stat=,lstart="])
        .env("LC_ALL", "C")
        .kill_on_drop(true);
    let out = tokio::time::timeout(Duration::from_secs(3), cmd.output())
        .await
        .context("process inspection timed out")??;
    ensure!(
        out.status.success(),
        "cannot inspect processes; refusing unmanaged execution"
    );
    let mut rows = BTreeMap::new();
    for line in String::from_utf8(out.stdout)?.lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() >= 8 {
            let pid = fields[0].parse()?;
            rows.insert(
                pid,
                Identity {
                    pid,
                    parent: fields[1].parse()?,
                    state: fields[2].into(),
                    started: fields[3..].join(" "),
                },
            );
        }
    }
    Ok(rows)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessSet {
    pub leader: i32,
    pub identities: BTreeMap<i32, Identity>,
}

impl ProcessSet {
    pub fn new(leader: i32) -> Self {
        Self {
            leader,
            identities: BTreeMap::new(),
        }
    }

    pub fn observe(&mut self, table: &BTreeMap<i32, Identity>) {
        if self.identities.is_empty() {
            if let Some(identity) = table.get(&self.leader) {
                self.identities.insert(self.leader, identity.clone());
            }
        }
        loop {
            let mut added = false;
            for (pid, row) in table {
                // Only a still-live, identity-matching parent can establish ownership.
                let owns_parent = self.identities.get(&row.parent).is_some_and(|saved| {
                    table.get(&row.parent).is_some_and(|live| {
                        live.started == saved.started && !live.state.starts_with('Z')
                    })
                });
                if owns_parent && !self.identities.contains_key(pid) {
                    self.identities.insert(*pid, row.clone());
                    added = true;
                }
            }
            if !added {
                break;
            }
        }
    }

    pub fn alive_in(&self, table: &BTreeMap<i32, Identity>) -> Vec<i32> {
        self.identities
            .iter()
            .filter_map(|(pid, saved)| {
                table
                    .get(pid)
                    .filter(|live| live.started == saved.started && !live.state.starts_with('Z'))
                    .map(|_| *pid)
            })
            .collect()
    }

    pub async fn persist(&self, path: &Path) -> Result<()> {
        let temporary = path.with_extension("tmp");
        tokio::fs::write(&temporary, serde_json::to_vec(self)?).await?;
        tokio::fs::rename(temporary, path).await?;
        Ok(())
    }

    pub async fn terminate(&mut self) -> Result<()> {
        let table = snapshot().await?;
        self.observe(&table);
        for signal in [libc::SIGTERM, libc::SIGKILL] {
            let table = snapshot().await?;
            self.observe(&table);
            for pid in self.alive_in(&table) {
                // SAFETY: PID is positive and belongs to a recorded live identity.
                let rc = unsafe { libc::kill(pid, signal) };
                if rc != 0 {
                    let error = std::io::Error::last_os_error();
                    if error.raw_os_error() != Some(libc::ESRCH) {
                        return Err(error.into());
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
        ensure!(
            self.alive_in(&snapshot().await?).is_empty(),
            "owned processes survived cancellation"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn row(pid: i32, parent: i32, started: &str) -> Identity {
        Identity {
            pid,
            parent,
            started: started.into(),
            state: "S".into(),
        }
    }
    #[test]
    fn does_not_signal_reused_pids_or_adopt_their_children() {
        let mut set = ProcessSet::new(10);
        set.observe(&BTreeMap::from([
            (10, row(10, 1, "old")),
            (11, row(11, 10, "child")),
        ]));
        let later = BTreeMap::from([
            (10, row(10, 1, "new")),
            (11, row(11, 1, "child")),
            (12, row(12, 10, "other")),
        ]);
        set.observe(&later);
        assert_eq!(set.alive_in(&later), vec![11]);
        assert!(!set.identities.contains_key(&12));
    }
}
