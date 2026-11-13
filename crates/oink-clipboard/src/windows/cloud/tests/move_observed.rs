//! Regression coverage for optimized MOVE racing the target's first hydration.
use super::*;
use std::os::windows::fs::OpenOptionsExt;

fn offer(service: &Arc<Service>, delivery: &Arc<Delivery>) -> Offer {
    Offer {
        created: Instant::now(),
        service: service.clone(),
        entries: delivery.entries(),
        contents: delivery.contents(),
        prepared: Mutex::new(Some(delivery.clone())),
        cancelled: AtomicBool::new(false),
        virtual_files: true,
    }
}

#[test]
fn observed_move_allows_reader_and_fetch_during_identity_query() {
    static GATE: Mutex<Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>> = Mutex::new(None);
    unsafe extern "system" fn gated_info(
        handle: HANDLE,
        kind: i32,
        buffer: *mut c_void,
        size: u32,
        written: *mut u32,
    ) -> i32 {
        let gate = GATE.lock().unwrap().take();
        if let Some((entered, resume)) = gate {
            let _ = entered.send(());
            // Bound fixture failures without panicking across the native ABI.
            let _ = resume.recv_timeout(Duration::from_secs(10));
        }
        // SAFETY: forwarding the caller's live metadata buffers.
        unsafe { owned_info(handle, kind, buffer, size, written) }
    }
    let _serial = SERIAL.lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let (service, receiver) = fake_with_info(temp.path().join("CloudFiles"), gated_info);
    let source = Bytes::new(3);
    let delivery = service
        .publish(vec![entry("file", 3, false)], source.clone(), Instant::now())
        .unwrap();
    let (entered, entered_rx) = mpsc::channel();
    let (resume, resume_rx) = mpsc::channel();
    *GATE.lock().unwrap() = Some((entered, resume_rx));
    let offer = offer(&service, &delivery);
    let cleanup = std::thread::spawn(move || offer.optimized_move_completed());
    entered_rx.recv_timeout(Duration::from_secs(10)).unwrap();

    let pin = native::ReaderPin::new(&delivery);
    let barrier = service.received.lock().unwrap().cleanup_inflight.clone();
    // A read-only handle sharing only READ excludes any outstanding WRITE or
    // DELETE handle. Observed must require neither access right nor exclusivity.
    let shared = std::fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ)
        .open(delivery.file_path(0));
    let mut identity = [0u8; 16];
    identity[..8].copy_from_slice(&delivery.id.to_le_bytes());
    HYDRATION_TARGETS
        .lock()
        .unwrap()
        .push((0, delivery.file_path(0), identity, gated_info));
    let info = CF_CALLBACK_INFO {
        CallbackContext: (&*service.context as *const Weak<Service>).cast_mut().cast(),
        FileIdentity: identity.as_ptr().cast(),
        FileIdentityLength: 16,
        FileSize: 3,
        TransferKey: 1,
        ..Default::default()
    };
    let params = CF_CALLBACK_PARAMETERS {
        Anonymous: CF_CALLBACK_PARAMETERS_0 {
            FetchData: CF_CALLBACK_PARAMETERS_0_1 {
                RequiredLength: 3,
                ..Default::default()
            },
        },
        ..Default::default()
    };
    // SAFETY: callback context, identity and records remain live during fetch.
    unsafe { fetch(&info, &params) };
    let work = receiver.try_recv();
    let admitted = work.is_ok();
    if let Ok(work) = work {
        work.run();
    }
    let transfers = TRANSFERS.lock().unwrap().clone();
    resume.send(()).unwrap();
    cleanup.join().unwrap().unwrap();
    assert!(pin.is_ok(), "Observed rejected a concurrent reader");
    assert!(barrier.is_empty(), "Observed installed a deletion admission barrier");
    assert!(shared.is_ok(), "Observed requested exclusive/write/delete access");
    assert!(admitted, "Observed canceled FETCH before it reached a worker");
    assert_eq!(transfers, [(0, b"abc".to_vec(), STATUS_SUCCESS)]);
    drop((pin, shared));
    assert_eq!(source.opens.load(Ordering::Relaxed), 1);
    assert!(
        TRANSFERS
            .lock()
            .unwrap()
            .iter()
            .all(|(_, _, status)| *status == STATUS_SUCCESS)
    );
}

#[test]
fn observed_move_mixed_batch_retires_only_absent_or_replaced_identities() {
    let _serial = SERIAL.lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let (service, _) = fake_with_info(temp.path().join("CloudFiles"), owned_info);
    let delivery = service
        .publish(
            vec![
                entry("retained", 3, false),
                entry("moved", 3, false),
                entry("replaced", 3, false),
            ],
            Bytes::new(3),
            Instant::now(),
        )
        .unwrap();
    let target = temp.path().join("target");
    std::fs::rename(delivery.file_path(1), &target).unwrap();
    // Give the old third path a different delivery's valid placeholder identity.
    let path = delivery.file_path(2).canonicalize().unwrap();
    IDENTITIES
        .lock()
        .unwrap()
        .iter_mut()
        .find(|(entry, _)| *entry == path)
        .unwrap()
        .1[..8]
        .copy_from_slice(&999u64.to_le_bytes());
    std::fs::write(&path, b"new").unwrap();
    offer(&service, &delivery).optimized_move_completed().unwrap();
    let journal = service.received.lock().unwrap();
    assert!(!journal.deliveries[&delivery.id][0].deleted);
    assert!(journal.deliveries[&delivery.id][1].deleted);
    assert!(journal.deliveries[&delivery.id][2].deleted);
    assert!(matches!(journal.moves[&delivery.id], moved::State::Retained));
    assert_eq!(std::fs::read(path).unwrap(), b"new");
    assert!(target.exists());
}

#[test]
fn observed_move_query_error_retries_without_closing_reader_admission() {
    static FAIL: AtomicBool = AtomicBool::new(false);
    unsafe extern "system" fn uncertain_info(
        handle: HANDLE,
        kind: i32,
        buffer: *mut c_void,
        size: u32,
        written: *mut u32,
    ) -> i32 {
        if FAIL.load(Ordering::Relaxed) {
            return 0x80070020u32 as i32;
        }
        // SAFETY: forwarding the caller's live metadata buffers.
        unsafe { owned_info(handle, kind, buffer, size, written) }
    }
    let _serial = SERIAL.lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let (service, _) = fake_with_info(temp.path().join("CloudFiles"), uncertain_info);
    let delivery = service
        .publish(vec![entry("file", 3, false)], Bytes::new(3), Instant::now())
        .unwrap();
    FAIL.store(true, Ordering::Relaxed);
    offer(&service, &delivery).optimized_move_completed().unwrap();
    service.cleanup_moved();
    assert!(matches!(
        service.received.lock().unwrap().moves[&delivery.id],
        moved::State::Observed
    ));
    assert!(!delivery.file(0).deleted.load(Ordering::Acquire));
    assert!(native::ReaderPin::new(&delivery).is_ok());
    FAIL.store(false, Ordering::Relaxed);
    service.cleanup_moved();
    assert!(matches!(
        service.received.lock().unwrap().moves[&delivery.id],
        moved::State::Retained
    ));
}

#[test]
fn move_journal_reads_existing_states_and_retained_without_migration() {
    let _serial = SERIAL.lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let (service, _) = fake_with_info(temp.path().join("CloudFiles"), owned_info);
    let delivery = service
        .publish(vec![entry("file", 3, false)], Bytes::new(3), Instant::now())
        .unwrap();
    let mut saved = serde_json::to_value(&*service.received.lock().unwrap()).unwrap();
    for state in ["pending", "observed", "removed", "retained"] {
        saved["moves"] = serde_json::json!({delivery.id.to_string(): state});
        std::fs::write(
            service.root.with_extension("received"),
            serde_json::to_vec(&saved).unwrap(),
        )
        .unwrap();
        let loaded = received::Journal::load(&service.root).unwrap();
        assert_eq!(serde_json::to_value(loaded.moves[&delivery.id]).unwrap(), state);
        assert!(!loaded.deliveries[&delivery.id][0].deleted);
    }
}

#[test]
fn observed_move_retains_copy_without_retry_after_sweep_or_reload() {
    static QUERIES: AtomicUsize = AtomicUsize::new(0);
    unsafe extern "system" fn counting_info(
        handle: HANDLE,
        kind: i32,
        buffer: *mut c_void,
        size: u32,
        written: *mut u32,
    ) -> i32 {
        QUERIES.fetch_add(1, Ordering::Relaxed);
        // SAFETY: forwarding the caller's live metadata buffers.
        unsafe { owned_info(handle, kind, buffer, size, written) }
    }
    let _serial = SERIAL.lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let (service, _) = fake_with_info(temp.path().join("CloudFiles"), counting_info);
    let delivery = one_shot_delivery(&service);
    let offer = offer(&service, &delivery);
    let before = serde_json::to_value(&service.received.lock().unwrap().deliveries).unwrap();
    let source_before = serde_json::to_value(&service.received.lock().unwrap().sources).unwrap();
    offer.optimized_move_completed().unwrap();
    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(service.root.with_extension("received")).unwrap()).unwrap();
    assert_eq!(saved["moves"][delivery.id.to_string()], "retained");
    assert_eq!(
        serde_json::to_value(&service.received.lock().unwrap().deliveries).unwrap(),
        before
    );
    assert_eq!(
        serde_json::to_value(&service.received.lock().unwrap().sources).unwrap(),
        source_before,
        "MOVE observation must not complete or release a one-shot source"
    );
    assert!(!delivery.file(0).deleted.load(Ordering::Acquire));
    let queries = QUERIES.load(Ordering::Relaxed);
    for _ in 0..3 {
        service.cleanup_moved();
    }
    *service.received.lock().unwrap() = received::Journal::load(&service.root).unwrap();
    service.cleanup_moved();
    offer.optimized_move_completed().unwrap();
    assert_eq!(
        QUERIES.load(Ordering::Relaxed),
        queries,
        "terminal MOVE retried identity checks"
    );
    // Stronger, unoptimized MOVE evidence can still authorize deletion later.
    offer.move_completed().unwrap();
    service.cleanup_moved();
    assert!(!delivery.file_path(0).exists());
    assert!(matches!(
        service.received.lock().unwrap().moves[&delivery.id],
        moved::State::Removed
    ));
}

#[test]
fn phone_move_requires_source_absence_and_follows_the_original_frn() {
    use crate::content::namespace::{Files, Local, Namespace};
    let _serial = SERIAL.lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let cache = temp.path().join("phone.bin");
    std::fs::write(&cache, b"phone bytes").unwrap();
    let namespace = Local::received_from(
        vec![(crate::model::NodeId(1), cache.clone())],
        oink_foundation::DeviceId([37; 32]),
    )
    .unwrap();
    let version = namespace.roots()[0].version;
    let files = Files::new(namespace.clone(), crate::sync::files::TargetOs::Windows).unwrap();
    let (service, _) = fake(temp.path().join("CloudFiles"));
    let delivery = service.publish(files.roots().to_vec(), files, Instant::now()).unwrap();
    let old = delivery.file_path(0);
    // The fake native publisher owns an ordinary file. Give it the delivered
    // version; the move-proof API itself uses real Windows file identities.
    std::fs::copy(&cache, &old).unwrap();
    std::fs::File::options()
        .write(true)
        .open(&old)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(std::fs::metadata(&cache).unwrap().modified().unwrap()))
        .unwrap();
    use std::os::windows::io::AsRawHandle;
    let handle = std::fs::File::open(&old).unwrap();
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: live fixture-owned handle and writable information record.
    let inspected = unsafe { GetFileInformationByHandle(handle.as_raw_handle(), &mut info) };
    assert_ne!(inspected, 0);
    drop(handle);
    let volume = info.dwVolumeSerialNumber;
    let index = ((u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow)) as i64;
    assert!(
        !service.track_proxy_move(&old, volume, index).unwrap(),
        "a copy or unrelated directory event cannot authorize deletion"
    );
    assert!(cache.exists());
    let target = temp.path().join("first-destination");
    service
        .received
        .lock()
        .unwrap()
        .renames
        .insert((volume, index), (old.clone(), target.clone()));
    service.watch_proxy_move(&old, &target, (volume, index)).unwrap();
    // A canceled watcher from an identical retry cannot retire its successor.
    service.watch_proxy_move(&old, &target, (volume, index)).unwrap();
    std::fs::rename(&old, &target).unwrap();
    // The user moves it again and leaves an unrelated replacement at the first
    // destination before the provider handles its native directory notification.
    let final_target = temp.path().join("second-destination");
    std::fs::rename(&target, &final_target).unwrap();
    std::fs::write(&target, b"unrelated replacement").unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while cache.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!cache.exists(), "only a real directory event can commit the handoff");
    assert_eq!(std::fs::read(&target).unwrap(), b"unrelated replacement");
    assert_eq!(
        crate::content::read_all(&mut namespace.open(crate::model::NodeId(1), version, 0).unwrap()).unwrap(),
        b"phone bytes"
    );
    service.stop();
}
