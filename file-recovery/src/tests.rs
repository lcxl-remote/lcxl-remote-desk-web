use super::*;
fn scope() -> Scope {
    Scope {
        authority: "central-a".into(),
        device: "device-a".into(),
        os_user: "user-a".into(),
        owner: "owner-a".into(),
    }
}
fn save(v: &mut LockedVault, conversation: &str, operation: &str) -> Record {
    v.backup(BackupRequest {
        scope: scope(),
        conversation,
        operation,
        generation: "generation",
        file_name: "notes.txt",
        content: b"before",
        metadata: b"{}",
        now_ms: 1000,
    })
    .unwrap()
}
fn fixture() -> (tempfile::TempDir, Vault) {
    let root = tempfile::tempdir().unwrap();
    let vault = Vault::open(root.path()).unwrap();
    (root, vault)
}

#[test]
fn maintenance_skips_an_active_transaction_lock() {
    let (_root, vault) = fixture();
    let lock = vault.lock().unwrap();
    assert!(vault.try_lock().unwrap().is_none());
    drop(lock);
    assert!(vault.try_lock().unwrap().is_some());
}

#[test]
fn epoch_cleanup_waits_for_material_quota_acknowledgment() {
    let (_root, vault) = fixture();
    let mut v = vault.lock().unwrap();
    let record = v
        .backup_with_reservation(
            BackupRequest {
                scope: scope(),
                conversation: "conversation",
                operation: "operation",
                generation: "generation",
                file_name: "notes.txt",
                content: b"before",
                metadata: b"{}",
                now_ms: 1000,
            },
            |_| Ok(()),
        )
        .unwrap();
    v.transition(&scope(), &record.id, ChangeState::Aborted)
        .unwrap();
    v.discard(&scope(), "conversation", &record.id, 1001)
        .unwrap();
    assert_eq!(
        v.prepare_epoch_cleanup(&scope().os_user)
            .unwrap_err()
            .kind(),
        io::ErrorKind::WouldBlock
    );
    assert!(v.epoch_cleanup_state().is_none());
    v.acknowledge_quota_cleanup(&scope(), &record.id).unwrap();
    assert!(v.prepare_epoch_cleanup(&scope().os_user).unwrap().is_some());
}

#[test]
fn epoch_cleanup_restarts_between_phases_and_keeps_unknown_material() {
    let (root, vault) = fixture();
    let device = quota::DeviceQuota::open(root.path()).unwrap();
    let mut v = vault.lock().unwrap();
    let mut q = device.lock().unwrap();
    let index = save(&mut v, "conversation", "old-index");
    let retained = save(&mut v, "conversation", "unknown");
    let key = |record: &Record| {
        let mut key = quota::QuotaKey::new(
            &record.scope,
            &record.conversation,
            &record.operation,
            &record.generation,
        )
        .unwrap();
        key.epoch = record.storage_epoch;
        key
    };
    q.reserve(key(&index), index.bytes, 10_000, 1000).unwrap();
    q.reserve(key(&retained), retained.bytes, 10_000, 1000)
        .unwrap();
    v.transition(&scope(), &index.id, ChangeState::Aborted)
        .unwrap();
    v.discard(&scope(), "conversation", &index.id, 1001)
        .unwrap();
    q.release_with_retained_index(
        &key(&index),
        LockedVault::retained_index_bytes(
            &v.list(&scope(), None)
                .into_iter()
                .find(|record| record.id == index.id)
                .unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    v.transition(&scope(), &retained.id, ChangeState::CommitIntent)
        .unwrap();
    v.recover_interrupted(1002).unwrap();
    assert!(v.prepare_epoch_cleanup("another-user").is_err());
    let state = v.prepare_epoch_cleanup(&scope().os_user).unwrap().unwrap();
    assert_eq!(
        state,
        EpochCleanupState {
            previous: 0,
            next: 1,
            pruned: false
        }
    );
    assert!(v.require_execution_epoch(0).is_err());
    assert!(v.finish_epoch_cleanup(1).is_err());
    drop(v);
    drop(q);
    let mut v = vault.lock().unwrap();
    let mut q = device.lock().unwrap();
    q.begin_epoch_cleanup(&scope().os_user, state.previous)
        .unwrap();
    let charged = q.used_bytes();
    assert!(v.prune_epoch_indexes(&scope().os_user, 2).is_err());
    v.prune_epoch_indexes(&scope().os_user, 1).unwrap();
    assert_eq!(
        q.used_bytes(),
        charged,
        "local pruning does not itself release shared quota"
    );
    assert_eq!(v.list(&scope(), None).len(), 1);
    assert_eq!(v.export(&scope(), &retained.id, 1003).unwrap().1, b"before");
    assert!(v.require_execution_epoch(1).is_err());
    drop(v);
    drop(q);
    let mut v = vault.lock().unwrap();
    let mut q = device.lock().unwrap();
    assert!(v.epoch_cleanup_state().unwrap().pruned);
    q.finish_epoch_cleanup(&scope().os_user, 1).unwrap();
    assert!(q.used_bytes() < charged);
    v.finish_epoch_cleanup(1).unwrap();
    assert!(v.require_execution_epoch(0).is_err());
    v.require_execution_epoch(1).unwrap();
    let new = v
        .backup_with_reservation_in_epoch(
            BackupRequest {
                scope: scope(),
                conversation: "conversation",
                operation: "new",
                generation: "new-generation",
                file_name: "notes.txt",
                content: b"new before",
                metadata: b"{}",
                now_ms: 1003,
            },
            1,
            |record| q.reserve(key(record), record.bytes, 10_000, 1003),
        )
        .unwrap();
    assert_eq!(new.storage_epoch, 1);
    assert!(
        v.backup_with_reservation_in_epoch(
            BackupRequest {
                scope: scope(),
                conversation: "conversation",
                operation: "old-index",
                generation: "replay",
                file_name: "notes.txt",
                content: b"replay",
                metadata: b"{}",
                now_ms: 1003,
            },
            0,
            |_| panic!("old epoch must be refused before reservation")
        )
        .is_err()
    );
}

#[test]
#[ignore = "requires access to the host boot identity and continuous clock"]
fn confirmed_clock_recovery_preserves_timestamps_and_allows_explicit_discard() {
    let (_root, vault) = fixture();
    let now = || {
        u64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis(),
        )
        .unwrap()
    };
    let mut locked = vault.lock().unwrap();
    let future = now() + 86_400_000;
    let record = locked
        .backup(BackupRequest {
            scope: scope(),
            conversation: "conversation",
            operation: "operation",
            generation: "generation",
            file_name: "notes.txt",
            content: b"before",
            metadata: b"{}",
            now_ms: future,
        })
        .unwrap();
    locked
        .transition(&scope(), &record.id, ChangeState::Aborted)
        .unwrap();
    locked.ledger.last_cleanup_at_ms = future;
    locked.ledger.cleanup_clock_paused = true;
    locked.persist().unwrap();
    assert!(locked.acknowledge_clock(future).is_err());
    assert!(locked.cleanup_clock_paused());
    locked.acknowledge_clock(now()).unwrap();
    assert!(!locked.cleanup_clock_paused());
    drop(locked);
    let mut reopened = vault.lock().unwrap();
    reopened.observe_system_clock().unwrap();
    reopened.cleanup(now()).unwrap();
    let retained = &reopened.list(&scope(), None)[0];
    assert_eq!(retained.created_at_ms, future);
    assert_eq!(retained.expires_at_ms, record.expires_at_ms);
    assert_eq!(retained.material, MaterialState::Saved);
    reopened
        .discard(&scope(), "conversation", &record.id, now())
        .unwrap();
    assert_eq!(
        reopened.list(&scope(), None)[0].material,
        MaterialState::Purged
    );
    assert_eq!(
        reopened.list(&scope(), None)[0].change,
        ChangeState::Aborted
    );
}

#[test]
fn persisted_forward_clock_pause_prevents_expired_material_cleanup() {
    let (_root, vault) = fixture();
    let mut locked = vault.lock().unwrap();
    let record = save(&mut locked, "conversation", "operation");
    locked
        .transition(&scope(), &record.id, ChangeState::Aborted)
        .unwrap();
    locked.ledger.clock_guard.blocked = true;
    locked.persist().unwrap();
    drop(locked);
    let mut reopened = vault.lock().unwrap();
    assert!(reopened.cleanup(record.expires_at_ms + 1).is_err());
    assert!(reopened.cleanup_clock_paused());
    assert_eq!(
        reopened.list(&scope(), None)[0].material,
        MaterialState::Saved
    );
}

#[test]
fn explicit_discard_requires_exact_scope_and_settled_operation_and_fences_replay() {
    let (_root, vault) = fixture();
    let mut v = vault.lock().unwrap();
    let record = save(&mut v, "conversation", "operation");
    assert_eq!(
        v.discard(&scope(), "conversation", &record.id, 1001)
            .unwrap_err()
            .kind(),
        io::ErrorKind::WouldBlock
    );
    v.transition(&scope(), &record.id, ChangeState::CommitIntent)
        .unwrap();
    v.recover_interrupted(1002).unwrap();
    let mut wrong_owner = scope();
    wrong_owner.owner = "someone-else".into();
    assert!(
        v.discard(&wrong_owner, "conversation", &record.id, 1003)
            .is_err()
    );
    assert!(v.discard(&scope(), "different", &record.id, 1003).is_err());
    assert!(
        v.export(&scope(), &record.id, record.expires_at_ms + 1)
            .is_ok()
    );
    v.discard(
        &scope(),
        "conversation",
        &record.id,
        record.expires_at_ms + 1,
    )
    .unwrap();
    drop(v);
    let mut reopened = vault.lock().unwrap();
    let retained = &reopened.list(&scope(), None)[0];
    assert_eq!(retained.change, ChangeState::OutcomeUnknown);
    assert_eq!(retained.material, MaterialState::Purged);
    assert!(retained.discard_requested);
    reopened
        .discard(
            &scope(),
            "conversation",
            &record.id,
            record.expires_at_ms + 2,
        )
        .unwrap();
    assert!(
        reopened
            .export(&scope(), &record.id, record.expires_at_ms + 2)
            .is_err()
    );
    assert!(
        reopened
            .backup(BackupRequest {
                scope: scope(),
                conversation: "conversation",
                operation: "operation",
                generation: "new-generation",
                file_name: "notes.txt",
                content: b"replacement",
                metadata: b"{}",
                now_ms: record.expires_at_ms + 2,
            })
            .is_err()
    );
}

#[test]
fn clock_rollback_pauses_recovery_across_restart_until_time_catches_up() {
    let (_root, vault) = fixture();
    let mut locked = vault.lock().unwrap();
    let record = save(&mut locked, "conversation", "operation");
    locked.cleanup(2000).unwrap();
    drop(locked);

    let mut reopened = vault.lock().unwrap();
    let error = reopened.recover_interrupted(1500).unwrap_err();
    assert!(error.to_string().contains("system clock changed"));
    assert_eq!(reopened.pending_cleanup_count(), 1);
    assert_eq!(
        reopened.list(&scope(), None)[0].change,
        ChangeState::BackupReady
    );
    assert_eq!(
        reopened.export(&scope(), &record.id, 1500).unwrap().1,
        b"before"
    );
    drop(reopened);

    let mut retried = vault.lock().unwrap();
    assert!(retried.cleanup(1600).is_err());
    retried.recover_interrupted(2001).unwrap();
    assert_eq!(retried.pending_cleanup_count(), 0);
    assert_eq!(
        retried.list(&scope(), None)[0].material,
        MaterialState::Purged
    );
}

#[test]
fn clock_before_backup_creation_pauses_first_cleanup() {
    let (_root, vault) = fixture();
    let mut locked = vault.lock().unwrap();
    let record = save(&mut locked, "conversation", "operation");
    assert!(locked.recover_interrupted(999).is_err());
    assert_eq!(
        locked.list(&scope(), None)[0].change,
        ChangeState::BackupReady
    );
    assert_eq!(
        locked.export(&scope(), &record.id, 1000).unwrap().1,
        b"before"
    );
}

#[test]
fn recovery_package_uses_fixed_names_and_enforces_scope_and_lifetime() {
    let (_root, vault) = fixture();
    let mut v = vault.lock().unwrap();
    let r = save(&mut v, "s", "a");
    let package = v.export_package(&scope(), &r.id, 1001).unwrap();
    let mut archive = zip::ZipArchive::new(io::Cursor::new(package)).unwrap();
    assert_eq!(archive.len(), 2);
    let mut content = String::new();
    archive
        .by_name("before.txt")
        .unwrap()
        .read_to_string(&mut content)
        .unwrap();
    assert_eq!(content, "before");
    let mut manifest = String::new();
    archive
        .by_name("metadata.json")
        .unwrap()
        .read_to_string(&mut manifest)
        .unwrap();
    let metadata: serde_json::Value = serde_json::from_str(&manifest).unwrap();
    assert_eq!(metadata["file_name"], "notes.txt");
    assert!(metadata.get("scope").is_none());
    let mut other = scope();
    other.authority = "other-central".into();
    assert!(v.export_package(&other, &r.id, 1001).is_err());
    assert!(v.export_package(&scope(), &r.id, r.expires_at_ms).is_err());
    v.delete_conversation(&scope(), "s", 1002).unwrap();
    assert!(v.export_package(&scope(), &r.id, 1003).is_err());
}

#[cfg(unix)]
#[test]
fn maintenance_retries_registered_temporary_files_before_backup_expiry() {
    use std::os::unix::fs::MetadataExt;
    let (_root, vault) = fixture();
    let user_dir = tempfile::tempdir().unwrap();
    let original = user_dir.path().join("document.txt");
    fs::write(&original, b"before").unwrap();
    let parent = fs::metadata(user_dir.path()).unwrap();
    let original_inode = fs::metadata(&original).unwrap().ino();
    let mut v = vault.lock().unwrap();
    let r = save(&mut v, "s", "a");
    let name = v
        .plan_transaction(
            &scope(),
            &r.id,
            user_dir.path().to_str().unwrap(),
            parent.dev(),
            parent.ino(),
            original_inode,
        )
        .unwrap();
    let directory = user_dir.path().join(name);
    fs::create_dir(&directory).unwrap();
    fs::write(directory.join("replacement"), b"after").unwrap();
    let staged_inode = fs::metadata(directory.join("replacement")).unwrap().ino();
    v.register_transaction(
        &scope(),
        &r.id,
        fs::metadata(&directory).unwrap().ino(),
        Some(staged_inode),
    )
    .unwrap();
    // Simulate a worker stopping before commit; only its staged file is removed.
    drop(v);
    let mut v = vault.lock().unwrap();
    assert!(v.recover_interrupted(1001).unwrap().is_empty());
    assert!(!directory.exists());
    assert_eq!(fs::read(&original).unwrap(), b"before");
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "creates and mounts a disposable APFS disk image with hdiutil"]
fn detached_volume_retains_transaction_and_remount_retries_cleanup() {
    use std::{os::unix::fs::MetadataExt, process::Command};
    struct ImageMount {
        mount: std::path::PathBuf,
        attached: bool,
    }
    impl Drop for ImageMount {
        fn drop(&mut self) {
            if self.attached {
                let _ = Command::new("hdiutil")
                    .arg("detach")
                    .arg(&self.mount)
                    .output();
            }
        }
    }
    let temp = tempfile::tempdir().unwrap();
    let image = temp.path().join("recovery-test.dmg");
    let mount = temp.path().join("mounted");
    fs::create_dir(&mount).unwrap();
    let created = Command::new("hdiutil")
        .args([
            "create",
            "-size",
            "64m",
            "-fs",
            "APFS",
            "-volname",
            "RecoveryTest",
        ])
        .arg(&image)
        .output()
        .unwrap();
    assert!(
        created.status.success(),
        "image creation failed: {}",
        String::from_utf8_lossy(&created.stderr)
    );
    let attach = || {
        let result = Command::new("hdiutil")
            .arg("attach")
            .arg(&image)
            .args(["-nobrowse", "-noautoopen", "-mountpoint"])
            .arg(&mount)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "image attach failed: {}",
            String::from_utf8_lossy(&result.stderr)
        );
    };
    attach();
    let mut guard = ImageMount {
        mount: mount.clone(),
        attached: true,
    };
    let parent = mount.join("work");
    fs::create_dir(&parent).unwrap();
    let original = parent.join("document.txt");
    fs::write(&original, b"unchanged").unwrap();
    let pm = fs::metadata(&parent).unwrap();
    let (_root, vault) = fixture();
    let mut v = vault.lock().unwrap();
    let record = save(&mut v, "conversation", "volume-test");
    let name = v
        .plan_transaction(
            &scope(),
            &record.id,
            parent.to_str().unwrap(),
            pm.dev(),
            pm.ino(),
            fs::metadata(&original).unwrap().ino(),
        )
        .unwrap();
    let transaction = parent.join(name);
    fs::create_dir(&transaction).unwrap();
    fs::write(transaction.join("replacement"), b"staged").unwrap();
    v.register_transaction(
        &scope(),
        &record.id,
        fs::metadata(&transaction).unwrap().ino(),
        Some(fs::metadata(transaction.join("replacement")).unwrap().ino()),
    )
    .unwrap();
    drop(v);
    let detached = Command::new("hdiutil")
        .arg("detach")
        .arg(&mount)
        .output()
        .unwrap();
    assert!(detached.status.success(), "image detach failed");
    guard.attached = false;
    let mut v = vault.lock().unwrap();
    assert!(!v.settle_transaction(&scope(), &record.id).unwrap());
    assert!(
        v.list(&scope(), Some("conversation"))[0]
            .transaction
            .is_some()
    );
    drop(v);
    // Occupy the former device number before remounting the original volume.
    let blocker_image = temp.path().join("blocker.dmg");
    let blocker_mount = temp.path().join("blocker");
    fs::create_dir(&blocker_mount).unwrap();
    assert!(
        Command::new("hdiutil")
            .args([
                "create",
                "-size",
                "64m",
                "-fs",
                "APFS",
                "-volname",
                "RecoveryBlocker"
            ])
            .arg(&blocker_image)
            .output()
            .unwrap()
            .status
            .success()
    );
    assert!(
        Command::new("hdiutil")
            .arg("attach")
            .arg(&blocker_image)
            .args(["-nobrowse", "-noautoopen", "-mountpoint"])
            .arg(&blocker_mount)
            .output()
            .unwrap()
            .status
            .success()
    );
    let _blocker_guard = ImageMount {
        mount: blocker_mount,
        attached: true,
    };
    attach();
    guard.attached = true;
    assert_ne!(
        fs::metadata(&parent).unwrap().dev(),
        pm.dev(),
        "test must exercise a reassigned device number"
    );
    let mut v = vault.lock().unwrap();
    assert!(
        v.settle_transaction(&scope(), &record.id).unwrap(),
        "remounted transaction must remain safely addressable"
    );
    assert!(!transaction.exists());
    assert_eq!(fs::read(&original).unwrap(), b"unchanged");
}

#[cfg(unix)]
#[test]
fn missing_commit_result_cleans_only_registered_temporary_objects() {
    use std::os::unix::fs::MetadataExt;
    for committed in [false, true] {
        let (_root, vault) = fixture();
        let user_dir = tempfile::tempdir().unwrap();
        let original = user_dir.path().join("document.txt");
        fs::write(&original, b"before").unwrap();
        let parent = fs::metadata(user_dir.path()).unwrap();
        let mut v = vault.lock().unwrap();
        let r = save(&mut v, "s", "unknown");
        let name = v
            .plan_transaction(
                &scope(),
                &r.id,
                user_dir.path().to_str().unwrap(),
                parent.dev(),
                parent.ino(),
                fs::metadata(&original).unwrap().ino(),
            )
            .unwrap();
        let directory = user_dir.path().join(name);
        fs::create_dir(&directory).unwrap();
        let replacement = directory.join("replacement");
        fs::write(&replacement, b"after").unwrap();
        v.register_transaction(
            &scope(),
            &r.id,
            fs::metadata(&directory).unwrap().ino(),
            Some(fs::metadata(&replacement).unwrap().ino()),
        )
        .unwrap();
        v.transition(&scope(), &r.id, ChangeState::CommitIntent)
            .unwrap();
        if committed {
            fs::rename(&replacement, &original).unwrap();
        }
        drop(v);
        let mut v = vault.lock().unwrap();
        let unknown = v.recover_interrupted(1001).unwrap();
        assert_eq!(unknown.len(), 1);
        assert_eq!(unknown[0].change, ChangeState::OutcomeUnknown);
        assert_eq!(
            fs::read(&original).unwrap(),
            if committed {
                b"after".as_slice()
            } else {
                b"before".as_slice()
            }
        );
        assert!(!directory.exists());
        assert!(v.export(&scope(), &r.id, 1002).is_ok());
    }
}

#[cfg(unix)]
#[test]
fn cleanup_never_deletes_replaced_transaction_objects() {
    use std::os::unix::fs::MetadataExt;
    let (_root, vault) = fixture();
    let user_dir = tempfile::tempdir().unwrap();
    let parent = fs::metadata(user_dir.path()).unwrap();
    let mut v = vault.lock().unwrap();
    let r = save(&mut v, "s", "a");
    let name = v
        .plan_transaction(
            &scope(),
            &r.id,
            user_dir.path().to_str().unwrap(),
            parent.dev(),
            parent.ino(),
            0,
        )
        .unwrap();
    let directory = user_dir.path().join(name);
    fs::create_dir(&directory).unwrap();
    let staged = directory.join("replacement");
    fs::write(&staged, b"staged").unwrap();
    v.register_transaction(
        &scope(),
        &r.id,
        fs::metadata(&directory).unwrap().ino(),
        Some(fs::metadata(&staged).unwrap().ino()),
    )
    .unwrap();
    fs::rename(&staged, user_dir.path().join("moved-staged")).unwrap();
    fs::write(&staged, b"unrelated user content").unwrap();
    v.transition(&scope(), &r.id, ChangeState::Aborted).unwrap();
    v.recover_interrupted(1001).unwrap();
    assert_eq!(fs::read(&staged).unwrap(), b"unrelated user content");
    assert!(v.list(&scope(), None)[0].cleanup_error.is_some());
    v.discard(&scope(), "s", &r.id, 1002).unwrap();
    assert_eq!(fs::read(&staged).unwrap(), b"unrelated user content");
    assert_ne!(v.list(&scope(), None)[0].material, MaterialState::Purged);
    assert!(v.list(&scope(), None)[0].discard_requested);
}
#[test]
fn private_persistent_backup() {
    let (root, vault) = fixture();
    let record = save(&mut vault.lock().unwrap(), "s", "a");
    let reopened = vault.lock().unwrap();
    let (actual, bytes, metadata) = reopened.export(&scope(), &record.id, 1001).unwrap();
    assert_eq!(bytes, b"before");
    assert_eq!(metadata, b"{}");
    assert_eq!(actual.change, ChangeState::BackupReady);
    assert_eq!(actual.sha256, hash(b"before"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(root.path().join("file-recovery"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(
                root.path()
                    .join("file-recovery")
                    .join(format!("{}.body", record.id))
            )
            .unwrap()
            .permissions()
            .mode()
                & 0o777,
            0o600
        );
    }
}
#[test]
fn deletion_cleans_related_backups_and_fences_late_records() {
    let (_root, vault) = fixture();
    let mut v = vault.lock().unwrap();
    let first = save(&mut v, "s", "a");
    let second = save(&mut v, "s", "b");
    let other = save(&mut v, "other", "c");
    v.delete_conversation(&scope(), "s", 2000).unwrap();
    for record in [&first, &second] {
        for suffix in ["body", "metadata"] {
            assert!(!vault.root.join(format!("{}.{suffix}", record.id)).exists());
        }
    }
    assert!(v.cleanup_complete(&scope(), "s"));
    assert!(v.export(&scope(), &first.id, 2000).is_err());
    assert!(v.export(&scope(), &other.id, 2000).is_ok());
    assert!(
        v.backup(BackupRequest {
            scope: scope(),
            conversation: "s",
            operation: "late",
            generation: "g",
            file_name: "a.txt",
            content: b"late",
            metadata: b"{}",
            now_ms: 2000
        })
        .is_err()
    );
    assert!(
        v.list(&scope(), Some("s"))
            .iter()
            .all(|r| r.material == MaterialState::Purged)
    );
    drop(v);
    assert!(vault.lock().unwrap().is_deleted(&scope(), "s"));
}
#[test]
fn domains_devices_users_and_owners_are_isolated() {
    let (_root, vault) = fixture();
    let mut v = vault.lock().unwrap();
    let first = save(&mut v, "s", "a");
    for field in 0..4 {
        let mut other = scope();
        match field {
            0 => other.authority = "b".into(),
            1 => other.device = "b".into(),
            2 => other.os_user = "b".into(),
            _ => other.owner = "b".into(),
        }
        assert!(v.export(&other, &first.id, 1001).is_err());
        v.delete_conversation(&other, "s", 1002).unwrap();
        assert!(v.export(&scope(), &first.id, 1003).is_ok());
    }
}
#[test]
fn duplicate_operation_and_invalid_transitions_are_rejected() {
    let (_root, vault) = fixture();
    let mut v = vault.lock().unwrap();
    let r = save(&mut v, "s", "a");
    assert_eq!(
        v.backup(BackupRequest {
            scope: scope(),
            conversation: "s",
            operation: "a",
            generation: "new-generation",
            file_name: "a.txt",
            content: b"new",
            metadata: b"{}",
            now_ms: 2000
        })
        .unwrap_err()
        .kind(),
        io::ErrorKind::AlreadyExists
    );
    assert!(
        v.transition(&scope(), &r.id, ChangeState::Succeeded)
            .is_err()
    );
    v.transition(&scope(), &r.id, ChangeState::CommitIntent)
        .unwrap();
    drop(v);
    let mut v = vault.lock().unwrap();
    assert_eq!(v.list(&scope(), None)[0].change, ChangeState::CommitIntent);
    v.transition(&scope(), &r.id, ChangeState::Succeeded)
        .unwrap();
    assert!(
        v.transition(&scope(), &r.id, ChangeState::CommitIntent)
            .is_err()
    );
}
#[test]
fn quota_includes_metadata_without_evicting_unexpired_material() {
    let (_root, vault) = fixture();
    let mut v = vault.lock().unwrap();
    v.set_policy(Policy {
        retention_days: 7,
        max_bytes: 1024 * 1024,
    })
    .unwrap();
    for index in 0..2 {
        v.backup(BackupRequest {
            scope: scope(),
            conversation: "s",
            operation: &index.to_string(),
            generation: "g",
            file_name: "a.txt",
            content: &[b'x'; MAX_TEXT_BYTES],
            metadata: &vec![b'm'; MAX_METADATA_BYTES],
            now_ms: 1000,
        })
        .unwrap();
    }
    let before = v.used_bytes();
    assert_eq!(
        v.backup(BackupRequest {
            scope: scope(),
            conversation: "s",
            operation: "overflow",
            generation: "g",
            file_name: "a.txt",
            content: &[b'x'; MAX_TEXT_BYTES],
            metadata: &vec![b'm'; MAX_METADATA_BYTES],
            now_ms: 1000
        })
        .unwrap_err()
        .kind(),
        io::ErrorKind::StorageFull
    );
    assert_eq!(v.used_bytes(), before);
    assert_eq!(v.list(&scope(), None).len(), 2);
}
#[test]
fn retention_uses_original_timestamp_and_preserves_unknown_commit() {
    let (_root, vault) = fixture();
    let mut v = vault.lock().unwrap();
    let done = save(&mut v, "s", "done");
    let unknown = save(&mut v, "s", "unknown");
    v.transition(&scope(), &done.id, ChangeState::CommitIntent)
        .unwrap();
    v.transition(&scope(), &done.id, ChangeState::Succeeded)
        .unwrap();
    v.transition(&scope(), &unknown.id, ChangeState::CommitIntent)
        .unwrap();
    v.set_policy(Policy {
        retention_days: 1,
        max_bytes: 1024 * 1024,
    })
    .unwrap();
    let expiry = 1000 + 86_400_000;
    v.cleanup(expiry - 1).unwrap();
    assert!(v.export(&scope(), &done.id, expiry - 1).is_ok());
    v.cleanup(expiry).unwrap();
    let rows = v.list(&scope(), None);
    assert_eq!(
        rows.iter().find(|r| r.id == done.id).unwrap().material,
        MaterialState::Purged
    );
    assert_eq!(
        rows.iter().find(|r| r.id == unknown.id).unwrap().material,
        MaterialState::Saved
    );
}
#[cfg(unix)]
#[test]
fn cleanup_rejects_symlinks_and_retains_retryable_error() {
    let (root, vault) = fixture();
    let mut v = vault.lock().unwrap();
    let r = save(&mut v, "s", "a");
    let outside = root.path().join("user-file");
    fs::write(&outside, b"keep").unwrap();
    let body = root
        .path()
        .join("file-recovery")
        .join(format!("{}.body", r.id));
    fs::remove_file(&body).unwrap();
    std::os::unix::fs::symlink(&outside, &body).unwrap();
    v.delete_conversation(&scope(), "s", 2000).unwrap();
    assert_eq!(fs::read(&outside).unwrap(), b"keep");
    assert!(v.list(&scope(), None)[0].cleanup_error.is_some());
    assert_eq!(v.list(&scope(), None)[0].material, MaterialState::Purging);
    fs::remove_file(body).unwrap();
    v.cleanup(3000).unwrap();
    assert!(v.cleanup_complete(&scope(), "s"));
    assert!(v.list(&scope(), None).is_empty());
}
#[test]
fn corrupt_index_is_not_recreated() {
    let (root, vault) = fixture();
    save(&mut vault.lock().unwrap(), "s", "a");
    fs::write(root.path().join("file-recovery/index.json"), b"broken").unwrap();
    assert!(vault.lock().is_err());
}
#[test]
fn separate_worker_handles_are_exclusively_locked() {
    let (root, vault) = fixture();
    let v = vault.lock().unwrap();
    let other = open_private(&root.path().join("file-recovery/lock"), true).unwrap();
    assert!(other.try_lock().is_err());
    drop(v);
    other.try_lock().unwrap();
}

#[test]
fn lock_child() {
    let Ok(path) = std::env::var("RECOVERY_TEST_LOCK_PATH") else {
        return;
    };
    assert!(
        open_private(Path::new(&path), true)
            .unwrap()
            .try_lock()
            .is_err()
    );
}
#[test]
fn worker_process_cannot_enter_another_process_transaction() {
    let (root, vault) = fixture();
    let _locked = vault.lock().unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "tests::lock_child"])
        .env(
            "RECOVERY_TEST_LOCK_PATH",
            root.path().join("file-recovery/lock"),
        )
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn backup_io_failure_is_persisted_as_aborted_before_commit() {
    let (root, vault) = fixture();
    let mut v = vault.lock().unwrap();
    let id = hash(&serde_json::to_vec(&(scope(), "s", "a")).unwrap());
    fs::create_dir(root.path().join("file-recovery").join(format!("{id}.body"))).unwrap();
    assert!(
        v.backup(BackupRequest {
            scope: scope(),
            conversation: "s",
            operation: "a",
            generation: "g",
            file_name: "a.txt",
            content: b"old",
            metadata: b"{}",
            now_ms: 1000
        })
        .is_err()
    );
    drop(v);
    let rows = vault.lock().unwrap().list(&scope(), Some("s"));
    assert_eq!(rows[0].change, ChangeState::Aborted);
    assert_ne!(rows[0].material, MaterialState::Saved);
}

#[test]
fn corrupted_identity_in_valid_json_is_rejected() {
    let (root, vault) = fixture();
    let r = save(&mut vault.lock().unwrap(), "s", "a");
    let path = root.path().join("file-recovery/index.json");
    let mut ledger: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    ledger["records"][&r.id]["id"] = "../user-file".into();
    fs::write(path, serde_json::to_vec(&ledger).unwrap()).unwrap();
    assert!(vault.lock().is_err());
}

#[test]
fn restart_reclaims_precommit_reservations_without_replaying_unknown_commits() {
    let (_root, vault) = fixture();
    let mut v = vault.lock().unwrap();
    let prepared = save(&mut v, "s", "prepared");
    let unknown = save(&mut v, "s", "intent");
    v.transition(&scope(), &unknown.id, ChangeState::CommitIntent)
        .unwrap();
    drop(v);
    let mut v = vault.lock().unwrap();
    let unresolved = v.recover_interrupted(2000).unwrap();
    assert_eq!(unresolved.len(), 1);
    assert_eq!(unresolved[0].id, unknown.id);
    assert_eq!(unresolved[0].change, ChangeState::OutcomeUnknown);
    assert!(v.export(&scope(), &prepared.id, 2001).is_err());
    assert_eq!(
        v.list(&scope(), Some("s"))
            .iter()
            .find(|r| r.id == prepared.id)
            .unwrap()
            .material,
        MaterialState::Purged
    );
    assert!(v.export(&scope(), &unknown.id, 2001).is_ok());
    assert!(
        v.transition(&scope(), &unknown.id, ChangeState::Succeeded)
            .is_err()
    );
    assert_eq!(
        v.recover_interrupted(unknown.expires_at_ms).unwrap().len(),
        1
    );
    let retained = v
        .list(&scope(), Some("s"))
        .into_iter()
        .find(|r| r.id == unknown.id)
        .unwrap();
    assert_eq!(retained.change, ChangeState::OutcomeUnknown);
    assert_eq!(retained.material, MaterialState::Saved);
    assert!(
        v.export(&scope(), &unknown.id, unknown.expires_at_ms)
            .is_ok()
    );
    assert!(
        v.backup(BackupRequest {
            scope: scope(),
            conversation: "s",
            operation: "intent",
            generation: "new-generation",
            file_name: "note.txt",
            content: b"new",
            metadata: b"{}",
            now_ms: unknown.expires_at_ms + 1
        })
        .is_err()
    );
}

#[test]
fn purged_records_and_deleted_conversation_tombstones_still_count_toward_capacity() {
    let root = tempfile::tempdir().unwrap();
    let vault = Vault::open(root.path()).unwrap();
    let mut locked = vault.lock().unwrap();
    let empty = locked.used_bytes();
    let record = save(&mut locked, "conversation", "operation");
    locked
        .transition(&scope(), &record.id, ChangeState::Aborted)
        .unwrap();
    locked.cleanup(u64::MAX).unwrap();
    let indexed = locked.used_bytes();
    assert!(indexed > empty);
    assert!(indexed < record.bytes);
    locked
        .delete_conversation(&scope(), "conversation", u64::MAX)
        .unwrap();
    assert!(locked.used_bytes() > empty);
    assert!(locked.used_bytes() < indexed);
    let bytes = std::fs::read(root.path().join("file-recovery/index.json")).unwrap();
    assert_eq!(locked.used_bytes(), bytes.len() as u64);
}

#[test]
fn quota_denial_keeps_a_durable_cleanup_record_without_writing_backup_material() {
    let root = tempfile::tempdir().unwrap();
    let vault = Vault::open(root.path()).unwrap();
    let mut locked = vault.lock().unwrap();
    let result = locked.backup_with_reservation(
        BackupRequest {
            scope: scope(),
            conversation: "conversation",
            operation: "operation",
            generation: "generation",
            file_name: "notes.txt",
            content: b"before",
            metadata: b"{}",
            now_ms: 1,
        },
        |record| {
            let index: Ledger = serde_json::from_slice(&std::fs::read(
                root.path().join("file-recovery/index.json"),
            )?)
            .unwrap();
            assert!(index.records[&record.id].device_quota_pending);
            assert!(
                !root
                    .path()
                    .join(format!("file-recovery/{}.body", record.id))
                    .exists()
            );
            Err(io::Error::new(io::ErrorKind::StorageFull, "quota denied"))
        },
    );
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::StorageFull);
    locked
        .delete_conversation(&scope(), "conversation", 2)
        .unwrap();
    assert!(!locked.cleanup_complete(&scope(), "conversation"));
    let pending = locked.pending_quota_cleanup();
    assert_eq!(pending.len(), 1);
    let record = &pending[0];
    assert_eq!(record.material, MaterialState::Purged);
    drop(locked);
    let mut reopened = vault.lock().unwrap();
    assert_eq!(reopened.pending_quota_cleanup().len(), 1);
    reopened
        .acknowledge_quota_cleanup(&scope(), &record.id)
        .unwrap();
    assert!(reopened.pending_quota_cleanup().is_empty());
    assert!(reopened.is_deleted(&scope(), "conversation"));
    assert!(!reopened.cleanup_complete(&scope(), "conversation"));
    let namespaces = reopened.pending_quota_namespaces();
    assert_eq!(namespaces, vec![scope().key("conversation")]);
    reopened
        .acknowledge_quota_namespace(&namespaces[0])
        .unwrap();
    assert!(reopened.cleanup_complete(&scope(), "conversation"));
}

#[test]
fn lost_quota_reply_is_reconciled_after_local_cleanup_without_replaying_the_write() {
    use crate::quota::{DeviceQuota, QuotaKey};
    let root = tempfile::tempdir().unwrap();
    let device = DeviceQuota::open(root.path()).unwrap();
    let vault = Vault::open(root.path()).unwrap();
    let mut locked = vault.lock().unwrap();
    let key = QuotaKey::new(&scope(), "conversation", "operation", "generation").unwrap();
    let result = locked.backup_with_reservation(
        BackupRequest {
            scope: scope(),
            conversation: "conversation",
            operation: "operation",
            generation: "generation",
            file_name: "notes.txt",
            content: b"before",
            metadata: b"{}",
            now_ms: 1,
        },
        |record| {
            device.lock()?.reserve(key.clone(), record.bytes, 100, 1)?;
            Err(io::Error::new(io::ErrorKind::TimedOut, "reply lost"))
        },
    );
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);
    let reserved = device.lock().unwrap().used_bytes();
    assert!(reserved > 32 * 1024);
    drop(locked);
    let mut restarted = vault.lock().unwrap();
    restarted.recover_interrupted(200).unwrap();
    let pending = restarted.pending_quota_cleanup();
    assert_eq!(pending.len(), 1);
    assert_eq!(device.lock().unwrap().used_bytes(), reserved);
    device.lock().unwrap().release(&key).unwrap();
    restarted
        .acknowledge_quota_cleanup(&scope(), &pending[0].id)
        .unwrap();
    assert!(device.lock().unwrap().used_bytes() < reserved);
    assert!(
        restarted
            .backup(BackupRequest {
                scope: scope(),
                conversation: "conversation",
                operation: "operation",
                generation: "generation",
                file_name: "notes.txt",
                content: b"before",
                metadata: b"{}",
                now_ms: 201,
            })
            .is_err()
    );
}

#[test]
fn epoch_driver_recovers_lost_acknowledgments_without_double_advancement() {
    struct LostReply<'a> {
        quota: &'a mut quota::LockedDeviceQuota,
        lose_begin: bool,
        lose_finish: bool,
    }
    impl EpochCoordinator for LostReply<'_> {
        fn begin(&mut self, user: &str, epoch: u64) -> io::Result<quota::EpochStatus> {
            let state = self.quota.begin_epoch_cleanup(user, epoch)?;
            if self.lose_begin {
                return Err(io::ErrorKind::TimedOut.into());
            }
            Ok(state)
        }
        fn finish(&mut self, user: &str, epoch: u64) -> io::Result<quota::EpochStatus> {
            let state = self.quota.finish_epoch_cleanup(user, epoch)?;
            if self.lose_finish {
                return Err(io::ErrorKind::TimedOut.into());
            }
            Ok(state)
        }
    }
    let (root, vault) = fixture();
    let device = quota::DeviceQuota::open(root.path()).unwrap();
    let mut v = vault.lock().unwrap();
    let mut q = device.lock().unwrap();
    let r = save(&mut v, "conversation", "operation");
    v.transition(&scope(), &r.id, ChangeState::Aborted).unwrap();
    v.discard(&scope(), "conversation", &r.id, 1001).unwrap();
    v.maintain_epoch_indexes(&scope().os_user, &mut q, 64)
        .unwrap();
    assert_eq!(v.execution_epoch(), 0);
    assert!(v.epoch_cleanup_state().is_none());
    let mut lost = LostReply {
        quota: &mut q,
        lose_begin: true,
        lose_finish: false,
    };
    assert_eq!(
        v.maintain_epoch_indexes(&scope().os_user, &mut lost, 1)
            .unwrap_err()
            .kind(),
        io::ErrorKind::TimedOut
    );
    assert!(!v.epoch_cleanup_state().unwrap().pruned);
    drop(v);
    let mut v = vault.lock().unwrap();
    lost.lose_begin = false;
    lost.lose_finish = true;
    assert_eq!(
        v.maintain_epoch_indexes(&scope().os_user, &mut lost, usize::MAX)
            .unwrap_err()
            .kind(),
        io::ErrorKind::TimedOut
    );
    assert!(v.epoch_cleanup_state().unwrap().pruned);
    assert!(v.require_execution_epoch(1).is_err());
    drop(v);
    let mut v = vault.lock().unwrap();
    lost.lose_finish = false;
    v.maintain_epoch_indexes(&scope().os_user, &mut lost, usize::MAX)
        .unwrap();
    v.require_execution_epoch(1).unwrap();
    assert!(v.list(&scope(), None).is_empty());
    assert_eq!(q.epoch(&scope().os_user).unwrap().epoch, 1);
}

#[test]
fn export_distinguishes_material_states_without_disclosing_other_scopes() {
    let (_root, vault) = fixture();
    let mut v = vault.lock().unwrap();
    let r = save(&mut v, "s", "a");
    for (state, expected) in [
        (MaterialState::Preparing, ExportError::Preparing),
        (MaterialState::Purging, ExportError::Cleaning),
        (MaterialState::Purged, ExportError::Cleaned),
    ] {
        v.ledger.records.get_mut(&r.id).unwrap().material = state;
        let error = v.export_package(&scope(), &r.id, 1001).unwrap_err();
        assert_eq!(
            error.get_ref().unwrap().downcast_ref::<ExportError>(),
            Some(&expected)
        );
        let mut other = scope();
        other.owner = "another-owner".into();
        let error = v.export_package(&other, &r.id, 1001).unwrap_err();
        assert_eq!(
            error.get_ref().unwrap().downcast_ref::<ExportError>(),
            Some(&ExportError::Unavailable)
        );
    }
    v.ledger.records.get_mut(&r.id).unwrap().material = MaterialState::Saved;
    let error = v
        .export_package(&scope(), &r.id, r.expires_at_ms)
        .unwrap_err();
    assert_eq!(
        error.get_ref().unwrap().downcast_ref::<ExportError>(),
        Some(&ExportError::Expired)
    );
    v.ledger.records.get_mut(&r.id).unwrap().change = ChangeState::OutcomeUnknown;
    assert!(v.export_package(&scope(), &r.id, r.expires_at_ms).is_ok());
}

#[test]
fn settlement_ack_write_failure_keeps_retry_in_memory_and_after_reopen() {
    let (root, vault) = fixture();
    let mut locked = vault.lock().unwrap();
    let record = locked
        .backup_with_reservation(
            BackupRequest {
                scope: scope(),
                conversation: "c",
                operation: "op",
                generation: "g",
                file_name: "notes.txt",
                content: b"before",
                metadata: b"{}",
                now_ms: 1000,
            },
            |_| Ok(()),
        )
        .unwrap();
    let index = root.path().join("file-recovery/index.json");
    let retained = root.path().join("held-index.json");
    std::fs::rename(&index, &retained).unwrap();
    std::fs::create_dir(&index).unwrap();
    assert!(
        locked
            .acknowledge_quota_settlement(&scope(), &record.id)
            .is_err()
    );
    assert_eq!(locked.pending_quota_settlement().len(), 1);
    std::fs::remove_dir(&index).unwrap();
    std::fs::rename(&retained, &index).unwrap();
    drop(locked);
    let mut locked = vault.lock().unwrap();
    assert_eq!(locked.pending_quota_settlement().len(), 1);
    locked
        .acknowledge_quota_settlement(&scope(), &record.id)
        .unwrap();
    assert!(locked.pending_quota_settlement().is_empty());
    assert_eq!(
        locked.export(&scope(), &record.id, 1001).unwrap().1,
        b"before"
    );
}
