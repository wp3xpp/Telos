use anyhow::Result;
mod backup;
mod collector;
mod delivery;
mod install;
mod observation_report;
mod readiness;
mod retention;
mod review;
mod service;
mod upgrade;
use clap::{Parser, Subcommand};
use serde_json::{json, Value};
use std::{path::PathBuf, time::Duration};
use telos_runtime::{daemon, runner::CodexRunner};
use tokio::process::Command;

#[derive(Parser)]
#[command(name = "telos", version, about = "Persistent goal loops for Codex")]
struct Cli {
    /// Explicit local state directory (all persistence stays here).
    #[arg(long, global = true, default_value = ".telos")]
    state_dir: PathBuf,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Create a delivery, prepare its worktree, configure acceptance and start it.
    Deliver {
        #[arg(long)]
        file: PathBuf,
        #[arg(long)]
        wait: bool,
        #[arg(long, default_value_t = 600)]
        wait_seconds: u64,
    },
    /// Follow a persisted goal without submitting another task or rerunning tests.
    Follow {
        #[arg(long)]
        id: String,
        /// Return one current snapshot, including project observation evidence.
        #[arg(long)]
        once: bool,
        #[arg(long, default_value_t = 600)]
        wait_seconds: u64,
    },
    /// Show configuration gaps, collection evidence, schedule and first verdict.
    Readiness {
        #[arg(long)]
        id: String,
        /// Emit structured JSON instead of the terminal summary.
        #[arg(long)]
        json: bool,
    },
    /// Export a saved delivery patch and integrity-checked acceptance evidence.
    Review {
        #[arg(long)]
        id: String,
        #[arg(long)]
        output: PathBuf,
    },
    /// Manage an independent macOS observation collector.
    Collector {
        #[command(subcommand)]
        action: CollectorAction,
    },
    /// Collect actual Telos control-interface probes into normalized JSONL.
    ObserveService {
        #[arg(long)]
        output: PathBuf,
        #[arg(long, default_value_t = 5)]
        timeout_seconds: u64,
        #[arg(long, default_value_t = 1)]
        samples: u64,
        /// Delay after each completed probe; missed intervals are never replayed.
        #[arg(
            long,
            default_value_t = 5,
            value_parser = telos_core::duration::parse_duration_seconds
        )]
        interval_seconds: u64,
    },
    /// Create or verify an offline state backup. Does not restore or start work.
    Backup {
        #[command(subcommand)]
        action: BackupAction,
    },
    /// Manage a per-user macOS LaunchAgent for an existing installation.
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
    /// Install binary, skill and MCP registration at explicit locations. Does not start daemon.
    Install {
        #[arg(long)]
        prefix: PathBuf,
        #[arg(long)]
        codex_home: PathBuf,
        #[arg(long, default_value = "codex")]
        codex: PathBuf,
    },
    /// Upgrade an owned, stopped installation; resume a journaled upgrade if interrupted.
    Upgrade {
        #[arg(long)]
        prefix: PathBuf,
        #[arg(long)]
        codex_home: PathBuf,
        #[arg(long, default_value = "codex")]
        codex: PathBuf,
    },
    /// Remove an unchanged installation; preserve its data and any user modifications.
    Uninstall {
        #[arg(long)]
        prefix: PathBuf,
        #[arg(long)]
        codex_home: PathBuf,
        #[arg(long, default_value = "codex")]
        codex: PathBuf,
    },
    /// Probe an operator-specified local test command. Does not change any goal state.
    TestCommand {
        #[arg(long)]
        request: PathBuf,
        #[arg(long)]
        cwd: PathBuf,
        #[arg(long)]
        output: PathBuf,
        #[arg(long, default_value = "codex")]
        codex: PathBuf,
    },
    /// Run the stdio MCP bridge (the daemon must already be running).
    Mcp,
    /// Read-only environment diagnostics. Does not inspect credential files.
    Doctor,
    /// Start the single-writer local control service.
    Daemon {
        /// Serve only restored records/evidence; do not run workers or observers.
        #[arg(long, conflicts_with = "notifications")]
        recovery_review: bool,
        #[arg(long, default_value = "codex")]
        codex: PathBuf,
        /// Deliver meaningful state changes through macOS local notifications.
        #[arg(long)]
        notifications: bool,
    },
    /// Send a typed operator request from a JSON file; ordinary use is via MCP.
    Request { file: PathBuf },
}

#[derive(Subcommand)]
enum CollectorAction {
    /// Summarize actual observation history without blocking the collector or accepting a goal.
    Report {
        #[arg(long)]
        directory: PathBuf,
        #[arg(long, default_value_t = 1209600)]
        minimum_duration_seconds: i64,
        #[arg(long, default_value_t = 15)]
        max_gap_seconds: i64,
    },
    Install {
        #[arg(long)]
        directory: PathBuf,
        #[arg(long, default_value_t = 5)]
        interval_seconds: u64,
        #[arg(long, default_value_t = 3)]
        timeout_seconds: u64,
    },
    Status {
        #[arg(long)]
        directory: PathBuf,
    },
    Remove {
        #[arg(long)]
        directory: PathBuf,
    },
}

#[derive(Subcommand)]
enum BackupAction {
    /// Retire an operator-reconciled external session in a new recovery copy.
    ReconcileEnvironment {
        #[arg(long)]
        image: PathBuf,
        #[arg(long)]
        expected_receipt: String,
        #[arg(long)]
        decision: PathBuf,
        #[arg(long)]
        materials: PathBuf,
        #[arg(long)]
        destination: PathBuf,
    },
    /// Plan backup retention; supply the exact plan hash to apply it.
    Prune {
        #[arg(long)]
        directory: PathBuf,
        /// Keep at least this many verified backups per original state directory.
        #[arg(long, default_value_t = 3)]
        keep: usize,
        #[arg(long)]
        expected_plan: Option<String>,
    },
    /// Publish an authorized restored state at its bound new location; goals stay paused.
    ActivateRestore {
        #[arg(long)]
        image: PathBuf,
        #[arg(long)]
        expected_receipt: String,
        #[arg(long)]
        decision: PathBuf,
        #[arg(long)]
        destination: PathBuf,
    },
    /// Preserve historical run records outside the live cleanup namespace in a new copy.
    ArchiveRuns {
        #[arg(long)]
        image: PathBuf,
        #[arg(long)]
        expected_receipt: String,
        #[arg(long)]
        destination: PathBuf,
    },
    /// List activation blockers without removing recovery gates or acting on old process records.
    InspectActivation {
        #[arg(long)]
        image: PathBuf,
        #[arg(long)]
        expected_receipt: String,
        /// Explicitly inspect external adopted worktree paths recorded in the image.
        #[arg(long)]
        check_workspaces: bool,
    },
    /// Apply explicit operator material to a new copy; never activate restored work.
    ReviewProject {
        #[arg(long)]
        image: PathBuf,
        #[arg(long)]
        expected_receipt: String,
        #[arg(long)]
        decision: PathBuf,
        #[arg(long)]
        materials: PathBuf,
        #[arg(long)]
        destination: PathBuf,
    },
    /// Adopt a reviewed registered worktree in a new recovery copy.
    AdoptWorkspace {
        #[arg(long)]
        image: PathBuf,
        #[arg(long)]
        expected_receipt: String,
        #[arg(long)]
        decision: PathBuf,
        #[arg(long)]
        materials: PathBuf,
        #[arg(long)]
        destination: PathBuf,
    },
    /// Observe a prepared image's external worktree and produce a pinned file fingerprint.
    InspectWorkspace {
        #[arg(long)]
        image: PathBuf,
        #[arg(long)]
        expected_receipt: String,
        #[arg(long)]
        workspace_id: String,
        #[arg(long)]
        candidate: PathBuf,
    },
    /// Discard explicitly reconciled unknown executions in a new copy; never retry.
    DiscardExecutions {
        #[arg(long)]
        image: PathBuf,
        #[arg(long)]
        expected_receipt: String,
        #[arg(long)]
        decision: PathBuf,
        #[arg(long)]
        materials: PathBuf,
        #[arg(long)]
        destination: PathBuf,
    },
    /// Discard independently reconciled unknown verification observations in a new copy.
    DiscardVerifications {
        #[arg(long)]
        image: PathBuf,
        #[arg(long)]
        expected_receipt: String,
        #[arg(long)]
        decision: PathBuf,
        #[arg(long)]
        materials: PathBuf,
        #[arg(long)]
        destination: PathBuf,
    },
    /// Explicit read-only external Git identity check for a workspace in a recovery image.
    CheckWorkspace {
        #[arg(long)]
        recovery: PathBuf,
        #[arg(long)]
        workspace_id: String,
        #[arg(long)]
        candidate: PathBuf,
        #[arg(long)]
        expected_manifest: String,
        #[arg(long)]
        expected_database: String,
    },
    /// Prepare a separate recovery copy; keep execution disabled until reconciliation.
    PrepareRestore {
        #[arg(long)]
        recovery: PathBuf,
        #[arg(long)]
        expected_manifest: String,
        #[arg(long)]
        expected_database: String,
    },
    /// Inspect a staged recovery image without starting any work.
    InspectRestore {
        #[arg(long)]
        recovery: PathBuf,
    },
    /// Stage a verified recovery image; execution stays disabled pending reconciliation.
    StageRestore {
        #[arg(long)]
        backup: PathBuf,
        #[arg(long)]
        destination: PathBuf,
    },
    Create {
        #[arg(long)]
        destination: PathBuf,
    },
    Verify {
        #[arg(long)]
        backup: PathBuf,
    },
}

#[derive(Subcommand)]
enum ServiceAction {
    Install {
        #[arg(long)]
        prefix: PathBuf,
        #[arg(long)]
        codex: PathBuf,
        #[arg(long)]
        notifications: bool,
    },
    Status {
        #[arg(long)]
        prefix: PathBuf,
    },
    Remove {
        #[arg(long)]
        prefix: PathBuf,
    },
}

async fn probe(program: &str, args: &[&str]) -> Value {
    let mut command = Command::new(program);
    command.args(args).kill_on_drop(true);
    match tokio::time::timeout(Duration::from_secs(10), command.output()).await {
        Ok(Ok(output)) => {
            json!({"ok":output.status.success(),"stdout":String::from_utf8_lossy(&output.stdout).trim(),"stderr":String::from_utf8_lossy(&output.stderr).trim()})
        }
        Ok(Err(e)) => json!({"ok":false,"error":e.to_string()}),
        Err(_) => json!({"ok":false,"error":"probe timed out"}),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Deliver {
            file,
            wait,
            wait_seconds,
        } => {
            let result = delivery::submit(&cli.state_dir, &file, wait, wait_seconds).await?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        Commands::Follow {
            id,
            once,
            wait_seconds,
        } => {
            let result = delivery::follow(&cli.state_dir, &id, wait_seconds, once).await?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        Commands::Readiness { id, json } => {
            let report = daemon::call(&cli.state_dir, &daemon::Request::Readiness { id }).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                readiness::print(&report)?;
            }
        }
        Commands::Review { id, output } => {
            let result = review::export(&cli.state_dir, &id, &output).await?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        Commands::Collector { action } => {
            let result = match action {
                CollectorAction::Report {
                    directory,
                    minimum_duration_seconds,
                    max_gap_seconds,
                } => collector::report(&directory, minimum_duration_seconds, max_gap_seconds)?,
                CollectorAction::Install {
                    directory,
                    interval_seconds,
                    timeout_seconds,
                } => {
                    collector::install(
                        &directory,
                        &cli.state_dir,
                        interval_seconds,
                        timeout_seconds,
                    )
                    .await?
                }
                CollectorAction::Status { directory } => collector::status(&directory).await?,
                CollectorAction::Remove { directory } => collector::remove(&directory).await?,
            };
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        Commands::ObserveService {
            output,
            timeout_seconds,
            samples,
            interval_seconds,
        } => {
            let (stop, mut cancel) = tokio::sync::watch::channel(false);
            // Register before probing so service managers can terminate even startup.
            let mut terminate =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
            let mut interrupt =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
            let signal = tokio::spawn(async move {
                tokio::select! {
                    _ = terminate.recv() => {},
                    _ = interrupt.recv() => {},
                }
                let _ = stop.send(true);
            });
            let result = if samples == 1 {
                tokio::select! {
                    biased;
                    _ = cancel.changed() => Ok(json!({"recorded":false,"interrupted":true})),
                    result = telos_runtime::service_observation::observe(&cli.state_dir, &output, timeout_seconds) => result,
                }
            } else {
                telos_runtime::service_observation::collect(
                    &cli.state_dir,
                    &output,
                    timeout_seconds,
                    interval_seconds,
                    samples,
                    cancel,
                )
                .await
            };
            signal.abort();
            println!("{}", serde_json::to_string_pretty(&result?)?);
        }
        Commands::Backup { action } => {
            let result = match action {
                BackupAction::ReconcileEnvironment {
                    image,
                    expected_receipt,
                    decision,
                    materials,
                    destination,
                } => {
                    backup::reconcile_environment(
                        &image,
                        &expected_receipt,
                        &decision,
                        &materials,
                        &destination,
                    )
                    .await?
                }
                BackupAction::Prune {
                    directory,
                    keep,
                    expected_plan,
                } => retention::prune(&directory, keep, expected_plan.as_deref()).await?,
                BackupAction::ActivateRestore {
                    image,
                    expected_receipt,
                    decision,
                    destination,
                } => {
                    backup::activate_restore(&image, &expected_receipt, &decision, &destination)
                        .await?
                }
                BackupAction::ArchiveRuns {
                    image,
                    expected_receipt,
                    destination,
                } => backup::archive_runs(&image, &expected_receipt, &destination).await?,
                BackupAction::InspectActivation {
                    image,
                    expected_receipt,
                    check_workspaces,
                } => {
                    backup::inspect_activation(&image, &expected_receipt, check_workspaces).await?
                }
                BackupAction::ReviewProject {
                    image,
                    expected_receipt,
                    decision,
                    materials,
                    destination,
                } => {
                    backup::review_project(
                        &image,
                        &expected_receipt,
                        &decision,
                        &materials,
                        &destination,
                    )
                    .await?
                }
                BackupAction::AdoptWorkspace {
                    image,
                    expected_receipt,
                    decision,
                    materials,
                    destination,
                } => {
                    backup::adopt_workspace(
                        &image,
                        &expected_receipt,
                        &decision,
                        &materials,
                        &destination,
                    )
                    .await?
                }
                BackupAction::InspectWorkspace {
                    image,
                    expected_receipt,
                    workspace_id,
                    candidate,
                } => {
                    backup::inspect_workspace(&image, &expected_receipt, &workspace_id, &candidate)
                        .await?
                }
                BackupAction::DiscardExecutions {
                    image,
                    expected_receipt,
                    decision,
                    materials,
                    destination,
                } => {
                    backup::discard_executions(
                        &image,
                        &expected_receipt,
                        &decision,
                        &materials,
                        &destination,
                    )
                    .await?
                }
                BackupAction::DiscardVerifications {
                    image,
                    expected_receipt,
                    decision,
                    materials,
                    destination,
                } => {
                    backup::discard_verifications(
                        &image,
                        &expected_receipt,
                        &decision,
                        &materials,
                        &destination,
                    )
                    .await?
                }
                BackupAction::CheckWorkspace {
                    recovery,
                    workspace_id,
                    candidate,
                    expected_manifest,
                    expected_database,
                } => {
                    backup::check_workspace(
                        &recovery,
                        &workspace_id,
                        &candidate,
                        &expected_manifest,
                        &expected_database,
                    )
                    .await?
                }
                BackupAction::PrepareRestore {
                    recovery,
                    expected_manifest,
                    expected_database,
                } => {
                    backup::prepare_restore(&recovery, &expected_manifest, &expected_database)
                        .await?
                }
                BackupAction::InspectRestore { recovery } => {
                    backup::inspect_restore(&recovery).await?
                }
                BackupAction::StageRestore {
                    backup,
                    destination,
                } => backup::stage_restore(&backup, &destination).await?,
                BackupAction::Create { destination } => {
                    backup::create(&cli.state_dir, &destination).await?
                }
                BackupAction::Verify { backup } => backup::verify(&backup).await?,
            };
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        Commands::Service { action } => {
            let result = match action {
                ServiceAction::Install {
                    prefix,
                    codex,
                    notifications,
                } => service::install(&prefix, &codex, notifications).await?,
                ServiceAction::Status { prefix } => service::status(&prefix).await?,
                ServiceAction::Remove { prefix } => service::remove(&prefix).await?,
            };
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        Commands::Install {
            prefix,
            codex_home,
            codex,
        } => {
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &install::install(&prefix, &codex_home, &cli.state_dir, &codex).await?
                )?
            );
        }
        Commands::Upgrade {
            prefix,
            codex_home,
            codex,
        } => {
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &upgrade::upgrade(&prefix, &codex_home, &codex).await?
                )?
            );
        }
        Commands::Uninstall {
            prefix,
            codex_home,
            codex,
        } => {
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &install::uninstall(&prefix, &codex_home, &codex).await?
                )?
            );
        }
        Commands::TestCommand {
            request,
            cwd,
            output,
            codex,
        } => {
            let spec: telos_core::TestCommand =
                serde_json::from_slice(&tokio::fs::read(request).await?)?;
            let (tx, rx) = tokio::sync::watch::channel(false);
            tokio::spawn(async move {
                let mut term =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                        .expect("install SIGTERM handler");
                tokio::select! {_=tokio::signal::ctrl_c()=>{},_=term.recv()=>{}}
                let _ = tx.send(true);
            });
            let report = telos_runtime::test_runner::TestRunner { codex }
                .run(&spec, &cwd, &output, rx)
                .await?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            anyhow::ensure!(
                report.verdict == telos_core::Verdict::Passed,
                "test command did not pass: {}",
                report.reason
            );
        }
        Commands::Mcp => {
            telos_runtime::mcp::serve(&cli.state_dir, tokio::io::stdin(), tokio::io::stdout())
                .await?;
        }
        Commands::Daemon {
            recovery_review,
            codex,
            notifications,
        } => {
            let (tx, rx) = tokio::sync::watch::channel(false);
            tokio::spawn(async move {
                let mut term =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                        .expect("install SIGTERM handler");
                tokio::select! {_=tokio::signal::ctrl_c()=>{},_=term.recv()=>{}}
                let _ = tx.send(true);
            });
            if recovery_review {
                daemon::serve_recovery_review(&cli.state_dir, rx).await?;
            } else {
                daemon::serve_with_notifications(
                    &cli.state_dir,
                    CodexRunner { program: codex },
                    rx,
                    notifications,
                )
                .await?;
            }
        }
        Commands::Request { file } => {
            let request: daemon::Request = serde_json::from_slice(&tokio::fs::read(file).await?)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&daemon::call(&cli.state_dir, &request).await?)?
            );
        }
        Commands::Doctor => {
            let version = probe("codex", &["--version"]).await;
            let auth = probe("codex", &["login", "status"]).await;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "telos_version":env!("CARGO_PKG_VERSION"),
                    "codex":version,"authentication":auth,
                    "desktop_mcp":"not_verified",
                    "daemon":match daemon::call(&cli.state_dir,&daemon::Request::Ping).await {Ok(value)=>value,Err(e)=>json!({"reachable":false,"reason":e.to_string()})},
                    "note":"CLI authentication and fixture tests do not prove desktop MCP availability."
                }))?
            );
        }
    }
    Ok(())
}
