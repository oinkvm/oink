use super::*;

fn cleanup_finished() {
    let (send, receive) = std::sync::mpsc::channel();
    cleanup().send(Cleanup::Barrier(send)).unwrap();
    receive.recv().unwrap();
}

#[test]
fn d1_restart_retires_orphaned_promise_writers_but_keeps_live_receivers() {
    let data = tempfile::tempdir().unwrap();
    let live = Batch::create(data.path(), StoreKind::PromiseExport, BatchId([90; 16])).unwrap();
    live.external_writers_started().unwrap();
    let stale = data
        .path()
        .join(StoreKind::PromiseExport.directory())
        .join(BatchId([91; 16]).name());
    create_directory(&stale).unwrap();
    save(&stale, &load(live.path(), StoreKind::PromiseExport).unwrap()).unwrap();
    fs::write(stale.join("partial"), b"orphaned write").unwrap();
    recover(data.path());
    assert!(!stale.exists(), "a dead receiver cannot ever deliver this export");
    assert!(live.path().exists(), "current-process receivers must stay pinned");
    live.external_writers_finished().unwrap();
}

#[test]
fn ownership_commit_works_while_cache_reader_holds_previous_record() {
    let data = tempfile::tempdir().unwrap();
    let batch = Batch::create(data.path(), StoreKind::PromiseExport, BatchId([71; 16])).unwrap();
    let mut accounting = fs::File::open(batch.path().join(RECORD)).unwrap();
    batch.external_writers_started().unwrap();
    let mut old = Vec::new();
    accounting.read_to_end(&mut old).unwrap();
    assert!(!serde_json::from_slice::<Record>(&old).unwrap().external_writers);
    assert!(load(batch.path(), StoreKind::PromiseExport).unwrap().external_writers);
    batch.external_writers_finished().unwrap();
}

#[test]
fn snapshots_fail_without_publishing_truncated_bytes_and_aggregate_all_representations() {
    let data = tempfile::tempdir().unwrap();
    let batch = Batch::create(data.path(), StoreKind::SourceSnapshot, BatchId([1; 16])).unwrap();
    let n = batch.snapshot_with_limits(1, &mut &b"abcd"[..], 4, 6).unwrap();
    assert_eq!(n, 4);
    assert_eq!(fs::read(batch.path().join("r-1")).unwrap(), b"abcd");
    assert_eq!(
        batch
            .snapshot_with_limits(2, &mut &b"abcde"[..], 4, 6)
            .unwrap_err()
            .kind(),
        io::ErrorKind::FileTooLarge
    );
    assert!(!batch.path().join("r-2").exists());
    assert!(!batch.path().join("r-2.part").exists());
    batch.snapshot_with_limits(2, &mut &b"ef"[..], 4, 6).unwrap();
    assert_eq!(
        batch.snapshot_with_limits(3, &mut &b"x"[..], 4, 6).unwrap_err().kind(),
        io::ErrorKind::FileTooLarge
    );
    batch.snapshot_with_limits(3, &mut &b""[..], 4, 6).unwrap();
    assert_eq!(fs::metadata(batch.path().join("r-3")).unwrap().len(), 0);
}

#[test]
fn a_stalled_snapshot_does_not_block_reading_an_already_stored_representation() {
    use std::{sync::mpsc, time::Duration};

    struct Stalled {
        entered: mpsc::Sender<()>,
        resume: mpsc::Receiver<()>,
    }
    impl Read for Stalled {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            self.entered.send(()).unwrap();
            self.resume.recv().unwrap();
            Ok(0)
        }
    }
    for state in [State::Receiving, State::Abandoned, State::Complete] {
        let data = tempfile::tempdir().unwrap();
        let batch = Batch::create(data.path(), StoreKind::SourceSnapshot, BatchId([72; 16])).unwrap();
        batch.snapshot(0, &mut &b"ready"[..]).unwrap();
        let (entered, waiting) = mpsc::channel();
        let (resume, paused) = mpsc::channel();
        let writing = batch.clone();
        let writer = std::thread::spawn(move || {
            writing.snapshot(
                1,
                &mut Stalled {
                    entered,
                    resume: paused,
                },
            )
        });
        waiting.recv().unwrap();
        let reading = batch.clone();
        let (opened, result) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            let _lease = reading.clone();
            let bytes = fs::read(reading.path().join("r-0")).unwrap();
            match state {
                State::Receiving => {}
                State::Abandoned => reading.abandon().unwrap(),
                State::Complete => reading.complete().unwrap(),
            }
            opened.send(bytes).unwrap();
        });
        let available = result.recv_timeout(Duration::from_secs(1));
        // Always release and join before asserting, including on the broken path.
        resume.send(()).unwrap();
        let stored = writer.join().unwrap();
        reader.join().unwrap();
        assert_eq!(
            available.expect("a completed representation must not wait for another download"),
            b"ready"
        );
        assert_eq!(stored.is_ok(), state == State::Receiving);
        assert_eq!(batch.path().join("r-1").exists(), state == State::Receiving);
        assert!(!batch.path().join("r-1.part").exists());
        let record = batch.0.record.lock().unwrap();
        assert_eq!(record.snapshot_bytes, 5);
        assert_eq!(record.state, state);
    }
}

#[test]
fn pins_share_cache_clear_lifetime_until_last_source_reader() {
    let data = tempfile::tempdir().unwrap();
    let id = BatchId([2; 16]);
    let batch = Batch::create(data.path(), StoreKind::SourceSnapshot, id).unwrap();
    batch.snapshot_with_limits(1, &mut &b"stored"[..], 10, 10).unwrap();
    batch.complete().unwrap();
    let reader = batch.clone();
    drop(batch);
    let (size, removed, incomplete) = usage(data.path(), StoreKind::SourceSnapshot, true);
    assert_eq!(removed, 0);
    assert!(!incomplete);
    assert!(size.bytes > 0);
    assert_eq!(size.bytes, size.kept_bytes);
    drop(reader);
    cleanup_finished();
    let (size, _, incomplete) = usage(data.path(), StoreKind::SourceSnapshot, true);
    assert!(!incomplete);
    assert_eq!(size.bytes, 0);
}

#[test]
fn abandoned_external_exporter_survives_restart_until_proven_finished() {
    let data = tempfile::tempdir().unwrap();
    let id = BatchId([3; 16]);
    let batch = Batch::create(data.path(), StoreKind::PromiseExport, id).unwrap();
    batch.external_writers_started().unwrap();
    let path = batch.path().to_owned();
    batch.abandon().unwrap();
    assert!(batch.complete().is_err());
    drop(batch); // Oink may exit; that says nothing about the external writer.
    let (size, removed, incomplete) = usage(data.path(), StoreKind::PromiseExport, true);
    assert_eq!(removed, 0);
    assert_eq!(size.bytes, size.kept_bytes);
    assert!(!incomplete);
    assert!(path.exists());
    assert!(load(&path, StoreKind::PromiseExport).unwrap().external_writers);
    let other = Batch::create(data.path(), StoreKind::PromiseExport, BatchId([4; 16])).unwrap();
    other.external_writers_started().unwrap();
    other.abandon().unwrap();
    other.external_writers_finished().unwrap();
    let other_path = other.path().to_owned();
    drop(other);
    cleanup_finished();
    assert!(!other_path.exists());
    assert!(path.exists());
}

#[test]
fn store_categories_account_and_pin_their_own_batches() {
    let data = tempfile::tempdir().unwrap();
    let mut pins = Vec::new();
    for kind in StoreKind::ALL {
        let batch = Batch::create(data.path(), kind, BatchId([5; 16])).unwrap();
        batch.snapshot_with_limits(0, &mut &b"data"[..], 10, 10).unwrap();
        let (size, removed, incomplete) = usage(data.path(), kind, true);
        assert_eq!(size.category, kind.category());
        assert!(size.bytes > 0);
        assert_eq!(size.bytes, size.kept_bytes);
        assert_eq!(removed, 0);
        assert!(!incomplete);
        pins.push(batch);
    }
    drop(pins);
    cleanup_finished();
    for kind in StoreKind::ALL {
        assert_eq!(usage(data.path(), kind, false).0.bytes, 0);
    }
}

#[test]
fn source_snapshot_last_reader_release_cleans_payload_without_manual_clear() {
    let data = tempfile::tempdir().unwrap();
    let batch = Batch::create(data.path(), StoreKind::SourceSnapshot, BatchId([19; 16])).unwrap();
    batch.snapshot(1, &mut &b"source"[..]).unwrap();
    batch.complete().unwrap();
    let path = batch.path().to_owned();
    let reader = batch.clone();
    drop(batch);
    cleanup_finished();
    assert!(path.join("r-1").exists());
    drop(reader);
    cleanup_finished();
    assert!(!path.exists());
}

#[test]
fn startup_cleans_only_unpinned_obligations_with_no_external_writer() {
    let data = tempfile::tempdir().unwrap();
    let active = Batch::create(data.path(), StoreKind::SourceSnapshot, BatchId([21; 16])).unwrap();
    let active_path = active.path().to_owned();
    // Simulate persisted records left by a dead process, without creating an
    // in-process lease or relying on a wall-clock cleanup delay.
    let root = data.path().join(StoreKind::SourceSnapshot.directory());
    let stale = root.join(BatchId([22; 16]).name());
    create_directory(&stale).unwrap();
    save(
        &stale,
        &Record {
            version: 1,
            kind: StoreKind::SourceSnapshot,
            state: State::Complete,
            external_writers: false,
            snapshot_bytes: 0,
            completed_at: Some(unix_now()),
        },
    )
    .unwrap();
    // Startup releases abandoned source snapshots, retaining any active owner.
    fs::write(stale.join("r-1"), b"unowned source payload").unwrap();
    recover(data.path());
    assert!(!stale.exists());
    assert!(active_path.exists());
}

#[test]
fn unrelated_or_malformed_batch_is_not_owned() {
    let data = tempfile::tempdir().unwrap();
    let path = data.path().join(StoreKind::SourceSnapshot.directory()).join("user");
    fs::create_dir_all(&path).unwrap();
    fs::write(path.join("keep"), "owned by user").unwrap();
    let (size, removed, incomplete) = usage(data.path(), StoreKind::SourceSnapshot, true);
    assert!(incomplete);
    assert_eq!(removed, 0);
    assert_eq!(size.kept_bytes, size.bytes);
    assert!(path.join("keep").exists());
}

#[cfg(unix)]
#[test]
fn storage_never_follows_links_and_private_snapshots_are_owner_only() {
    use std::os::unix::{fs::PermissionsExt, fs::symlink};
    let data = tempfile::tempdir().unwrap();
    let batch = Batch::create(data.path(), StoreKind::SourceSnapshot, BatchId([6; 16])).unwrap();
    batch.snapshot_with_limits(1, &mut &b"secret"[..], 10, 10).unwrap();
    assert_eq!(fs::metadata(batch.path()).unwrap().permissions().mode() & 0o777, 0o700);
    assert_eq!(
        fs::metadata(batch.path().join("r-1")).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let path = batch.path().to_owned();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("keep"), b"safe").unwrap();
    symlink(outside.path(), path.join("link")).unwrap();
    drop(batch);
    assert!(usage(data.path(), StoreKind::SourceSnapshot, true).2);
    assert!(outside.path().join("keep").exists());
    assert!(path.join("r-1").exists());
}

#[test]
fn owned_commits_and_cleanup_invalidate_without_a_filesystem_watcher() {
    let data = tempfile::tempdir().unwrap();
    let root = data.path().join(StoreKind::PromiseExport.directory());
    let batch = cache::assert_mutation(&root, || {
        Batch::create(data.path(), StoreKind::PromiseExport, BatchId([211; 16]))
    })
    .unwrap();
    cache::assert_mutation(&root, || batch.external_writers_started()).unwrap();
    fs::write(batch.path().join("external"), b"promise callback bytes").unwrap();
    cache::assert_mutation(&root, || batch.external_writers_finished()).unwrap();
    cache::assert_mutation(&root, || batch.complete()).unwrap();
    cache::assert_mutation(&root, || batch.abandon()).unwrap();

    let snapshots = data.path().join(StoreKind::SourceSnapshot.directory());
    let snapshot = Batch::create(data.path(), StoreKind::SourceSnapshot, BatchId([212; 16])).unwrap();
    cache::assert_mutation(&snapshots, || snapshot.snapshot(1, &mut &b"snapshot"[..])).unwrap();
    assert!(
        cache::assert_mutation(&snapshots, || snapshot.snapshot_with_limits(
            2,
            &mut &b"too big"[..],
            2,
            100
        ))
        .is_err()
    );

    // Create detached owned records explicitly: dropping a real Batch schedules
    // asynchronous cleanup, which would race the recovery/clear boundary here.
    for clear in [false, true] {
        let stale = root.join(if clear { "clear" } else { "recover" });
        create_directory(&stale).unwrap();
        let mut record = load(batch.path(), StoreKind::PromiseExport).unwrap();
        record.external_writers = false;
        save(&stale, &record).unwrap();
        fs::write(stale.join("payload"), b"owned bytes").unwrap();
        cache::assert_mutation(&root, || {
            if clear {
                usage(data.path(), StoreKind::PromiseExport, true);
            } else {
                recover(data.path());
            }
        });
        assert!(!stale.exists());
    }
}
