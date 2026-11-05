//! Local OS notification delivery; never sends email or messages to third parties.
use anyhow::{ensure, Result};
use std::{path::PathBuf, process::Stdio, time::Duration};
use telos_store::{now, NotificationLease, Store};
use tokio::{process::Command, sync::watch};

const SCRIPT: &str =
    "on run argv\ndisplay notification (item 1 of argv) with title (item 2 of argv)\nend run";

pub fn message(lease: &NotificationLease) -> String {
    let description = match lease.kind.as_str() {
        "verification_recorded" => match lease.body["verdict"].as_str() {
            Some("passed") => "验收通过，请查看目标结果",
            Some("failed") => "验收未通过，需要继续处理",
            Some("inconclusive") => "证据不足，暂时无法判断",
            _ => "验收检查出错，需要检查执行记录",
        },
        "diagnosis_ready" => "异常诊断已完成，请查看原因与处理建议",
        "execution_uncertain" => "执行结果未知，请核对后再继续",
        "schedule_suspended" => "周期调度已暂停，需要检查原因",
        "verification_deferred" => "验收已延后，需要检查原因",
        _ => "目标有新的待处理状态",
    };
    format!(
        "{} · {} · 事件 {}",
        lease.loop_id.as_deref().unwrap_or("Telos"),
        description,
        lease.event_sequence
    )
}

pub async fn deliver(program: &std::path::Path, lease: &NotificationLease) -> Result<()> {
    let mut command = Command::new(program);
    // The script is fixed; user data is passed as arguments, never interpolated as code.
    command
        .args(["-e", SCRIPT, "--", &message(lease), "Telos"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let status = tokio::time::timeout(Duration::from_secs(10), command.status()).await??;
    ensure!(
        status.success(),
        "local notification adapter failed (exit {:?})",
        status.code()
    );
    Ok(())
}

pub async fn run(
    store: Store,
    program: PathBuf,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! { _=tick.tick()=>{}, _=shutdown.changed()=>{} }
        if *shutdown.borrow() {
            break;
        }
        if let Some(lease) = store.claim_notification(now()).await? {
            let error = deliver(&program, &lease).await.err().map(|e| e.to_string());
            store
                .finish_notification(&lease, error.as_deref(), now())
                .await?;
        }
    }
    Ok(())
}
