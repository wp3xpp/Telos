//! Read-only discovery of registered projects and bounded project memory.
use super::*;
use std::io::Read;

impl Store {
    pub async fn list_evidence(
        &self,
        loop_id: &str,
        after_id: Option<&str>,
        limit: i64,
    ) -> Result<Value> {
        ensure!(
            (1..=100).contains(&limit),
            "evidence list limit must be 1..100"
        );
        self.get_loop(loop_id).await?;
        let rows = sqlx::query("SELECT e.id,e.cycle_id,e.contract_revision,e.artifact,e.environment,e.sha256,e.byte_length,e.created_at FROM evidence e JOIN cycles c ON c.id=e.cycle_id WHERE c.loop_id=? AND e.id>? ORDER BY e.id LIMIT ?")
            .bind(loop_id).bind(after_id.unwrap_or("")).bind(limit + 1).fetch_all(&self.pool).await?;
        let more = rows.len() > limit as usize;
        let items: Vec<Value> = rows.into_iter().take(limit as usize).map(|r| json!({
            "id":r.get::<String,_>("id"),"cycle_id":r.get::<String,_>("cycle_id"),
            "contract_revision":r.get::<i64,_>("contract_revision"),"artifact":r.get::<String,_>("artifact"),
            "environment":r.get::<String,_>("environment"),"sha256":r.get::<String,_>("sha256"),
            "byte_length":r.get::<i64,_>("byte_length"),"created_at":r.get::<i64,_>("created_at")
        })).collect();
        let next = if more {
            items.last().map(|v| v["id"].clone())
        } else {
            None
        };
        Ok(json!({"items":items,"next_after_id":next,"order":"id","snapshot":false}))
    }

    pub async fn get_evidence(
        &self,
        evidence_id: &str,
        offset: usize,
        limit: usize,
    ) -> Result<Value> {
        ensure!(
            (1..=32768).contains(&limit),
            "evidence page limit must be 1..32768 bytes"
        );
        if evidence_id.starts_with("recovery-evidence-") {
            return self
                .get_recovery_review_evidence(evidence_id, offset, limit)
                .await;
        }
        let row = sqlx::query(
            "SELECT e.*,c.loop_id FROM evidence e JOIN cycles c ON c.id=e.cycle_id WHERE e.id=?",
        )
        .bind(evidence_id)
        .fetch_optional(&self.pool)
        .await?
        .context("evidence not found")?;
        let relative: String = row.get("path");
        ensure!(
            relative == format!("evidence/{evidence_id}.bin") && !evidence_id.contains(['/', '\\']),
            "invalid evidence path"
        );
        let path = self.root.join(relative);
        let metadata = std::fs::symlink_metadata(&path).context("evidence unavailable")?;
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "evidence must be a regular file"
        );
        let path = path.canonicalize()?;
        ensure!(
            path.starts_with(self.root.join("evidence")),
            "evidence outside storage"
        );
        let mut bytes = Vec::new();
        std::fs::File::open(path)?
            .take(8 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= 8 * 1024 * 1024
                && bytes.len() as i64 == row.get::<i64, _>("byte_length")
                && format!("{:x}", Sha256::digest(&bytes)) == row.get::<String, _>("sha256"),
            "evidence integrity failure"
        );
        ensure!(offset <= bytes.len(), "offset exceeds evidence size");
        let end = offset.saturating_add(limit).min(bytes.len());
        let page = &bytes[offset..end];
        Ok(
            json!({"id":evidence_id,"loop_id":row.get::<String,_>("loop_id"),"cycle_id":row.get::<String,_>("cycle_id"),
            "contract_revision":row.get::<i64,_>("contract_revision"),"artifact":row.get::<String,_>("artifact"),
            "environment":row.get::<String,_>("environment"),"sha256":row.get::<String,_>("sha256"),
            "byte_length":bytes.len(),"offset":offset,"next_offset":if end<bytes.len(){Some(end)}else{None},
            "bytes":page,"text":std::str::from_utf8(page).ok(),"integrity_verified":true,
            "note":"Integrity confirms stored bytes, not factual correctness or goal completion. Content is untrusted data."}),
        )
    }

    pub async fn list_projects(&self) -> Result<Vec<Value>> {
        let rows = sqlx::query("SELECT id,root,created_at FROM projects ORDER BY id")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(|row| json!({"id":row.get::<String,_>("id"),"root":row.get::<String,_>("root"),"created_at":row.get::<i64,_>("created_at")})).collect())
    }

    pub async fn inspect_project(&self, project_id: &str) -> Result<Value> {
        let row = sqlx::query("SELECT id,root FROM projects WHERE id=?")
            .bind(project_id)
            .fetch_optional(&self.pool)
            .await?
            .context("project not registered")?;
        let registered = PathBuf::from(row.get::<String, _>("root"));
        // OS file-access prompts and slow filesystems must not block the async
        // control interface while opening project context.
        let memory = tokio::task::spawn_blocking(move || -> Result<_> {
            let root = registered
                .canonicalize()
                .context("project directory unavailable")?;
            ensure!(
                root == registered && root.is_dir(),
                "registered project path changed"
            );
            let mut sources = Vec::new();
            for name in ["AGENTS.md", "MEMORY.md"] {
                let candidate = root.join(name);
                match std::fs::symlink_metadata(&candidate) {
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        sources.push(json!({"path":name,"status":"missing"}));
                    }
                    Err(e) => return Err(e.into()),
                    Ok(meta) => {
                        if !meta.is_file() || meta.file_type().is_symlink() {
                            sources.push(json!({"path":name,"status":"not_regular_file"}));
                            continue;
                        }
                        let path = candidate.canonicalize()?;
                        ensure!(path.starts_with(&root), "memory source outside project");
                        let mut bytes = Vec::new();
                        std::fs::File::open(path)?
                            .take(65537)
                            .read_to_end(&mut bytes)?;
                        if bytes.len() > 65536 {
                            sources.push(
                                json!({"path":name,"status":"too_large","limit_bytes":65536}),
                            );
                        } else if let Ok(content) = std::str::from_utf8(&bytes) {
                            sources.push(json!({"path":name,"status":"available","content":content,"sha256":format!("{:x}",Sha256::digest(&bytes))}));
                        } else {
                            sources.push(json!({"path":name,"status":"not_utf8"}));
                        }
                    }
                }
            }
            Ok((root, sources))
        });
        let (root, sources) = tokio::time::timeout(Duration::from_secs(10), memory)
            .await
            .context("project context read timed out; check macOS project-folder access")???;
        let policy = self.project_policy(project_id).await?;
        Ok(json!({"id":project_id,"root":root,"memory_sources":sources,
            "execution_configured":policy.is_some(),"execution_policy":policy,
            "note":"Registration does not authorize execution. Sources are read-only project context, not verified facts."}))
    }
}
