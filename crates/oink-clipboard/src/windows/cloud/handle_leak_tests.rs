mod handle_leak_tests {
    use super::*;

    #[test]
    fn missing_cleanup_paths_leave_the_retry_set() {
        let _serial = SERIAL.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let (service, _) = fake_with_info(temp.path().join("CloudFiles"), owned_info);
        let delivery = service
            .publish(
                vec![entry("folder", 0, true), entry("folder/file", 3, false)],
                Bytes::new(3),
                Instant::now(),
            )
            .unwrap();
        std::fs::remove_dir_all(&delivery.path).unwrap();
        service.received.lock().unwrap().cancelled.insert(delivery.id);
        service.received_sweep();
        {
            let journal = service.received.lock().unwrap();
            assert!(journal.deliveries[&delivery.id].iter().all(|node| node.deleted));
            assert!(journal.cleanup_complete.contains(&(delivery.id, usize::MAX)));
        }
        // Retiring old names must never authorize cleanup of their replacements.
        std::fs::create_dir_all(delivery.file_path(0)).unwrap();
        std::fs::write(delivery.file_path(1), b"new").unwrap();
        for _ in 0..3 {
            service.received_sweep();
        }
        assert_eq!(std::fs::read(delivery.file_path(1)).unwrap(), b"new");
    }

    #[test]
    fn missing_withdrawn_file_is_not_retried() {
        let _serial = SERIAL.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let (service, _) = fake_with_info(temp.path().join("CloudFiles"), owned_info);
        let delivery = service
            .publish(vec![entry("file", 3, false)], Bytes::new(3), Instant::now())
            .unwrap();
        let path = delivery.file_path(0);
        std::fs::remove_file(&path).unwrap();
        delivery.source_withdrawn(&service, 0);
        delivery.remove_withdrawn(&service);
        assert!(
            service
                .received
                .lock()
                .unwrap()
                .cleanup_complete
                .contains(&(delivery.id, 0))
        );
        std::fs::write(&path, b"foreign").unwrap();
        delivery.remove_withdrawn(&service);
        assert_eq!(std::fs::read(&path).unwrap(), b"foreign");
    }

    #[test]
    fn unconfirmed_tombstone_still_retries_a_busy_native_delete() {
        use std::os::windows::fs::OpenOptionsExt;
        let _serial = SERIAL.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let (service, _) = fake_with_info(temp.path().join("CloudFiles"), owned_info);
        let delivery = service
            .publish(vec![entry("file", 3, false)], Bytes::new(3), Instant::now())
            .unwrap();
        let path = delivery.file_path(0);
        let busy = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&path)
            .unwrap();
        {
            let mut journal = service.received.lock().unwrap();
            // Withdrawal persists its tombstone before attempting native unlink.
            journal.deliveries.get_mut(&delivery.id).unwrap()[0].deleted = true;
            journal.cancelled.insert(delivery.id);
        }
        service.received_sweep();
        assert!(path.exists());
        assert!(
            !service
                .received
                .lock()
                .unwrap()
                .cleanup_complete
                .contains(&(delivery.id, 0))
        );
        drop(busy);
        service.received_sweep();
        assert!(!path.exists());
        assert!(
            service
                .received
                .lock()
                .unwrap()
                .cleanup_complete
                .contains(&(delivery.id, 0))
        );
    }

    /// Private regular files and absent paths only: no installed provider,
    /// clipboard, desktop, registration or hydration activity is involved.
    #[test]
    fn native_missing_and_closed_protected_opens_do_not_leak_handles() {
        const CHILD: &str = "OINK_TEST_CF_HANDLE_LEAK_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "windows::cloud::tests::handle_leak_tests::native_missing_and_closed_protected_opens_do_not_leak_handles",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            print!("{}", String::from_utf8_lossy(&output.stdout));
            return;
        }
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessHandleCount};
        fn handles() -> u32 {
            let mut count = 0;
            // SAFETY: own pseudo handle and valid count output; no owned handle.
            let queried = unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut count) };
            assert_ne!(queried, 0);
            count
        }
        let temp = tempfile::tempdir().unwrap();
        let present = temp.path().join("present");
        let missing = temp.path().join("missing");
        std::fs::write(&present, b"private").unwrap();
        let api = Api::load().unwrap();
        let round = || {
            drop(protection::Protected::open(api.clone(), &present, CF_OPEN_FILE_FLAG_NONE).unwrap());
            let error = protection::Protected::open(api.clone(), &missing, CF_OPEN_FILE_FLAG_NONE)
                .err()
                .unwrap();
            assert_eq!(error.kind(), io::ErrorKind::NotFound);
        };
        for _ in 0..100 {
            round();
        }
        let before = handles();
        for _ in 0..1000 {
            round();
        }
        let after = handles();
        println!("CF protected opens: success=1000 missing=1000 handles_before={before} handles_after={after}");
        assert!(
            after <= before + 2,
            "repeated opens retained {} handles",
            after.saturating_sub(before)
        );
    }
}
