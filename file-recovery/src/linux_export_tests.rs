//! Real process/OS-lock coverage for Linux export versus material cleanup.
use super::*;
use std::{
    io::{Read, Write},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

fn scope() -> Scope {
    Scope {
        authority: "fixture-center".into(),
        device: "fixture-device".into(),
        os_user: "fixture-user".into(),
        owner: "fixture-owner".into(),
    }
}
struct Peer(Child);
impl Drop for Peer {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn peer(root: &Path, id: &str, mode: &str) -> Peer {
    Peer(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "linux_export_tests::export_cleanup_peer"])
            .env("LCXL_RECOVERY_EXPORT_ROOT", root)
            .env("LCXL_RECOVERY_EXPORT_ID", id)
            .env("LCXL_RECOVERY_EXPORT_MODE", mode)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    )
}
fn ready(root: &Path, child: &mut Peer) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !root.join("peer-ready").exists() {
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "peer exited before its lock assertion"
        );
        assert!(Instant::now() < deadline, "peer readiness timeout");
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn finish(child: &mut Peer) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            assert!(status.success());
            return;
        }
        assert!(Instant::now() < deadline, "peer completion timeout");
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn seed(vault: &Vault) -> String {
    let mut locked = vault.lock().unwrap();
    let record = locked
        .backup(BackupRequest {
            scope: scope(),
            conversation: "conversation",
            operation: "operation",
            generation: "generation",
            file_name: "fixture.txt",
            content: b"synthetic original",
            metadata: b"{}",
            now_ms: 1000,
        })
        .unwrap();
    locked
        .transition(&scope(), &record.id, ChangeState::Aborted)
        .unwrap();
    record.id
}
fn original(package: Vec<u8>) {
    let mut archive = zip::ZipArchive::new(io::Cursor::new(package)).unwrap();
    let mut bytes = Vec::new();
    archive
        .by_name("before.txt")
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    assert_eq!(bytes, b"synthetic original");
}

#[test]
fn export_cleanup_peer() {
    let Ok(root) = std::env::var("LCXL_RECOVERY_EXPORT_ROOT") else {
        return;
    };
    let root = Path::new(&root);
    let vault = Vault::open(root).unwrap();
    let id = std::env::var("LCXL_RECOVERY_EXPORT_ID").unwrap();
    match std::env::var("LCXL_RECOVERY_EXPORT_MODE").unwrap().as_str() {
        "cleanup" => {
            assert!(
                vault.try_lock().unwrap().is_none(),
                "cleanup entered an active export lock"
            );
            fs::write(root.join("peer-ready"), b"blocked").unwrap();
            std::io::stdin().read_exact(&mut [0u8; 1]).unwrap();
            let mut locked = vault.lock().unwrap();
            locked.discard(&scope(), "conversation", &id, 1002).unwrap();
            assert_eq!(
                locked.list(&scope(), None)[0].material,
                MaterialState::Purged
            );
        }
        "export" => {
            let locked = vault.lock().unwrap();
            original(locked.export_package(&scope(), &id, 1001).unwrap());
            fs::write(root.join("peer-ready"), b"export-held").unwrap();
            // The parent kills this process while it still owns the OS lock.
            std::io::stdin().read_exact(&mut [0u8; 1]).unwrap();
            drop(locked);
        }
        _ => panic!("unknown fixture mode"),
    }
}

#[test]
fn cleanup_cannot_interrupt_export_and_completed_bytes_survive_purge() {
    let root = tempfile::tempdir().unwrap();
    let vault = Vault::open(root.path()).unwrap();
    let id = seed(&vault);
    let locked = vault.lock().unwrap();
    let mut child = peer(root.path(), &id, "cleanup");
    ready(root.path(), &mut child);
    let package = locked.export_package(&scope(), &id, 1001).unwrap();
    drop(locked);
    child.0.stdin.take().unwrap().write_all(b"x").unwrap();
    finish(&mut child);
    original(package);
    let error = vault
        .lock()
        .unwrap()
        .export_package(&scope(), &id, 1003)
        .unwrap_err();
    assert_eq!(
        error.get_ref().unwrap().downcast_ref::<ExportError>(),
        Some(&ExportError::Cleaned)
    );
}

#[test]
fn killed_exporter_releases_lock_and_preserves_material_until_explicit_cleanup() {
    let root = tempfile::tempdir().unwrap();
    let vault = Vault::open(root.path()).unwrap();
    let id = seed(&vault);
    let mut child = peer(root.path(), &id, "export");
    ready(root.path(), &mut child);
    assert!(vault.try_lock().unwrap().is_none());
    child.0.kill().unwrap();
    assert!(!child.0.wait().unwrap().success());
    let mut locked = vault
        .try_lock()
        .unwrap()
        .expect("dead exporter kept OS lock");
    original(locked.export_package(&scope(), &id, 1002).unwrap());
    locked.discard(&scope(), "conversation", &id, 1003).unwrap();
    assert_eq!(
        locked.list(&scope(), None)[0].material,
        MaterialState::Purged
    );
}
