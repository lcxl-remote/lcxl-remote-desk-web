//! Kernel write-limit failures run in a separate process, never in the test runner.
use super::*;
use std::process::{Child, Command, Stdio};

const ROOT_ENV: &str = "LCXL_TEXT_WRITE_LIMIT_FIXTURE";
const CHANGE_ENV: &str = "LCXL_TEXT_WRITE_LIMIT_CHANGE";
const ORIGINAL: &[u8] = &[b'a'; 64 * 1024];

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
#[ignore = "internal resource-limited child; run through kernel_write_failures_preserve_target"]
fn limited_write_child() {
    let root = PathBuf::from(std::env::var_os(ROOT_ENV).expect("parent fixture required"));
    assert!(
        root.file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("lcxl-text-limit-")
    );
    assert_eq!(
        std::fs::read(root.join("marker")).unwrap(),
        b"text-limit-fixture"
    );
    let change = std::env::var(CHANGE_ENV).unwrap();
    assert!(["update", "delete", "stage"].contains(&change.as_str()));
    let original = if change == "stage" {
        b"before".as_slice()
    } else {
        ORIGINAL
    };
    let replacement = "b".repeat(64 * 1024);
    let selected = root.join("selected");
    let path = selected.join("notes.txt");
    let directory = issue(&selected).unwrap();
    let target = issue(&path).unwrap();
    let mut context = test_context(&directory);
    context.data_root = root.join("data");
    let limit = libc::rlimit {
        rlim_cur: 4096,
        rlim_max: 4096,
    };
    unsafe {
        assert_ne!(libc::signal(libc::SIGXFSZ, libc::SIG_IGN), libc::SIG_ERR);
        assert_eq!(libc::setrlimit(libc::RLIMIT_FSIZE, &limit), 0);
    }
    // Prove the kernel is enforcing EFBIG, independently of the mutation result.
    let probe = root.join("write-probe");
    assert_eq!(
        std::fs::write(&probe, ORIGINAL).unwrap_err().raw_os_error(),
        Some(libc::EFBIG)
    );
    std::fs::remove_file(probe).unwrap();
    let mut submissions = 0;
    let result = mutate_text_with_submit(
        TextMutationRequest {
            directory: &directory,
            target: &target,
            expected_sha256: &digest(original),
            change: if change == "delete" {
                TextChange::Trash
            } else if change == "stage" {
                TextChange::ReplaceAll(&replacement)
            } else {
                TextChange::ReplaceAll("intended")
            },
        },
        || {},
        || {},
        || Ok(()),
        &context,
        |_, _, _, _, _| {
            submissions += 1;
            panic!("write failure must not reach submission")
        },
    );
    let error = result.err().expect("write must fail");
    assert!(
        error.message.contains(if change == "stage" {
            "write text recovery file"
        } else {
            "save private file backup"
        }),
        "{error:?}"
    );
    assert_eq!(submissions, 0);
    assert_eq!(std::fs::read(path).unwrap(), original);
    let vault = desk_file_recovery::Vault::open(&context.data_root).unwrap();
    let locked = vault.lock().unwrap();
    let records = locked.list(&context.scope, Some(&context.conversation_id));
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].change, desk_file_recovery::ChangeState::Aborted);
    assert_eq!(
        records[0].material,
        if change == "stage" {
            desk_file_recovery::MaterialState::Saved
        } else {
            desk_file_recovery::MaterialState::Preparing
        }
    );
    assert!(records[0].transaction.is_none());
    assert_eq!(
        context
            .data_root
            .join("file-recovery")
            .join(format!("{}.body", records[0].id))
            .exists(),
        change == "stage"
    );
}

#[test]
fn kernel_write_failures_preserve_target() {
    let _guard = file_store_test_lock();
    for change in ["update", "delete", "stage"] {
        let original = if change == "stage" {
            b"before".as_slice()
        } else {
            ORIGINAL
        };
        let root = tempfile::Builder::new()
            .prefix("lcxl-text-limit-")
            .tempdir()
            .unwrap();
        std::fs::write(root.path().join("marker"), b"text-limit-fixture").unwrap();
        let selected = root.path().join("selected");
        std::fs::create_dir(&selected).unwrap();
        std::fs::create_dir(root.path().join("data")).unwrap();
        let path = selected.join("notes.txt");
        std::fs::write(&path, original).unwrap();
        let before = std::fs::metadata(&path).unwrap();
        let child_name = format!(
            "{}::limited_write_child",
            module_path!().split_once("::").unwrap().1
        );
        let mut child = Process(
            Command::new(std::env::current_exe().unwrap())
                .args(["--ignored", "--exact", &child_name, "--test-threads=1"])
                .env(ROOT_ENV, root.path())
                .env(CHANGE_ENV, change)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap(),
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let status = loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                break status;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "write-limit child timed out"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        assert!(status.success(), "{change}: {status}");
        assert_eq!(std::fs::read(&path).unwrap(), original);
        let after = std::fs::metadata(&path).unwrap();
        assert_eq!(
            (before.dev(), before.ino(), before.mode()),
            (after.dev(), after.ino(), after.mode())
        );
        assert_eq!(std::fs::read_dir(selected).unwrap().count(), 1);
    }
}
