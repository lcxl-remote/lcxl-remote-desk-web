//! Abrupt child-process exit tests; all files remain inside parent-owned fixtures.
use super::*;
use std::process::{Child, Command, Stdio};

const ROOT_ENV: &str = "LCXL_NATIVE_TEXT_CRASH_ROOT";
const PHASE_ENV: &str = "LCXL_NATIVE_TEXT_CRASH_PHASE";
const CHANGE_ENV: &str = "LCXL_NATIVE_TEXT_CRASH_CHANGE";
const CRASH_EXIT: i32 = 86;

fn context(root: &Path) -> RecoveryContext {
    RecoveryContext {
        data_root: root.join("data"),
        execution_epoch: 0,
        quota: None,
        scope: desk_file_recovery::Scope {
            authority: "test".into(),
            device: "device".into(),
            os_user: "test-user".into(),
            owner: "owner".into(),
        },
        conversation_id: "crash-conversation".into(),
        operation_id: "crash-operation".into(),
        generation: "crash-generation".into(),
        _test_data: None,
    }
}

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
#[ignore = "internal crash child; run only through interrupted_transactions_preserve_target_and_fence_replay"]
fn subprocess_crash_point() {
    let root = PathBuf::from(std::env::var_os(ROOT_ENV).expect("parent fixture required"));
    assert!(
        root.file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("lcxl-text-crash-")
    );
    assert_eq!(
        std::fs::read(root.join("fixture-marker")).unwrap(),
        b"native-text-crash-test"
    );
    let phase = std::env::var(PHASE_ENV).unwrap();
    assert!(["prepared", "intent", "namespace"].contains(&phase.as_str()));
    let change = std::env::var(CHANGE_ENV).unwrap();
    assert!(["update", "delete"].contains(&change.as_str()));
    let selected = root.join("selected");
    let path = selected.join("notes.txt");
    let directory = issue(&selected).unwrap();
    let target = issue(&path).unwrap();
    let context = context(&root);
    let _receipt = mutate_text_with_submit(
        TextMutationRequest {
            directory: &directory,
            target: &target,
            expected_sha256: &digest(b"original"),
            change: if change == "delete" {
                TextChange::Trash
            } else {
                TextChange::ReplaceAll("intended")
            },
        },
        || {
            if phase == "prepared" {
                unsafe { libc::_exit(CRASH_EXIT) }
            }
        },
        || {},
        || Ok(()),
        &context,
        |parent, leaf, recovery, destination, flags| {
            if phase == "intent" {
                unsafe { libc::_exit(CRASH_EXIT) }
            }
            assert_eq!(
                unsafe {
                    libc::syscall(
                        libc::SYS_renameat2,
                        parent.as_raw_fd(),
                        leaf.as_ptr(),
                        recovery.as_raw_fd(),
                        destination.as_ptr(),
                        flags,
                    )
                },
                0
            );
            unsafe { libc::_exit(CRASH_EXIT) }
        },
    )
    .unwrap();
    panic!("crash point was not reached");
}

#[test]
fn interrupted_transactions_preserve_target_and_fence_replay() {
    let _guard = file_store_test_lock();
    for change in ["update", "delete"] {
        for phase in ["prepared", "intent", "namespace"] {
            let root = tempfile::Builder::new()
                .prefix("lcxl-text-crash-")
                .tempdir()
                .unwrap();
            std::fs::write(
                root.path().join("fixture-marker"),
                b"native-text-crash-test",
            )
            .unwrap();
            let selected = root.path().join("selected");
            std::fs::create_dir(&selected).unwrap();
            std::fs::create_dir(root.path().join("data")).unwrap();
            let path = selected.join("notes.txt");
            std::fs::write(&path, b"original").unwrap();
            let child_name = format!(
                "{}::subprocess_crash_point",
                module_path!().split_once("::").unwrap().1
            );
            let mut child = Process(
                Command::new(std::env::current_exe().unwrap())
                    .args(["--ignored", "--exact", &child_name, "--test-threads=1"])
                    .env(ROOT_ENV, root.path())
                    .env(PHASE_ENV, phase)
                    .env(CHANGE_ENV, change)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::inherit())
                    .spawn()
                    .unwrap(),
            );
            let deadline = std::time::Instant::now() + Duration::seconds(20).to_std().unwrap();
            let status = loop {
                if let Some(status) = child.0.try_wait().unwrap() {
                    break status;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "crash child timed out"
                );
                std::thread::sleep(std::time::Duration::from_millis(10));
            };
            assert_eq!(status.code(), Some(CRASH_EXIT), "{change}/{phase}");
            let expected = if phase == "namespace" {
                if change == "delete" {
                    None
                } else {
                    Some(b"intended".as_slice())
                }
            } else {
                Some(b"original".as_slice())
            };
            assert_eq!(std::fs::read(&path).ok().as_deref(), expected);
            let context = context(root.path());
            let vault = desk_file_recovery::Vault::open(&context.data_root).unwrap();
            let mut locked = vault.lock().unwrap();
            let records = locked.list(&context.scope, Some(&context.conversation_id));
            assert_eq!(records.len(), 1);
            assert_eq!(
                records[0].change,
                if phase == "prepared" {
                    desk_file_recovery::ChangeState::BackupReady
                } else {
                    desk_file_recovery::ChangeState::CommitIntent
                }
            );
            for _ in 0..2 {
                let unknown = locked
                    .recover_interrupted(Utc::now().timestamp_millis() as u64)
                    .unwrap();
                assert_eq!(unknown.len(), usize::from(phase != "prepared"));
                assert_eq!(
                    std::fs::read(&path).ok().as_deref(),
                    expected,
                    "{change}/{phase}"
                );
            }
            let records = locked.list(&context.scope, Some(&context.conversation_id));
            assert_eq!(records.len(), 1);
            if phase != "prepared" {
                assert_eq!(
                    records[0].change,
                    desk_file_recovery::ChangeState::OutcomeUnknown
                );
                assert_eq!(
                    records[0].material,
                    desk_file_recovery::MaterialState::Saved
                );
                let body = context
                    .data_root
                    .join("file-recovery")
                    .join(format!("{}.body", records[0].id));
                assert_eq!(std::fs::read(body).unwrap(), b"original");
            }
            drop(locked);
            // Even a newly selected current version cannot reuse the interrupted operation identity.
            if expected.is_none() {
                std::fs::write(&path, b"new external file").unwrap();
            }
            let current = std::fs::read(&path).unwrap();
            let directory = issue(&selected).unwrap();
            let target = issue(&path).unwrap();
            let result = mutate_text_managed(
                TextMutationRequest {
                    directory: &directory,
                    target: &target,
                    expected_sha256: &digest(&current),
                    change: TextChange::ReplaceAll("must not replay"),
                },
                || {},
                || {},
                || Ok(()),
                &context,
            );
            assert!(result.is_err(), "{change}/{phase}");
            assert_eq!(std::fs::read(&path).unwrap(), current);
        }
    }
}
