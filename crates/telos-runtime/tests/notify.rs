use anyhow::Result;
use std::{os::unix::fs::PermissionsExt, path::PathBuf};
use telos_runtime::notify;
use telos_store::NotificationLease;

struct Fixture(PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn notification_payload_is_passed_as_data_and_failures_are_reported() -> Result<()> {
    let f = Fixture(std::env::temp_dir().join(format!("telos-notify-{}", uuid::Uuid::new_v4())));
    std::fs::create_dir(&f.0)?;
    let program = f.0.join("adapter");
    let captured = f.0.join("arguments.json");
    std::fs::write(&program,format!("#!/usr/bin/env python3\nimport sys,json,pathlib\npathlib.Path({}).write_text(json.dumps(sys.argv[1:]))\n",serde_json::to_string(&captured.to_str().unwrap())?))?;
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700))?;
    let lease = NotificationLease {
        id: 1,
        token: "fixture".into(),
        event_sequence: 17,
        loop_id: Some("fixture\"; arbitrary text".into()),
        kind: "verification_recorded".into(),
        body: serde_json::json!({"verdict":"passed","private_log":"not for notification"}),
    };
    notify::deliver(&program, &lease).await?;
    let args: Vec<String> = serde_json::from_slice(&std::fs::read(captured)?)?;
    assert_eq!(args[0], "-e");
    assert!(!args[1].contains("arbitrary text"));
    assert_eq!(args[2], "--");
    assert!(args[3].contains("arbitrary text"));
    assert!(!args[3].contains("not for notification"));
    std::fs::write(&program, "#!/bin/sh\nexit 1\n")?;
    assert!(notify::deliver(&program, &lease).await.is_err());
    Ok(())
}
