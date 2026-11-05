//! Incremental normalized JSONL observations. Timestamped heartbeat/error records
//! are required; absence of errors alone never establishes a healthy window.
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Seek, SeekFrom},
    os::unix::fs::MetadataExt,
    path::Path,
};
use telos_core::{LogWindowCheck, Verdict};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cursor {
    pub device: u64,
    pub inode: u64,
    pub offset: u64,
    pub anchor: Vec<u8>,
    pub floor: i64,
    pub last_poll: i64,
    pub window_start: Option<i64>,
    pub last_heartbeat: Option<i64>,
    pub last_error: Option<i64>,
    pub errors: u64,
    pub seen: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    timestamp: i64,
    kind: String,
    #[serde(default)]
    message: String,
}

impl Cursor {
    pub fn fresh(at: i64) -> Self {
        Self {
            device: 0,
            inode: 0,
            offset: 0,
            anchor: vec![],
            floor: at,
            last_poll: at,
            window_start: None,
            last_heartbeat: None,
            last_error: None,
            errors: 0,
            seen: vec![],
        }
    }
    pub fn reset_window(&mut self, at: i64) {
        self.floor = at;
        self.window_start = None;
        self.last_heartbeat = None;
    }
}

pub fn sample(
    root: &Path,
    check: &LogWindowCheck,
    previous: Option<Cursor>,
    at: i64,
) -> Result<(Cursor, Verdict, Value)> {
    ensure!(at >= 0, "invalid observation time");
    let mut cursor = previous.unwrap_or_else(|| Cursor::fresh(at));
    let prior_errors = cursor.errors;
    ensure!(
        cursor.anchor.len() <= 64
            && cursor.anchor.len() as u64 <= cursor.offset
            && cursor.seen.len() <= 256
            && cursor.last_poll >= 0,
        "invalid persisted log cursor"
    );
    let mut discontinuity = false;
    if at < cursor.last_poll || at - cursor.last_poll > check.max_gap_seconds {
        cursor.reset_window(at);
        discontinuity = true;
    }
    cursor.last_poll = at;
    let candidate = root.join(&check.relative_path);
    let metadata = match std::fs::symlink_metadata(&candidate) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            cursor.reset_window(at);
            return Ok((
                cursor,
                Verdict::Inconclusive,
                json!({"reason":"log source missing; no healthy observation"}),
            ));
        }
        Err(e) => return Err(e.into()),
    };
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "log source must be a regular file"
    );
    let path = candidate.canonicalize()?;
    ensure!(path.starts_with(root), "log source outside project");
    let mut file = std::fs::File::open(path)?;
    let metadata = file.metadata()?;
    ensure!(metadata.is_file(), "log source must be a regular file");
    let changed_identity =
        cursor.inode != 0 && (cursor.inode != metadata.ino() || cursor.device != metadata.dev());
    let mut changed_anchor = false;
    if !cursor.anchor.is_empty() && metadata.len() >= cursor.offset {
        file.seek(SeekFrom::Start(cursor.offset - cursor.anchor.len() as u64))?;
        let mut old = vec![0; cursor.anchor.len()];
        file.read_exact(&mut old)?;
        changed_anchor = old != cursor.anchor;
    }
    if changed_identity || metadata.len() < cursor.offset || changed_anchor {
        cursor.offset = 0;
        cursor.anchor.clear();
        cursor.reset_window(at);
        discontinuity = true;
    }
    cursor.device = metadata.dev();
    cursor.inode = metadata.ino();
    file.seek(SeekFrom::Start(cursor.offset))?;
    let mut bytes = Vec::new();
    (&mut file).take(65536).read_to_end(&mut bytes)?;
    let complete = bytes
        .iter()
        .rposition(|b| *b == b'\n')
        .map(|i| i + 1)
        .unwrap_or(0);
    let mut invalid = false;
    for line in bytes[..complete]
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
    {
        if line.len() > 16384 {
            invalid = true;
            cursor.reset_window(at);
            continue;
        }
        let record = match serde_json::from_slice::<Record>(line) {
            Ok(record)
                if matches!(record.kind.as_str(), "heartbeat" | "error")
                    && record.timestamp >= 0
                    && record.timestamp <= at =>
            {
                record
            }
            _ => {
                invalid = true;
                cursor.reset_window(at);
                continue;
            }
        };
        let hash = format!("{:x}", Sha256::digest(line));
        if cursor.seen.contains(&hash) {
            continue;
        }
        cursor.seen.push(hash);
        if cursor.seen.len() > 256 {
            cursor.seen.remove(0);
        }
        if record.timestamp < cursor.floor {
            continue;
        }
        if record.kind == "error" {
            cursor.errors = cursor.errors.saturating_add(1);
            cursor.last_error = Some(record.timestamp);
            cursor.window_start = None;
        } else if at - record.timestamp <= check.max_gap_seconds
            && cursor
                .last_error
                .is_none_or(|error| record.timestamp > error)
        {
            if cursor
                .last_heartbeat
                .is_none_or(|previous| record.timestamp - previous > check.max_gap_seconds)
                || cursor.window_start.is_none()
            {
                cursor.window_start = Some(record.timestamp);
            }
            cursor.last_heartbeat = Some(
                cursor
                    .last_heartbeat
                    .unwrap_or(record.timestamp)
                    .max(record.timestamp),
            );
        }
        let _ = record.message; // Source messages are not copied into user-facing notifications.
    }
    cursor.offset += complete as u64;
    if complete > 0 {
        let mut anchor = cursor.anchor.clone();
        anchor.extend_from_slice(&bytes[..complete]);
        cursor.anchor = anchor[anchor.len().saturating_sub(64)..].to_vec();
    }
    let pending = cursor.offset < file.metadata()?.len();
    if complete == 0 && bytes.len() == 65536 {
        invalid = true;
        cursor.reset_window(at);
    }
    let fresh = cursor
        .last_heartbeat
        .is_some_and(|t| at - t <= check.max_gap_seconds);
    if !fresh {
        cursor.window_start = None;
    }
    let failed = cursor.errors > prior_errors
        || cursor
            .last_error
            .is_some_and(|e| cursor.last_heartbeat.is_none_or(|h| e >= h));
    let (verdict, reason) = if invalid {
        (Verdict::Error, "invalid or oversized normalized log record")
    } else if failed {
        (
            Verdict::Failed,
            "error observed; a new healthy window is required",
        )
    } else if discontinuity {
        (
            Verdict::Inconclusive,
            "source or polling continuity changed; observation window restarted",
        )
    } else if pending {
        (
            Verdict::Inconclusive,
            "unread backlog or partial record remains",
        )
    } else if fresh
        && cursor
            .window_start
            .is_some_and(|start| at - start >= check.window_seconds)
    {
        (
            Verdict::Passed,
            "fresh heartbeats cover the configured error-free window",
        )
    } else {
        (
            Verdict::Inconclusive,
            "waiting for a complete window of fresh heartbeats",
        )
    };
    let detail = json!({"reason":reason,"expected_wait":verdict==Verdict::Inconclusive && fresh && !pending && !discontinuity && !invalid,
        "offset":cursor.offset,"inode":cursor.inode,"window_start":cursor.window_start,
        "last_heartbeat":cursor.last_heartbeat,"error_count":cursor.errors,"last_error":cursor.last_error,"observed_at":at,"pending_bytes":pending});
    Ok((cursor, verdict, detail))
}
