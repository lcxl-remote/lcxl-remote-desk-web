//! A private bus exercises service replacement without changing accessibility.
use super::*;
use std::{
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::io::{AsyncBufReadExt, BufReader};

struct Registry(Arc<Mutex<Vec<String>>>);
#[zbus::interface(name = "org.a11y.atspi.Registry")]
impl Registry {
    async fn register_event(&self, event: String, _properties: Vec<String>, _application: String) {
        self.0.lock().unwrap().push(event);
    }
}

#[tokio::test]
#[ignore = "spawns a private dbus-daemon; run explicitly with an external timeout"]
async fn registry_replacement_invalidates_subscription_without_bus_restart() {
    let directory = tempfile::tempdir().unwrap();
    let mut daemon = tokio::process::Command::new("dbus-daemon")
        .args(["--session", "--nofork", "--print-address=1"])
        .arg(format!(
            "--address=unix:tmpdir={}",
            directory.path().display()
        ))
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut address = String::new();
    tokio::time::timeout(
        Duration::from_secs(3),
        BufReader::new(daemon.stdout.take().unwrap()).read_line(&mut address),
    )
    .await
    .unwrap()
    .unwrap();
    let address = address.trim();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let service = zbus::connection::Builder::address(address)
        .unwrap()
        .name(REGISTRY)
        .unwrap()
        .serve_at("/org/a11y/atspi/registry", Registry(calls.clone()))
        .unwrap()
        .build()
        .await
        .unwrap();
    let client = zbus::connection::Builder::address(address)
        .unwrap()
        .build()
        .await
        .unwrap();
    let mut original = subscribe(&client).await.unwrap();
    assert_eq!(calls.lock().unwrap().len(), 3);
    assert_eq!(original.owner, service.unique_name().unwrap().as_str());
    service.release_name(REGISTRY).await.unwrap();
    let event = tokio::time::timeout(Duration::from_secs(2), original.events.next())
        .await
        .unwrap()
        .unwrap();
    assert!(
        event
            .unwrap_err()
            .message
            .contains("Registry owner changed")
    );
    let replacement_calls = Arc::new(Mutex::new(Vec::new()));
    let replacement = zbus::connection::Builder::address(address)
        .unwrap()
        .name(REGISTRY)
        .unwrap()
        .serve_at(
            "/org/a11y/atspi/registry",
            Registry(replacement_calls.clone()),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    let current = subscribe(&client).await.unwrap();
    assert_ne!(current.owner, original.owner);
    assert_eq!(current.owner, replacement.unique_name().unwrap().as_str());
    assert_eq!(replacement_calls.lock().unwrap().len(), 3);
    assert_eq!(
        calls.lock().unwrap().len(),
        3,
        "registration went to the old owner"
    );
    drop((current, original, client, replacement, service));
    daemon.kill().await.unwrap();
    daemon.wait().await.unwrap();
}
