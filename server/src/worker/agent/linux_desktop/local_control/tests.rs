use super::*;
use desk_input_injection::linux_input_block::BlockReport;
use std::sync::atomic::{AtomicBool, Ordering};

struct Fake {
    active: Arc<AtomicBool>,
    released: Arc<AtomicBool>,
}
impl ActiveControl for Fake {
    fn active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }
}
impl Drop for Fake {
    fn drop(&mut self) {
        self.released.store(true, Ordering::SeqCst);
    }
}
fn fake() -> (Fake, Arc<AtomicBool>, Arc<AtomicBool>) {
    let active = Arc::new(AtomicBool::new(true));
    let released = Arc::new(AtomicBool::new(false));
    (
        Fake {
            active: active.clone(),
            released: released.clone(),
        },
        active,
        released,
    )
}
async fn response(stream: &mut UnixStream) -> Response {
    tokio::time::timeout(Duration::from_secs(2), async {
        let n = stream.read_u32().await.unwrap() as usize;
        assert!(n <= MAX_MESSAGE);
        let mut body = vec![0; n];
        stream.read_exact(&mut body).await.unwrap();
        serde_json::from_slice(&body).unwrap()
    })
    .await
    .unwrap()
}
fn report() -> BlockReport {
    BlockReport {
        grabbed: 2,
        failed: 1,
        skipped: 3,
    }
}

#[tokio::test]
async fn explicit_stop_releases_before_end_ack_and_reports_partial_counts() {
    let (server, mut client) = UnixStream::pair().unwrap();
    let (lease, _, released) = fake();
    let task = tokio::spawn(hold_connection(server, lease, report()));
    assert!(matches!(
        response(&mut client).await,
        Response::Active {
            grabbed: 2,
            failed: 1,
            skipped: 3
        }
    ));
    client.write_all(&[0]).await.unwrap();
    assert!(matches!(response(&mut client).await, Response::Ended));
    assert!(released.load(Ordering::SeqCst));
    task.await.unwrap().unwrap();
}
#[tokio::test]
async fn disconnect_or_cancel_releases_the_connection_owned_period() {
    for cancel in [false, true] {
        let (server, mut client) = UnixStream::pair().unwrap();
        let (lease, _, released) = fake();
        let task = tokio::spawn(hold_connection(server, lease, report()));
        response(&mut client).await;
        if cancel {
            task.abort();
        } else {
            drop(client);
        }
        let _ = tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap();
        assert!(released.load(Ordering::SeqCst));
    }
}
#[tokio::test]
async fn backend_expiry_ends_the_native_connection() {
    let (server, mut client) = UnixStream::pair().unwrap();
    let (lease, active, released) = fake();
    let task = tokio::spawn(hold_connection(server, lease, report()));
    response(&mut client).await;
    active.store(false, Ordering::SeqCst);
    assert!(matches!(response(&mut client).await, Response::Ended));
    assert!(released.load(Ordering::SeqCst));
    task.await.unwrap().unwrap();
}
#[tokio::test]
async fn malformed_or_stale_worker_requests_cannot_reach_acquisition() {
    for stale in [false, true] {
        let broker = Arc::new(ComputerUseBroker::new());
        let owner = Arc::new(super::super::monitor_owner::MonitorOwner::default());
        let lease = owner.claim();
        if stale {
            drop(owner.invalidate());
        }
        let (server, mut client) = UnixStream::pair().unwrap();
        let task = tokio::spawn(serve(server, broker.clone(), lease));
        if stale {
            let bytes = serde_json::to_vec(&Request {
                version: 1,
                duration_secs: 1,
                accept_partial: false,
            })
            .unwrap();
            client.write_u32(bytes.len() as u32).await.unwrap();
            client.write_all(&bytes).await.unwrap();
            assert!(
                matches!(response(&mut client).await, Response::Unavailable { reason } if reason == "Worker was replaced")
            );
            task.await.unwrap().unwrap();
        } else {
            client.write_u32((MAX_MESSAGE + 1) as u32).await.unwrap();
            assert!(task.await.unwrap().is_err());
        }
        assert!(!broker.input_ownership_is_ready());
    }
}
#[test]
fn protocol_and_runtime_directory_require_explicit_bounds_and_owner() {
    for seconds in [0, 301] {
        assert!(
            Request {
                version: 1,
                duration_secs: seconds,
                accept_partial: false
            }
            .validate()
            .is_err()
        );
    }
    assert!(
        Request {
            version: 2,
            duration_secs: 1,
            accept_partial: false
        }
        .validate()
        .is_err()
    );
    let root = tempfile::tempdir().unwrap();
    let uid = unsafe { libc::geteuid() };
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    private_directory(root.path(), uid).unwrap();
    assert!(private_directory(root.path(), uid.wrapping_add(1)).is_err());
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o750)).unwrap();
    assert!(private_directory(root.path(), uid).is_err());
    let link = root.path().join("link");
    std::os::unix::fs::symlink(root.path(), &link).unwrap();
    assert!(private_directory(&link, uid).is_err());
}
