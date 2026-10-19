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
fn whole_delivery_commits_while_cache_reader_holds_previous_record() {
    let data = tempfile::tempdir().unwrap();
    let id = BatchId([71; 16]);
    let batch = Batch::create(data.path(), StoreKind::CompletedDelivery, id).unwrap();
    // Pin the exact overlap with cache accounting's load(): the previous
    // ownership record stays open throughout its atomic replacement.
    let mut accounting = fs::File::open(batch.path().join(RECORD)).unwrap();
    let receipt = batch.commit_manifest(b"whole delivery").unwrap();
    assert_eq!(receipt, *blake3::hash(b"whole delivery").as_bytes());
    let mut old = Vec::new();
    accounting.read_to_end(&mut old).unwrap();
    assert_eq!(serde_json::from_slice::<Record>(&old).unwrap().state, State::Receiving);
    let restored = Batch::restore(data.path(), id).unwrap();
    assert_eq!(restored.manifest().unwrap(), b"whole delivery");
}

#[test]
fn snapshots_fail_without_publishing_truncated_bytes_and_aggregate_all_representations() {
    let data = tempfile::tempdir().unwrap();
    let batch = Batch::create(data.path(), StoreKind::SourceSnapshot, BatchId([1; 16])).unwrap();
    let (n, hash) = batch.snapshot_with_limits(1, &mut &b"abcd"[..], 4, 6).unwrap();
    assert_eq!((n, hash), (4, *blake3::hash(b"abcd").as_bytes()));
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
fn pins_share_cache_clear_lifetime_and_completed_batches_restore_without_the_source() {
    let data = tempfile::tempdir().unwrap();
    let id = BatchId([2; 16]);
    let batch = Batch::create(data.path(), StoreKind::CompletedDelivery, id).unwrap();
    batch.snapshot_with_limits(1, &mut &b"stored"[..], 10, 10).unwrap();
    assert!(Batch::restore(data.path(), id).is_err());
    batch.complete().unwrap();
    let reader = batch.clone();
    drop(batch);
    let (size, removed, incomplete) = usage(data.path(), StoreKind::CompletedDelivery, true);
    assert_eq!(removed, 0);
    assert!(!incomplete);
    assert!(size.bytes > 0);
    assert_eq!(size.bytes, size.kept_bytes);
    drop(reader);
    let restored = Batch::restore(data.path(), id).unwrap();
    assert_eq!(fs::read(restored.path().join("r-1")).unwrap(), b"stored");
    assert_eq!(usage(data.path(), StoreKind::CompletedDelivery, true).1, 0);
    drop(restored);
    let (size, removed, incomplete) = usage(data.path(), StoreKind::CompletedDelivery, true);
    assert!(removed > 0);
    assert!(!incomplete);
    assert_eq!(size.bytes, 0);
    assert!(Batch::restore(data.path(), id).is_err());
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
    let complete = Batch::create(data.path(), StoreKind::CompletedDelivery, BatchId([20; 16])).unwrap();
    complete.commit_manifest(b"receipt").unwrap();
    let completed_path = complete.path().to_owned();
    drop(complete);
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
            manifest_hash: None,
            completed_at: Some(unix_now()),
        },
    )
    .unwrap();
    // Startup releases abandoned source snapshots, retaining any active owner.
    fs::write(stale.join("r-1"), b"unowned source payload").unwrap();
    recover(data.path());
    assert!(!stale.exists());
    assert!(active_path.exists());
    assert!(completed_path.exists());
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
fn retention_delivered_superseded_and_expired_reads() {
    let data = tempfile::tempdir().unwrap();
    let batch = Batch::create(data.path(), StoreKind::CompletedDelivery, BatchId([81; 16])).unwrap();
    batch.complete().unwrap();
    let path = batch.path().to_owned();
    let now = load(&path, StoreKind::CompletedDelivery).unwrap().completed_at.unwrap();
    batch.delivered().unwrap();
    sweep_at(data.path(), now);
    assert!(path.exists(), "current publication protects delivered content");
    let reader = batch.read_lease().unwrap();
    assert!(batch.read_lease_with(|| now + RETENTION_SECS - 1).is_ok());
    assert!(
        batch.read_lease_with(|| now + RETENTION_SECS).is_err(),
        "a retained Batch cannot create a new reader at expiry"
    );
    sweep_at(data.path(), now + RETENTION_SECS);
    assert!(path.exists(), "expiry waits for the active read");
    drop(reader);
    let _clipboard = cache::SourceLease::current(path.join("r-1"));
    sweep_at(data.path(), now + RETENTION_SECS);
    assert!(!path.exists(), "expiry ignores the publication owner");

    let batch = Batch::create(data.path(), StoreKind::CompletedDelivery, BatchId([82; 16])).unwrap();
    batch.complete().unwrap();
    let path = batch.path().to_owned();
    batch.delivered().unwrap();
    drop(batch);
    sweep_at(data.path(), now);
    assert!(!path.exists(), "delivery plus supersede evicts before 24 hours");
}

#[test]
fn retention_startup_uses_explicit_completion_and_rejects_missing_timestamp() {
    let data = tempfile::tempdir().unwrap();
    let batch = Batch::create(data.path(), StoreKind::CompletedDelivery, BatchId([83; 16])).unwrap();
    batch.complete().unwrap();
    let path = batch.path().to_owned();
    let mut record = load(&path, StoreKind::CompletedDelivery).unwrap();
    record.completed_at = Some(1);
    save(&path, &record).unwrap();
    drop(batch);
    recover(data.path());
    assert!(!path.exists(), "restart must not reset completion time");
    let batch = Batch::create(data.path(), StoreKind::CompletedDelivery, BatchId([84; 16])).unwrap();
    batch.complete().unwrap();
    let path = batch.path().to_owned();
    let mut record = load(&path, StoreKind::CompletedDelivery).unwrap();
    record.completed_at = None;
    save(&path, &record).unwrap();
    drop(batch);
    sweep_at(data.path(), unix_now() + RETENTION_SECS);
    assert!(Batch::restore(data.path(), BatchId([84; 16])).is_err());
    assert!(
        path.exists(),
        "a malformed obligation cannot be retired by guessing its age"
    );
    assert!(usage(data.path(), StoreKind::CompletedDelivery, true).2);
    assert!(path.exists());
    let mut json: serde_json::Value = serde_json::from_slice(&fs::read(path.join(RECORD)).unwrap()).unwrap();
    json.as_object_mut().unwrap().remove("completed_at");
    fs::write(path.join(RECORD), serde_json::to_vec(&json).unwrap()).unwrap();
    assert!(load(&path, StoreKind::CompletedDelivery).is_err());
}

#[test]
fn retention_keeps_unread_before_deadline_and_expires_exactly_at_deadline() {
    let data = tempfile::tempdir().unwrap();
    let batch = Batch::create(data.path(), StoreKind::CompletedDelivery, BatchId([86; 16])).unwrap();
    batch.complete().unwrap();
    let path = batch.path().to_owned();
    let completed = load(&path, StoreKind::CompletedDelivery).unwrap().completed_at.unwrap();
    drop(batch);
    sweep_at(data.path(), completed + RETENTION_SECS - 1);
    assert!(Batch::restore_at(data.path(), BatchId([86; 16]), completed + RETENTION_SECS - 1).is_ok());
    assert!(Batch::restore_at(data.path(), BatchId([86; 16]), completed + RETENTION_SECS).is_err());
    assert!(path.exists());
    sweep_at(data.path(), completed - 1); // backward wall-clock adjustment cannot age a new record
    assert!(path.exists());
    sweep_at(data.path(), completed + RETENTION_SECS);
    assert!(!path.exists());
    assert_eq!(usage(data.path(), StoreKind::CompletedDelivery, false).0.bytes, 0);
}

#[cfg(windows)]
#[test]
fn placeholder_retention_uses_the_existing_completion_deadline() {
    let data = tempfile::tempdir().unwrap();
    let batch = Batch::create(data.path(), StoreKind::CompletedDelivery, BatchId([97; 16])).unwrap();
    assert!(batch.remaining_retention().is_none());
    batch.complete().unwrap();
    let remaining = batch.remaining_retention().unwrap();
    assert!(remaining <= std::time::Duration::from_secs(RETENTION_SECS));
    assert!(remaining > std::time::Duration::from_secs(RETENTION_SECS - 10));
    batch.0.record.lock().unwrap().completed_at = Some(unix_now() - RETENTION_SECS);
    assert_eq!(batch.remaining_retention(), Some(std::time::Duration::ZERO));
}
