//! Opt-in evidence against a real synthetic GTK application, never other UI trees.
use super::*;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;

struct Fixture(Child);
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn params() -> UiInspectParams {
    UiInspectParams {
        allow_unfiltered: true,
        overview: false,
        query: None,
        element_only: false,
        scope: Default::default(),
        root: None,
        max_depth: 10,
        max_nodes: 64,
        max_bytes: 32 * 1024,
    }
}

#[tokio::test]
#[ignore = "requires local enabled accessibility and GNOME Wayland; see PoC README; timeout 60s"]
async fn live_gtk_tree_redacts_password_and_rejects_exited_application() {
    // Fail before creating a window when local prerequisites are absent. Never
    // enable org.a11y.Status or change a dconf preference from this test.
    Bus::connect()
        .await
        .expect("local accessibility must already be enabled");
    let script = std::env::var_os("LCXL_GTK_ATSPI_SCRIPT").expect("explicit fixture path required");
    let mut fixture = Fixture(
        Command::new("/usr/bin/python3")
            .arg(script)
            .env("LCXL_GTK_ATSPI_FIXTURE", "1")
            .env("GDK_BACKEND", "wayland")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let mut line = String::new();
    BufReader::new(fixture.0.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert_eq!(line.trim(), "fixture-ready");
    let pid = fixture.0.id();
    let broker = Arc::new(crate::worker::agent::computer_use_broker::ComputerUseBroker::new());
    let _monitor = lifetime::start(Arc::downgrade(&broker), broker.linux_monitor_owner.claim());
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let (application, tree) = loop {
        let attempt = tokio::task::spawn_blocking(move || {
            let app = application_by_pid(pid)?;
            let tree = collect(
                pid,
                app.image_path.clone(),
                app.process_started_at,
                params(),
                None,
            )?;
            Ok::<_, AgentError>((app, tree))
        })
        .await
        .unwrap();
        if let Ok(result) = attempt {
            break result;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "fixture tree did not become ready"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert!(!tree.truncated);
    assert!(
        tree.nodes
            .iter()
            .any(|node| node.name.as_deref() == Some("Fixture public entry"))
    );
    assert!(
        tree.nodes
            .iter()
            .any(|node| node.name.as_deref() == Some("Fixture Save"))
    );
    let protected: Vec<_> = tree.nodes.iter().filter(|node| node.is_protected).collect();
    assert!(!protected.is_empty());
    assert!(protected.iter().all(|node| node.name.is_none()
        && node.value.is_none()
        && node.supported_actions.is_empty()));
    let encoded = serde_json::to_string(&tree.nodes).unwrap();
    assert!(!encoded.contains("fixture-secret-never-export"));
    assert!(!encoded.contains("fixture-password-name-must-be-redacted"));
    fixture
        .0
        .stdin
        .take()
        .unwrap()
        .write_all(b"quit\n")
        .unwrap();
    assert!(fixture.0.wait().unwrap().success());
    let result = tokio::task::spawn_blocking(move || {
        collect(
            pid,
            application.image_path,
            application.process_started_at,
            params(),
            None,
        )
    })
    .await
    .unwrap();
    assert!(
        result.is_err(),
        "exited application must not produce a fresh tree"
    );
}
