use super::*;
#[test]
fn commit_io_error_retains_recovery_before_or_after_namespace_change() {
    let _guard = file_store_test_lock();
    for (apply_change, deletion) in [(false, false), (true, false), (false, true), (true, true)] {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("notes.txt");
        std::fs::write(&path, b"original").unwrap();
        let directory = issue(root.path()).unwrap();
        let target = issue(&path).unwrap();
        let context = test_context(&directory);
        let mut submissions = 0;
        let receipt = mutate_text_with_submit(
            TextMutationRequest {
                directory: &directory,
                target: &target,
                expected_sha256: &digest(b"original"),
                change: if deletion {
                    TextChange::Trash
                } else {
                    TextChange::ReplaceAll("new")
                },
            },
            || {},
            || {},
            || Ok(()),
            &context,
            |parent, leaf, recovery, destination, flags| {
                submissions += 1;
                if apply_change {
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
                }
                Err(std::io::Error::from_raw_os_error(libc::EIO))
            },
        )
        .unwrap();
        assert_eq!(submissions, 1);
        assert!(!receipt.verified);
        assert!(receipt.file.is_none());
        assert!(receipt.recovery.cleanup_pending);
        if deletion && apply_change {
            assert!(!path.exists());
        } else {
            assert_eq!(
                std::fs::read(&path).unwrap(),
                if apply_change {
                    b"new".as_slice()
                } else {
                    b"original".as_slice()
                }
            );
        }
        assert_eq!(std::fs::read(&receipt.backup_path).unwrap(), b"original");
        let vault = desk_file_recovery::Vault::open(&context.data_root).unwrap();
        let locked = vault.lock().unwrap();
        let records = locked.list(&context.scope, Some(&context.conversation_id));
        let record = records
            .iter()
            .find(|record| record.id == receipt.recovery.recovery_id)
            .unwrap();
        assert_eq!(record.change, desk_file_recovery::ChangeState::CommitIntent);
        assert!(record.transaction.is_some());
    }
}

#[test]
fn concurrent_target_change_after_submit_does_not_publish_intended_artifact() {
    let _guard = file_store_test_lock();
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("notes.txt");
    std::fs::write(&path, b"original").unwrap();
    let directory = issue(root.path()).unwrap();
    let target = issue(&path).unwrap();
    let context = test_context(&directory);
    let receipt = mutate_text_with_submit(
        TextMutationRequest {
            directory: &directory,
            target: &target,
            expected_sha256: &digest(b"original"),
            change: TextChange::ReplaceAll("intended"),
        },
        || {},
        || {},
        || Ok(()),
        &context,
        |parent, leaf, recovery, destination, flags| {
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
            // Simulate an external writer before the post-commit readback.
            std::fs::write(&path, b"external writer")?;
            Ok(())
        },
    )
    .unwrap();
    assert!(!receipt.verified);
    assert!(receipt.file.is_none());
    assert!(receipt.recovery.cleanup_pending);
    assert_eq!(std::fs::read(&path).unwrap(), b"external writer");
    let vault = desk_file_recovery::Vault::open(&context.data_root).unwrap();
    let locked = vault.lock().unwrap();
    let records = locked.list(&context.scope, Some(&context.conversation_id));
    let record = records
        .iter()
        .find(|record| record.id == receipt.recovery.recovery_id)
        .unwrap();
    assert_eq!(record.change, desk_file_recovery::ChangeState::CommitIntent);
    let transaction = record.transaction.as_ref().unwrap();
    assert_eq!(
        std::fs::read(
            Path::new(&transaction.parent)
                .join(&transaction.directory)
                .join("replacement")
        )
        .unwrap(),
        b"original"
    );
}

#[test]
fn hard_link_is_rejected_without_changing_either_name() {
    let _guard = file_store_test_lock();
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("notes.txt");
    std::fs::write(&path, b"original").unwrap();
    std::fs::hard_link(&path, root.path().join("alias.txt")).unwrap();
    let directory = issue(root.path()).unwrap();
    let file = issue(&path).unwrap();
    assert!(
        mutate_text(
            &directory,
            &file,
            &digest(b"original"),
            TextChange::ReplaceAll("new")
        )
        .is_err()
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"original");
    assert_eq!(
        std::fs::read(root.path().join("alias.txt")).unwrap(),
        b"original"
    );
}

#[test]
fn update_and_delete_verify_content_and_keep_original_backup() {
    use std::os::unix::fs::PermissionsExt;
    let _guard = file_store_test_lock();
    for delete in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("notes.txt");
        std::fs::write(&path, b"version one").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        let directory = issue(root.path()).unwrap();
        let target = issue(&path).unwrap();
        let receipt = mutate_text(
            &directory,
            &target,
            &digest(b"version one"),
            if delete {
                TextChange::Trash
            } else {
                TextChange::ReplaceOnce {
                    before: "one",
                    after: "two",
                }
            },
        )
        .unwrap();
        assert!(receipt.verified);
        assert!(!receipt.recovery.cleanup_pending);
        assert_eq!(std::fs::read(&receipt.backup_path).unwrap(), b"version one");
        if delete {
            assert!(!path.exists());
            assert!(receipt.file.is_none());
        } else {
            assert_eq!(std::fs::read(&path).unwrap(), b"version two");
            assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o640);
            let updated = receipt.file.unwrap();
            assert_eq!(updated.sha256, digest(b"version two"));
            assert_eq!(
                read_verified_bytes(&updated.file, updated.byte_len)
                    .unwrap()
                    .bytes,
                b"version two"
            );
        }
    }
}

#[test]
fn precommit_file_changes_never_reach_native_submission() {
    use std::{
        cell::Cell,
        os::unix::fs::{PermissionsExt, symlink},
    };
    let _guard = file_store_test_lock();
    for change in ["inode", "content", "symlink", "hardlink", "mode"] {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("notes.txt");
        let other = root.path().join("external.txt");
        std::fs::write(&path, b"original").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::write(&other, b"external").unwrap();
        let directory = issue(root.path()).unwrap();
        let target = issue(&path).unwrap();
        let context = test_context(&directory);
        let submissions = Cell::new(0);
        let result = mutate_text_with_submit(
            TextMutationRequest {
                directory: &directory,
                target: &target,
                expected_sha256: &digest(b"original"),
                change: TextChange::ReplaceAll("must not publish"),
            },
            || match change {
                "inode" => std::fs::rename(&other, &path).unwrap(),
                "content" => std::fs::write(&path, b"external").unwrap(),
                "symlink" => {
                    std::fs::remove_file(&path).unwrap();
                    symlink(&other, &path).unwrap();
                }
                "hardlink" => std::fs::hard_link(&path, root.path().join("alias.txt")).unwrap(),
                "mode" => {
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap()
                }
                _ => unreachable!(),
            },
            || {},
            || Ok(()),
            &context,
            |_, _, _, _, _| {
                submissions.set(submissions.get() + 1);
                Ok(())
            },
        );
        assert!(result.is_err(), "{change}");
        assert_eq!(submissions.get(), 0, "{change}");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            if matches!(change, "inode" | "content" | "symlink") {
                b"external".as_slice()
            } else {
                b"original".as_slice()
            },
            "{change}"
        );
        if change == "symlink" {
            assert!(path.is_symlink());
        }
        if change == "hardlink" {
            assert_eq!(
                std::fs::read(root.path().join("alias.txt")).unwrap(),
                b"original"
            );
        }
    }
}

#[test]
fn revoked_commit_guard_preserves_target_and_aborts_intent() {
    use std::cell::Cell;
    let _guard = file_store_test_lock();
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("notes.txt");
    std::fs::write(&path, b"original").unwrap();
    let directory = issue(root.path()).unwrap();
    let target = issue(&path).unwrap();
    let context = test_context(&directory);
    let submissions = Cell::new(0);
    let result = mutate_text_with_submit(
        TextMutationRequest {
            directory: &directory,
            target: &target,
            expected_sha256: &digest(b"original"),
            change: TextChange::Trash,
        },
        || {},
        || {},
        || Err(conflict()),
        &context,
        |_, _, _, _, _| {
            submissions.set(submissions.get() + 1);
            Ok(())
        },
    );
    assert!(result.is_err());
    assert_eq!(submissions.get(), 0);
    assert_eq!(std::fs::read(&path).unwrap(), b"original");
    let vault = desk_file_recovery::Vault::open(&context.data_root).unwrap();
    let locked = vault.lock().unwrap();
    let records = locked.list(&context.scope, Some(&context.conversation_id));
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].change, desk_file_recovery::ChangeState::Aborted);
    assert!(records[0].transaction.is_none());
}

#[test]
fn replaced_parent_is_rejected_and_cleanup_does_not_touch_the_new_directory() {
    use std::cell::Cell;
    let _guard = file_store_test_lock();
    let root = tempfile::tempdir().unwrap();
    let selected = root.path().join("selected");
    let moved = root.path().join("moved");
    std::fs::create_dir(&selected).unwrap();
    let path = selected.join("notes.txt");
    std::fs::write(&path, b"original").unwrap();
    let directory = issue(&selected).unwrap();
    let target = issue(&path).unwrap();
    let context = test_context(&directory);
    let submissions = Cell::new(0);
    let sentinel = std::cell::RefCell::new(None);
    let result = mutate_text_with_submit(
        TextMutationRequest {
            directory: &directory,
            target: &target,
            expected_sha256: &digest(b"original"),
            change: TextChange::ReplaceAll("new"),
        },
        || {
            std::fs::rename(&selected, &moved).unwrap();
            std::fs::create_dir(&selected).unwrap();
            std::fs::write(&path, b"unrelated target").unwrap();
            let transaction = std::fs::read_dir(&moved)
                .unwrap()
                .map(Result::unwrap)
                .find(|entry| entry.file_type().unwrap().is_dir())
                .unwrap();
            let unrelated = selected.join(transaction.file_name());
            std::fs::create_dir(&unrelated).unwrap();
            let file = unrelated.join("replacement");
            std::fs::write(&file, b"unrelated recovery name").unwrap();
            *sentinel.borrow_mut() = Some(file);
        },
        || {},
        || Ok(()),
        &context,
        |_, _, _, _, _| {
            submissions.set(submissions.get() + 1);
            Ok(())
        },
    );
    assert!(result.is_err());
    assert_eq!(submissions.get(), 0);
    assert_eq!(std::fs::read(path).unwrap(), b"unrelated target");
    assert_eq!(std::fs::read(moved.join("notes.txt")).unwrap(), b"original");
    assert_eq!(
        std::fs::read(sentinel.borrow().as_ref().unwrap()).unwrap(),
        b"unrelated recovery name"
    );
}

#[test]
fn last_moment_inode_replacement_retains_both_versions_without_claiming_success() {
    let _guard = file_store_test_lock();
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("notes.txt");
    let external = root.path().join("external.txt");
    std::fs::write(&path, b"original").unwrap();
    std::fs::write(&external, b"external replacement").unwrap();
    let directory = issue(root.path()).unwrap();
    let target = issue(&path).unwrap();
    let context = test_context(&directory);
    let receipt = mutate_text_with_submit(
        TextMutationRequest {
            directory: &directory,
            target: &target,
            expected_sha256: &digest(b"original"),
            change: TextChange::ReplaceAll("intended"),
        },
        || {},
        || {},
        || Ok(()),
        &context,
        |parent, leaf, recovery, destination, flags| {
            // Rename has no inode/content CAS: expose the gap after the last check.
            std::fs::rename(&external, &path)?;
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
            Ok(())
        },
    )
    .unwrap();
    assert!(!receipt.verified);
    assert!(receipt.file.is_none());
    assert!(receipt.recovery.cleanup_pending);
    assert_eq!(std::fs::read(&path).unwrap(), b"intended");
    assert_eq!(std::fs::read(&receipt.backup_path).unwrap(), b"original");
    let vault = desk_file_recovery::Vault::open(&context.data_root).unwrap();
    let locked = vault.lock().unwrap();
    let records = locked.list(&context.scope, Some(&context.conversation_id));
    let record = records
        .iter()
        .find(|record| record.id == receipt.recovery.recovery_id)
        .unwrap();
    assert_eq!(record.change, desk_file_recovery::ChangeState::CommitIntent);
    let transaction = record.transaction.as_ref().unwrap();
    assert_eq!(
        std::fs::read(
            Path::new(&transaction.parent)
                .join(&transaction.directory)
                .join("replacement")
        )
        .unwrap(),
        b"external replacement"
    );
}

#[path = "crash.rs"]
mod crash;

#[path = "storage_failure.rs"]
mod storage_failure;

#[path = "full_disk.rs"]
mod full_disk;
