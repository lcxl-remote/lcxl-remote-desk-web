//! Real ENOSPC on a small, disposable tmpfs; invoked only by the isolated PoC.
use super::*;
use std::io::Write;

#[test]
#[ignore = "requires isolated bounded tmpfs; use pocs/poc-linux-file-storage/run.py"]
fn full_backup_filesystem_preserves_target() {
    assert_eq!(std::env::var("LCXL_ISOLATED_FULL_DISK").as_deref(), Ok("1"));
    let backup = Path::new("/backup");
    let mut stat = std::mem::MaybeUninit::<libc::statfs>::uninit();
    assert_eq!(
        unsafe { libc::statfs(c"/backup".as_ptr(), stat.as_mut_ptr()) },
        0
    );
    let stat = unsafe { stat.assume_init() };
    assert_eq!(stat.f_type, libc::TMPFS_MAGIC);
    assert!(stat.f_blocks * stat.f_bsize as u64 <= 2 * 1024 * 1024);
    let original = vec![b'a'; MAX_TEXT_READ_BYTES as usize];
    let _guard = file_store_test_lock();
    for change in ["update", "delete"] {
        let selected = Path::new("/fixture").join(change);
        std::fs::create_dir(&selected).unwrap();
        let path = selected.join("notes.txt");
        std::fs::write(&path, &original).unwrap();
        let before = std::fs::metadata(&path).unwrap();
        let directory = issue(&selected).unwrap();
        let target = issue(&path).unwrap();
        let parent = open_verified(&selected).unwrap();
        let source =
            desk_file_recovery::linux::open_text_beneath(&parent.handle, Path::new("notes.txt"))
                .expect("container must permit production openat2 source access");
        assert_eq!(
            unix_file_identity(&source).unwrap(),
            resolve(&target).unwrap().identity
        );
        assert_eq!(complete_text(&source).unwrap(), original);
        let mut context = test_context(&directory);
        context.data_root = backup.join(change);
        std::fs::create_dir(&context.data_root).unwrap();
        drop(desk_file_recovery::Vault::open(&context.data_root).unwrap());
        let filler_path = backup.join("filler");
        let mut filler = std::fs::File::create(&filler_path).unwrap();
        let block = [0_u8; 4096];
        let error = loop {
            match filler.write_all(&block) {
                Ok(()) => assert!(filler.metadata().unwrap().len() <= 2 * 1024 * 1024),
                Err(error) => break error,
            }
        };
        assert_eq!(error.raw_os_error(), Some(libc::ENOSPC));
        // Leave enough room for the intent/error ledger, but not the original body.
        filler
            .set_len(filler.metadata().unwrap().len() - 32 * 1024)
            .unwrap();
        let mut submissions = 0;
        let result = mutate_text_with_submit(
            TextMutationRequest {
                directory: &directory,
                target: &target,
                expected_sha256: &digest(&original),
                change: if change == "delete" {
                    TextChange::Trash
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
                panic!("full backup filesystem must not reach native submission")
            },
        );
        let error = result.err().expect("backup must fail");
        assert!(
            format!("{error:?}").contains("save private file backup"),
            "{error:?}"
        );
        assert!(error.message.contains("os error 28"), "{error:?}");
        assert_eq!(submissions, 0);
        assert_eq!(std::fs::read(&path).unwrap(), original);
        let after = std::fs::metadata(&path).unwrap();
        assert_eq!(
            (before.dev(), before.ino(), before.mode()),
            (after.dev(), after.ino(), after.mode())
        );
        assert_eq!(std::fs::read_dir(&selected).unwrap().count(), 1);
        let vault = desk_file_recovery::Vault::open(&context.data_root).unwrap();
        let locked = vault.lock().unwrap();
        let records = locked.list(&context.scope, Some(&context.conversation_id));
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].change, desk_file_recovery::ChangeState::Aborted);
        assert_eq!(
            records[0].material,
            desk_file_recovery::MaterialState::Preparing
        );
        assert!(records[0].transaction.is_none());
        assert!(
            !context
                .data_root
                .join("file-recovery")
                .join(format!("{}.body", records[0].id))
                .exists()
        );
        drop(locked);
        drop(vault);
        drop(filler);
        std::fs::remove_file(filler_path).unwrap();
        std::fs::remove_dir_all(&context.data_root).unwrap();
    }
}

#[test]
#[ignore = "requires isolated ext4 image; use pocs/poc-linux-file-storage/run.py --target-ext4"]
fn full_target_filesystem_preserves_target() {
    assert_eq!(std::env::var("LCXL_ISOLATED_EXT4").as_deref(), Ok("1"));
    assert_ne!(unsafe { libc::geteuid() }, 0);
    let mut stat = std::mem::MaybeUninit::<libc::statfs>::uninit();
    assert_eq!(
        unsafe { libc::statfs(c"/fixture".as_ptr(), stat.as_mut_ptr()) },
        0
    );
    let stat = unsafe { stat.assume_init() };
    assert_eq!(stat.f_type, libc::EXT4_SUPER_MAGIC);
    assert!(stat.f_blocks * stat.f_bsize as u64 <= 64 * 1024 * 1024);
    let _guard = file_store_test_lock();
    for change in ["update", "delete", "stage"] {
        let selected = Path::new("/fixture").join(change);
        std::fs::create_dir(&selected).unwrap();
        let path = selected.join("notes.txt");
        std::fs::write(&path, b"before").unwrap();
        let before = std::fs::metadata(&path).unwrap();
        let directory = issue(&selected).unwrap();
        let target = issue(&path).unwrap();
        let mut context = test_context(&directory);
        context.data_root = Path::new("/backup").join(change);
        std::fs::create_dir(&context.data_root).unwrap();
        File::open(&path).unwrap().sync_all().unwrap();
        let filler_root = Path::new("/fixture/filler");
        std::fs::create_dir(filler_root).unwrap();
        if change == "stage" {
            let mut filler = File::create(filler_root.join("blocks")).unwrap();
            let block = [0_u8; 4096];
            loop {
                // Sync each block so delayed allocation cannot hide ENOSPC.
                match filler.write_all(&block).and_then(|_| filler.sync_all()) {
                    Ok(()) => assert!(filler.metadata().unwrap().len() <= 64 * 1024 * 1024),
                    Err(error) => {
                        assert_eq!(error.raw_os_error(), Some(libc::ENOSPC));
                        break;
                    }
                }
            }
            filler
                .set_len(filler.metadata().unwrap().len() - 8192)
                .unwrap();
            filler.sync_all().unwrap();
        } else {
            let mut index = 0;
            loop {
                match File::create(filler_root.join(format!("inode-{index}"))) {
                    Ok(_) => {
                        index += 1;
                        assert!(index <= 2048);
                    }
                    Err(error) => {
                        assert_eq!(error.raw_os_error(), Some(libc::ENOSPC));
                        break;
                    }
                }
            }
            let mut full = std::mem::MaybeUninit::<libc::statfs>::uninit();
            assert_eq!(
                unsafe { libc::statfs(c"/fixture".as_ptr(), full.as_mut_ptr()) },
                0
            );
            assert_eq!(
                unsafe { full.assume_init() }.f_ffree,
                0,
                "directory failure must be inode exhaustion"
            );
        }
        let replacement = "b".repeat(MAX_TEXT_READ_BYTES as usize);
        let mut submissions = 0;
        let result = mutate_text_with_submit(
            TextMutationRequest {
                directory: &directory,
                target: &target,
                expected_sha256: &digest(b"before"),
                change: if change == "delete" {
                    TextChange::Trash
                } else {
                    TextChange::ReplaceAll(&replacement)
                },
            },
            || {},
            || {},
            || Ok(()),
            &context,
            |_, _, _, _, _| {
                submissions += 1;
                panic!("{change}: target ENOSPC must not reach native submission")
            },
        );
        let error = result.err().expect("target preparation must fail");
        assert!(error.message.contains("os error 28"), "{change}: {error:?}");
        assert!(
            error.message.contains(if change == "stage" {
                "write text recovery file"
            } else {
                "create file transaction directory"
            }),
            "{change}: {error:?}"
        );
        assert_eq!(submissions, 0);
        assert_eq!(std::fs::read(&path).unwrap(), b"before");
        let after = std::fs::metadata(&path).unwrap();
        assert_eq!(
            (before.dev(), before.ino(), before.mode()),
            (after.dev(), after.ino(), after.mode())
        );
        assert_eq!(std::fs::read_dir(&selected).unwrap().count(), 1);
        let vault = desk_file_recovery::Vault::open(&context.data_root).unwrap();
        let locked = vault.lock().unwrap();
        let records = locked.list(&context.scope, Some(&context.conversation_id));
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].change, desk_file_recovery::ChangeState::Aborted);
        assert_eq!(
            records[0].material,
            desk_file_recovery::MaterialState::Saved
        );
        assert!(records[0].transaction.is_none());
        assert_eq!(
            std::fs::read(
                context
                    .data_root
                    .join("file-recovery")
                    .join(format!("{}.body", records[0].id))
            )
            .unwrap(),
            b"before"
        );
        drop((locked, vault));
        std::fs::remove_dir_all(filler_root).unwrap();
        std::fs::remove_dir_all(&context.data_root).unwrap();
    }
}

#[test]
#[ignore = "requires isolated ext4 image; use pocs/poc-linux-file-storage/run.py --target-ext4 --commit-full"]
fn full_target_after_staging_verifies_real_commit() {
    assert_eq!(std::env::var("LCXL_ISOLATED_EXT4").as_deref(), Ok("1"));
    assert_ne!(unsafe { libc::geteuid() }, 0);
    let mut stat = std::mem::MaybeUninit::<libc::statfs>::uninit();
    assert_eq!(
        unsafe { libc::statfs(c"/fixture".as_ptr(), stat.as_mut_ptr()) },
        0
    );
    let stat = unsafe { stat.assume_init() };
    assert_eq!(stat.f_type, libc::EXT4_SUPER_MAGIC);
    assert!(stat.f_blocks * stat.f_bsize as u64 <= 64 * 1024 * 1024);
    let _guard = file_store_test_lock();
    for deletion in [false, true] {
        let name = if deletion { "delete" } else { "update" };
        let selected = Path::new("/fixture").join(name);
        std::fs::create_dir(&selected).unwrap();
        let path = selected.join("notes.txt");
        std::fs::write(&path, b"before").unwrap();
        File::open(&path).unwrap().sync_all().unwrap();
        let directory = issue(&selected).unwrap();
        let target = issue(&path).unwrap();
        let mut context = test_context(&directory);
        context.data_root = Path::new("/backup").join(name);
        std::fs::create_dir(&context.data_root).unwrap();
        let guards = std::cell::Cell::new(0);
        let receipt = mutate_text_managed(
            TextMutationRequest {
                directory: &directory,
                target: &target,
                expected_sha256: &digest(b"before"),
                change: if deletion {
                    TextChange::Trash
                } else {
                    TextChange::ReplaceAll("after")
                },
            },
            || {
                // Staging is already durable. Do not replace the native submit seam:
                // a full data allocator does not imply atomic rename must fail.
                let mut filler = File::create("/fixture/filler").unwrap();
                loop {
                    match filler.write_all(&[0; 4096]).and_then(|_| filler.sync_all()) {
                        Ok(()) => assert!(filler.metadata().unwrap().len() <= 64 * 1024 * 1024),
                        Err(error) => {
                            assert_eq!(error.raw_os_error(), Some(libc::ENOSPC));
                            break;
                        }
                    }
                }
            },
            || {},
            || {
                guards.set(guards.get() + 1);
                Ok(())
            },
            &context,
        )
        .unwrap();
        assert_eq!(guards.get(), 1);
        assert!(receipt.verified, "{name}: {}", receipt.message);
        assert!(!receipt.recovery.cleanup_pending);
        if deletion {
            assert!(!path.exists());
            assert!(receipt.file.is_none());
        } else {
            assert_eq!(std::fs::read(&path).unwrap(), b"after");
            assert_eq!(receipt.file.as_ref().unwrap().sha256, digest(b"after"));
        }
        assert_eq!(
            std::fs::read_dir(&selected).unwrap().count(),
            if deletion { 0 } else { 1 }
        );
        let vault = desk_file_recovery::Vault::open(&context.data_root).unwrap();
        let locked = vault.lock().unwrap();
        let records = locked.list(&context.scope, Some(&context.conversation_id));
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].change,
            desk_file_recovery::ChangeState::Succeeded
        );
        assert_eq!(
            records[0].material,
            desk_file_recovery::MaterialState::Saved
        );
        assert!(records[0].transaction.is_none());
        assert_eq!(std::fs::read(&receipt.backup_path).unwrap(), b"before");
        drop((locked, vault));
        std::fs::remove_file("/fixture/filler").unwrap();
        std::fs::remove_dir_all(&context.data_root).unwrap();
    }
}
