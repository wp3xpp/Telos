//! Operator-triggered detached worktrees with durable ownership and bounded local Git.
use anyhow::{ensure, Context, Result};
use std::{
    io::Read,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use telos_core::{DeliveryWorkspace, LoopKind, LoopState, ProjectExecutionPolicy, Sandbox};
use telos_store::Store;
use tokio::{io::AsyncReadExt, process::Command};

async fn git(root: &Path, args: &[&str]) -> Result<(bool, String)> {
    let mut command = Command::new("git");
    command.env_clear();
    for key in ["PATH", "HOME", "TMPDIR"] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    command
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "submodule.recurse=false",
            "-c",
            "gc.auto=0",
            "-c",
            "maintenance.auto=false",
        ])
        .arg("-C")
        .arg(root)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn()?;
    let stdout = child.stdout.take().context("git stdout missing")?;
    let stderr = child.stderr.take().context("git stderr missing")?;
    let result = tokio::time::timeout(Duration::from_secs(30), async {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut stdout = stdout.take(1_048_577);
        let mut stderr = stderr.take(1_048_577);
        let (a, b) = tokio::join!(stdout.read_to_end(&mut out), stderr.read_to_end(&mut err));
        a?;
        b?;
        ensure!(
            out.len() <= 1_048_576 && err.len() <= 1_048_576,
            "Git output limit exceeded"
        );
        let status = child.wait().await?;
        // Git diagnostics may include project paths; do not forward config contents.
        ensure!(
            status.success() || status.code() == Some(1),
            "Git command failed (exit {:?})",
            status.code()
        );
        Ok::<_, anyhow::Error>((status.success(), String::from_utf8(out)?))
    })
    .await;
    match result {
        Ok(value) => value,
        Err(_) => {
            anyhow::bail!("Git operation timed out; inspect the reserved worktree before retrying")
        }
    }
}
async fn required(root: &Path, args: &[&str]) -> Result<String> {
    let (ok, out) = git(root, args).await?;
    ensure!(ok, "Git command did not succeed");
    Ok(out.trim_end_matches(['\n', '\r']).to_owned())
}

pub async fn validate_identity(workspace: &DeliveryWorkspace) -> Result<()> {
    let path = Path::new(&workspace.path);
    let m = std::fs::symlink_metadata(path)?;
    ensure!(
        m.is_dir() && !m.file_type().is_symlink(),
        "worktree must be an owned regular directory"
    );
    ensure!(
        Path::new(&required(path, &["rev-parse", "--show-toplevel"]).await?).canonicalize()?
            == path.canonicalize()?,
        "worktree root mismatch"
    );
    ensure!(
        Path::new(
            &required(
                path,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"]
            )
            .await?
        )
        .canonicalize()?
            == Path::new(&workspace.repository_git_dir),
        "worktree repository mismatch"
    );
    // A copied .git pointer can still refer to the original worktree's administration.
    // Require the repository's reciprocal backlink to identify this exact checkout.
    let admin = PathBuf::from(required(path, &["rev-parse", "--absolute-git-dir"]).await?);
    let common = Path::new(&workspace.repository_git_dir);
    ensure!(
        admin.parent() == Some(common.join("worktrees").as_path())
            && admin.canonicalize()? == admin,
        "worktree administration is outside expected repository"
    );
    let backlink = admin.join("gitdir");
    let mut bytes = Vec::new();
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&backlink)?;
    ensure!(
        file.metadata()?.is_file(),
        "worktree backlink must be regular"
    );
    file.take(65537).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 65536, "worktree backlink exceeds limit");
    let text = std::str::from_utf8(&bytes)?.trim_end_matches(['\n', '\r']);
    let pointer = path.join(".git");
    ensure!(
        std::fs::symlink_metadata(&pointer)?.is_file(),
        "worktree .git pointer must be regular"
    );
    ensure!(
        Path::new(text).is_absolute()
            && Path::new(text).canonicalize()? == pointer.canonicalize()?,
        "worktree registration points to a different checkout"
    );
    ensure!(
        required(path, &["rev-parse", "HEAD"]).await? == workspace.base_commit,
        "worktree base changed"
    );
    let (attached, _) = git(path, &["symbolic-ref", "--quiet", "HEAD"]).await?;
    ensure!(!attached, "worktree must have detached HEAD");
    Ok(())
}

async fn validate(workspace: &DeliveryWorkspace) -> Result<()> {
    validate_identity(workspace).await?;
    let path = Path::new(&workspace.path);
    ensure!(
        required(path, &["status", "--porcelain=v1", "--untracked-files=all"])
            .await?
            .is_empty(),
        "worktree has changes; preserved for inspection"
    );
    Ok(())
}

pub async fn prepare(
    store: &Store,
    id: &str,
    revision: i64,
    policy_revision: i64,
    base: &str,
) -> Result<DeliveryWorkspace> {
    ensure!(
        base == "HEAD"
            || ([40, 64].contains(&base.len()) && base.bytes().all(|b| b.is_ascii_hexdigit())),
        "base must be HEAD or a complete commit hash"
    );
    let goal = store.get_loop(id).await?;
    ensure!(
        goal.kind == LoopKind::Delivery
            && goal.state == LoopState::Active
            && goal.revision == revision,
        "active delivery goal required"
    );
    let project = store.inspect_project(&goal.project_id).await?;
    let config = &project["execution_policy"];
    let policy: ProjectExecutionPolicy = serde_json::from_value(config["policy"].clone())?;
    ensure!(
        config["revision"].as_i64() == Some(policy_revision)
            && policy.enabled
            && policy.sandbox == Sandbox::WorkspaceWrite,
        "current explicit write authority required"
    );
    let root = Path::new(project["root"].as_str().context("project root missing")?);
    ensure!(
        Path::new(&required(root, &["rev-parse", "--show-toplevel"]).await?).canonicalize()?
            == root,
        "register the Git repository root, not a subdirectory"
    );
    let git_dir = Path::new(
        &required(
            root,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )
        .await?,
    )
    .canonicalize()?;
    // No checkout filters or repository hooks execute during preparation.
    let (filters, _) = git(
        root,
        &["config", "--name-only", "--get-regexp", "^filter\\."],
    )
    .await?;
    ensure!(
        !filters,
        "repositories with local checkout filters need a dedicated adapter"
    );
    let parent = store.workspace_parent();
    std::fs::create_dir_all(&parent)?;
    let metadata = std::fs::symlink_metadata(&parent)?;
    ensure!(
        metadata.is_dir()
            && !metadata.file_type().is_symlink()
            && !parent.canonicalize()?.starts_with(root),
        "workspaces must be outside the project in a regular directory"
    );
    let _guard = crate::daemon::maintenance_lock(&parent)
        .context("another workspace preparation is active")?;
    let existing = store.workspace_for_loop(id, revision).await?;
    let commit = if let Some(w) = &existing {
        // Retrying HEAD means the already-pinned base, even if the user's branch advanced.
        ensure!(
            base == "HEAD" || base.eq_ignore_ascii_case(&w.base_commit),
            "workspace already pinned to another commit"
        );
        w.base_commit.clone()
    } else {
        required(
            root,
            &[
                "rev-parse",
                "--verify",
                "--end-of-options",
                &format!("{base}^{{commit}}"),
            ],
        )
        .await?
    };
    let tree = required(root, &["ls-tree", "-r", "--format=%(objectmode)", &commit]).await?;
    ensure!(
        !tree.lines().any(|mode| mode == "160000"),
        "submodule delivery requires a dedicated adapter"
    );
    let cycle = store
        .start_cycle(id, revision, &format!("delivery-workspace:{revision}"))
        .await?;
    let (workspace, fresh) = store
        .reserve_workspace(
            &cycle,
            revision,
            policy_revision,
            git_dir.to_str().context("Git path must be UTF-8")?,
            &commit,
        )
        .await?;
    let result: Result<()> = async {
        if fresh {
            ensure!(
                !Path::new(&workspace.path).try_exists()?,
                "reserved workspace destination already exists"
            );
            required(
                root,
                &[
                    "worktree",
                    "add",
                    "--detach",
                    "--",
                    &workspace.path,
                    &workspace.base_commit,
                ],
            )
            .await?;
        }
        // Existing reservations are inspected, never recreated after unknown effects.
        validate(&workspace).await
    }
    .await;
    match result {
        Ok(()) => store.finish_workspace(&workspace, None).await,
        Err(error) => {
            store
                .finish_workspace(&workspace, Some(&error.to_string()))
                .await?;
            Err(error.context("workspace preserved; preparation is not verified"))
        }
    }
}

/// Fingerprint Git-visible files plus explicitly checked paths. Ignored build
/// outputs/dependencies are outside this identity unless explicitly checked.
pub async fn snapshot(
    workspace: &DeliveryWorkspace,
    extra_paths: &[String],
) -> Result<serde_json::Value> {
    use serde_json::json;
    use sha2::{Digest, Sha256};
    use std::{
        collections::BTreeSet,
        io::Read,
        os::unix::fs::{OpenOptionsExt, PermissionsExt},
        path::Component,
    };
    validate_identity(workspace).await?;
    let (ok, list) = git(
        Path::new(&workspace.path),
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ],
    )
    .await?;
    ensure!(ok, "cannot enumerate delivery files");
    let mut paths: BTreeSet<String> = list
        .split('\0')
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    paths.extend(extra_paths.iter().cloned());
    ensure!(
        paths.len() <= 10000,
        "delivery snapshot file limit exceeded"
    );
    let workspace = workspace.clone();
    let manifest=tokio::task::spawn_blocking(move || -> Result<serde_json::Value> {
        let root=Path::new(&workspace.path).canonicalize()?;
        let mut files=Vec::new();let mut total=0usize;
        for relative in paths {
            ensure!(Path::new(&relative).components().all(|p|matches!(p,Component::Normal(v) if v != ".git")),"invalid snapshot path");
            let path=root.join(&relative);
            let metadata=match std::fs::symlink_metadata(&path) {
                Ok(m)=>m,
                Err(e) if e.kind()==std::io::ErrorKind::NotFound => {files.push(json!({"path":relative,"kind":"missing"}));continue;},
                Err(e)=>return Err(e.into())
            };
            ensure!(path.parent().context("parent missing")?.canonicalize()?.starts_with(&root),"snapshot path escapes workspace");
            if metadata.file_type().is_symlink() {
                let target=std::fs::read_link(&path)?;
                files.push(json!({"path":relative,"kind":"symlink","target":target.to_str().context("non-UTF8 symlink target")?}));
            }else{
                ensure!(metadata.is_file(),"snapshot entry must be a file or symlink");
                let file=std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(&path)?;let before=file.metadata()?;
                let mut bytes=Vec::new();(&file).take(8*1024*1024+1).read_to_end(&mut bytes)?;
                total+=bytes.len();ensure!(bytes.len()<=8*1024*1024 && total<=64*1024*1024,"snapshot byte limit exceeded");
                let after=file.metadata()?;
                ensure!(before.len()==after.len() && before.modified()?==after.modified()? && after.len()==bytes.len() as u64,"snapshot file changed while reading");
                files.push(json!({"path":relative,"kind":"file","sha256":format!("{:x}",Sha256::digest(&bytes)),"byte_length":bytes.len(),"executable":after.permissions().mode() & 0o111 != 0}));
            }
        }
        Ok(json!({"workspace_id":workspace.id,"base_commit":workspace.base_commit,"files":files,"scope":"git-visible files plus acceptance paths; ignored dependencies are not captured"}))
    }).await??;
    Ok(manifest)
}

/// Reviewable patch from the pinned base. No index, branch or source mutation.
/// Caller must compare before/after snapshots around this operation.
pub async fn review_patch(
    workspace: &DeliveryWorkspace,
    manifest: &serde_json::Value,
) -> Result<serde_json::Value> {
    use serde_json::json;
    use sha2::{Digest, Sha256};
    use std::collections::BTreeSet;
    let root = Path::new(&workspace.path);
    validate_identity(workspace).await?;
    let (ok, tracked) = git(root, &["ls-files", "-z", "--cached"]).await?;
    ensure!(ok, "cannot enumerate tracked review files");
    let tracked: BTreeSet<&str> = tracked.split('\0').filter(|s| !s.is_empty()).collect();
    let (ok, mut patch) = git(
        root,
        &[
            "diff",
            "--binary",
            "--no-ext-diff",
            "--no-textconv",
            "--no-renames",
            "--no-color",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            &workspace.base_commit,
            "--",
        ],
    )
    .await?;
    ensure!(ok, "tracked review diff failed");
    let files = manifest["files"]
        .as_array()
        .context("snapshot files missing")?;
    let mut untracked = 0;
    for entry in files {
        let path = entry["path"].as_str().context("snapshot path missing")?;
        if !tracked.contains(path) && entry["kind"] != "missing" {
            untracked += 1;
            ensure!(untracked <= 100, "review has more than 100 untracked files");
            let (same, extra) = git(
                root,
                &[
                    "diff",
                    "--no-index",
                    "--binary",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--no-color",
                    "--src-prefix=a/",
                    "--dst-prefix=b/",
                    "--",
                    "/dev/null",
                    path,
                ],
            )
            .await?;
            ensure!(
                !same || extra.is_empty(),
                "unexpected new-file patch status"
            );
            patch.push_str(&extra);
        }
        ensure!(
            patch.len() <= 4 * 1024 * 1024,
            "delivery review patch exceeds 4MiB"
        );
    }
    Ok(
        json!({"format":"telos-delivery-review-v1","workspace_id":workspace.id,"base_commit":workspace.base_commit,"snapshot_sha256":format!("{:x}",Sha256::digest(serde_json::to_vec(manifest)?)),"patch":patch,"scope":manifest["scope"],"note":"Review-only patch from the pinned base, including Git-visible new files and explicit acceptance paths. Inspect before applying; does not merge, publish, or prove project tests passed."}),
    )
}
