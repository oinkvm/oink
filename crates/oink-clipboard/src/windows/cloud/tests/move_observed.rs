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
