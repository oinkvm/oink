use super::*;

struct Frozen(i64);
impl clock::Source for Frozen {
    fn read(&self) -> (u64, i64) {
        (0, self.0)
    }
}
fn row(utc_us: i64, count: u64) -> Row {
    Row {
        mono_us: count,
        clock_id: 0,
        utc_us,
        event: Event::State {
            kind: StateKind::RecorderTick,
            active: true,
            count,
            value: 0.,
        },
    }
}
fn manifest(bundle: &BundleInfo) -> serde_json::Value {
    let mut zip = zip::ZipArchive::new(std::fs::File::open(&bundle.path).unwrap()).unwrap();
    serde_json::from_reader(zip.by_name("manifest.json").unwrap()).unwrap()
}
fn archive_contents(bundle: &BundleInfo) -> std::collections::BTreeMap<String, Vec<u8>> {
    use std::io::Read;
    let mut zip = zip::ZipArchive::new(std::fs::File::open(&bundle.path).unwrap()).unwrap();
    (0..zip.len())
        .map(|index| {
            let mut entry = zip.by_index(index).unwrap();
            let name = entry.name().unwrap().into_owned();
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes).unwrap();
            if name == "manifest.json" {
                let mut manifest: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                // Existing boundary clipping creates fresh receipt metadata
                // using the real clock, independently of the frozen cutoff.
                // Every other manifest field and every entry byte must match.
                for meta in manifest["coverage"].as_array_mut().unwrap() {
                    meta.as_object_mut().unwrap().remove("received_us");
                }
                bytes = serde_json::to_vec(&manifest).unwrap();
            }
            (name, bytes)
        })
        .collect()
}
#[test]
fn range_snapshot_avoids_excluded_links_and_preserves_archive_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let end = 10 * 24 * 60 * 60 * 1_000_000;
    let minute = 60 * 1_000_000;
    let mut source = store::Store::open_with_clock(
        dir.path().into(),
        "a".repeat(24),
        true,
        clock::Clock::new(Arc::new(Frozen(end))),
    )
    .unwrap();
    for times in [
        vec![end - 31 * minute],
        vec![end - 31 * minute, end - 29 * minute],
        vec![end - minute],
        vec![end - 1, end + 1],
        vec![end + minute],
    ] {
        source
            .append(
                times
                    .into_iter()
                    .enumerate()
                    .map(|(i, time)| row(time, i as u64))
                    .collect(),
                0,
            )
            .unwrap();
    }
    // All-history saves retain earlier clock epochs even if their wall time
    // appears later than the current cutoff after a backstep.
    let mut historical = row(end + 2 * minute, 0);
    historical.clock_id = 42;
    source.append(vec![historical], 0).unwrap();
    for (reason, minutes) in [
        (Reason::Manual, Some(30)),
        (Reason::Manual, None),
        (Reason::Manual, Some(1)),
        (Reason::Emergency, None),
    ] {
        let mut original = source.export_snapshot().unwrap();
        original.prepare_range(reason, minutes).unwrap();
        let mut selected = source.export_snapshot_for_save(reason, minutes).unwrap();
        selected.prepare_range(reason, minutes).unwrap();
        let before = original.export(reason, 0, 0).unwrap();
        let after = selected.export(reason, 0, 0).unwrap();
        let expected = archive_contents(&before);
        assert_eq!(expected, archive_contents(&after));
        let (start, end) = original.requested_range.unwrap();
        let mut peer = source.export_snapshot_between(start, end).unwrap();
        peer.prepare_range(reason, minutes).unwrap();
        let served = peer.export(reason, 0, 0).unwrap();
        assert_eq!(expected, archive_contents(&served));
    }
    // A segment excluded by metadata need not even be present on disk. This
    // checks the avoided filesystem operation without a wall-clock assertion.
    let old = &source.segments[0];
    let old_name = format!("{}-{}-{}.dseg", old.device, old.instance, old.seq);
    std::fs::remove_file(dir.path().join("buffer").join(old_name)).unwrap();
    assert!(source.export_snapshot_for_save(Reason::Manual, Some(30)).is_ok());
    assert!(source.export_snapshot_between(end - 30 * minute, end).is_ok());
}
#[test]
fn named_segments_split_at_the_expansion_limit_without_losing_rows_or_multiplying_drops() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = store::Store::open(dir.path().into(), "a".repeat(24), true).unwrap();
    let rows = (0..1024)
        .map(|i| {
            let mut row = row(utc_us(), i);
            row.event = Event::Log {
                module: "synthetic".into(),
                line: 1,
                level: 2,
                fields: (0..32)
                    .map(|field| (format!("numeric_field_{field}"), i as f64))
                    .collect(),
                labels: vec![],
            };
            row
        })
        .collect();
    s.append(rows, 17).unwrap();
    assert!(s.segments.len() > 1);
    let segments: Vec<_> = s
        .segments
        .iter()
        .map(|meta| store::decode(&s.read(meta).unwrap()).unwrap())
        .collect();
    assert_eq!(segments.iter().map(|s| s.rows.len()).sum::<usize>(), 1024);
    assert_eq!(segments.iter().map(|s| s.dropped).sum::<u64>(), 17);
    for segment in segments {
        assert!(serde_json::to_vec(&segment).unwrap().len() <= MAX_SEGMENT_RAW);
    }
}
#[test]
fn segment_cleanup_never_touches_bundles_ordinary_logs_or_reports() {
    assert_eq!(MAX_BYTES, 67_108_864);
    let dir = tempfile::tempdir().unwrap();
    let mut s = store::Store::open(dir.path().into(), "a".repeat(24), true).unwrap();
    for name in [
        "1-manual.zip",
        "2-emergency.zip",
        "3-recovered_crash.zip",
        "oink.2026-01-01.log",
    ] {
        std::fs::write(dir.path().join(name), b"keep").unwrap();
    }
    std::fs::create_dir(dir.path().join("reports")).unwrap();
    std::fs::write(dir.path().join("reports/checkpoint.json"), b"keep").unwrap();
    s.append(vec![row(utc_us(), 1)], 0).unwrap();
    s.limit = 0;
    s.prune().unwrap();
    assert!(s.segments.is_empty());
    assert_eq!(std::fs::read_dir(dir.path().join("buffer")).unwrap().count(), 0);
    for name in [
        "1-manual.zip",
        "2-emergency.zip",
        "3-recovered_crash.zip",
        "oink.2026-01-01.log",
        "reports/checkpoint.json",
    ] {
        assert_eq!(std::fs::read(dir.path().join(name)).unwrap(), b"keep");
    }
}
#[test]
fn size_eviction_removes_oldest_segment_and_manifest_reports_retained_coverage() {
    let dir = tempfile::tempdir().unwrap();
    let end = 10 * 24 * 60 * 60 * 1_000_000;
    let mut store = store::Store::open_with_clock(
        dir.path().into(),
        "a".repeat(24),
        true,
        clock::Clock::new(Arc::new(Frozen(end))),
    )
    .unwrap();
    // Deliberately append out of timestamp order (also covers recovery order).
    for (time, count) in [(end - 60_000_000, 2), (end - 3 * 24 * 60 * 60 * 1_000_000, 1), (end, 3)] {
        store.append(vec![row(time, count)], 0).unwrap();
    }
    let oldest_bytes = store.segments[1].bytes;
    store.limit = store.bytes - oldest_bytes;
    store.prune().unwrap();
    assert_eq!(store.evicted, 1);
    assert_eq!(store.segments.iter().map(|m| m.seq).collect::<Vec<_>>(), vec![1, 3]);
    let mut snapshot = store.export_snapshot().unwrap();
    snapshot.prepare_range(Reason::Manual, None).unwrap();
    let bundle = snapshot.export(Reason::Manual, 0, 0).unwrap();
    let m = manifest(&bundle);
    assert_eq!(m["requested_range_us"], serde_json::json!([null, end]));
    assert_eq!(m["evicted_segments_by_device"]["a".repeat(24)], 1);
    let coverage = m["coverage"].as_array().unwrap();
    assert_eq!(coverage.len(), 2);
    let rows = tests::timeline(&bundle.path, "merged.ndjson");
    assert_eq!(rows.len(), 2);
    for meta in coverage {
        let times = rows
            .iter()
            .filter(|r| r["segment"] == meta["seq"])
            .map(|r| r["original_utc_us"].as_i64().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(meta["first_us"], *times.iter().min().unwrap());
        assert_eq!(meta["last_us"], *times.iter().max().unwrap());
    }
}
#[test]
fn crash_retention_never_evicts_manual_or_emergency_and_keeps_newest_oversized() {
    let dir = tempfile::tempdir().unwrap();
    let store = store::Store::open(dir.path().into(), "a".repeat(24), true).unwrap();
    for name in ["ordinary.log", "unindexed.dseg", "checkpoint.json"] {
        std::fs::write(dir.path().join(name), b"keep").unwrap();
    }
    for at_ms in 1..=12 {
        for reason in ["manual", "recovered_crash", "peer_restart", "emergency"] {
            std::fs::write(dir.path().join(format!("{at_ms}-{reason}.zip")), [0]).unwrap();
        }
    }
    let path = dir.path().join("0-recovered_crash.zip");
    std::fs::File::create(&path)
        .unwrap()
        .set_len(257 * 1024 * 1024)
        .unwrap();
    let newest = BundleInfo {
        path: path.to_string_lossy().into(),
        at_ms: 0,
        reason: Reason::RecoveredCrash,
        bytes: 257 * 1024 * 1024,
        included_device_count: None,
        missing_device_count: None,
        cutoff_complete: None,
    };
    store.retain_bundles(&newest).unwrap();
    for name in ["ordinary.log", "unindexed.dseg", "checkpoint.json"] {
        assert_eq!(std::fs::read(dir.path().join(name)).unwrap(), b"keep");
    }
    let files = store.list().unwrap();
    assert_eq!(
        files
            .iter()
            .filter(|b| matches!(b.reason, Reason::RecoveredCrash | Reason::PeerRestart))
            .count(),
        10
    );
    assert_eq!(files.iter().filter(|b| b.reason == Reason::Manual).count(), 12);
    assert_eq!(files.iter().filter(|b| b.reason == Reason::Emergency).count(), 12);
    assert!(path.exists());
    // A manual save neither runs crash retention nor deletes any manual file.
    let manual = BundleInfo {
        reason: Reason::Manual,
        ..newest
    };
    store.retain_bundles(&manual).unwrap();
    assert_eq!(store.list().unwrap().len(), 34);
}

#[test]
fn calibrated_range_clips_peer_boundary_and_updates_manifest_coverage() {
    let dir = tempfile::tempdir().unwrap();
    let end = 10 * 24 * 60 * 60 * 1_000_000;
    let start = end - 30 * 60 * 1_000_000;
    let offset = 1000;
    let local = "a".repeat(24);
    let peer = "b".repeat(24);
    let instance = "c".repeat(24);
    let store = store::Store::open_with_clock(
        dir.path().into(),
        local.clone(),
        true,
        clock::Clock::new(Arc::new(Frozen(end))),
    )
    .unwrap();
    let mut snapshot = store.export_snapshot().unwrap();
    snapshot.prepare_range(Reason::Manual, Some(30)).unwrap();
    let segment = store::Segment {
        schema: SCHEMA,
        device: peer.clone(),
        instance: instance.clone(),
        seq: 1,
        dropped: 0,
        snapshot: Snapshot::default(),
        clock_samples: vec![],
        rows: [
            start + offset - 1,
            start + offset + 1,
            end + offset - 1,
            end + offset + 1,
        ]
        .into_iter()
        .enumerate()
        .map(|(i, time)| row(time, i as u64))
        .collect(),
    };
    snapshot
        .add_gathered(&segment, &store::encode(&segment).unwrap())
        .unwrap();
    snapshot.clocks.push(ClockSample {
        device: peer.clone(),
        instance,
        at_us: end,
        source_clock: 0,
        local_clock: 0,
        local_instance: snapshot.instance.clone(),
        first_source_mono_us: 0,
        last_source_mono_us: 3,
        offset_us: offset,
        uncertainty_us: 0,
    });
    snapshot.restrict_range(start, end).unwrap();
    let bundle = snapshot.export(Reason::Manual, 0, 0).unwrap();
    let m = manifest(&bundle);
    assert_eq!(m["requested_range_us"], serde_json::json!([start, end]));
    let coverage = &m["coverage"][0];
    assert_eq!(coverage["first_us"], start + offset + 1);
    assert_eq!(coverage["last_us"], end + offset - 1);
    let rows = tests::timeline(&bundle.path, "merged.ndjson");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["original_utc_us"], coverage["first_us"]);
    assert_eq!(rows[1]["original_utc_us"], coverage["last_us"]);
    assert_eq!(rows[0]["unified_utc_us"], start + 1);
    assert_eq!(rows[1]["unified_utc_us"], end - 1);
}

#[test]
fn automatic_save_ranges_stay_thirty_minutes_independent_of_manual_choice() {
    let dir = tempfile::tempdir().unwrap();
    let end = 10 * 24 * 60 * 60 * 1_000_000;
    let recorder = Recorder::start_inner(
        dir.path().into(),
        DeviceId([1; 32]),
        Some(GroupId([2; 16])),
        true,
        true,
        clock::Clock::new(Arc::new(Frozen(end))),
        None,
        None,
        false,
    )
    .unwrap();
    recorder.wait_recovery();
    for (time, count) in [(end - 2 * 60 * 60 * 1_000_000, 1), (end - 60_000_000, 2)] {
        lock(&recorder.inner.store).append(vec![row(time, count)], 0).unwrap();
    }
    let default = recorder.save(Reason::Manual).unwrap();
    assert_eq!(
        manifest(&default)["requested_range_us"],
        serde_json::json!([end - AUTOMATIC_RANGE_US, end])
    );
    assert_eq!(tests::timeline(&default.path, "merged.ndjson").len(), 1);
    let manual = recorder.save_range(Some(1440)).unwrap();
    assert_eq!(tests::timeline(&manual.path, "merged.ndjson").len(), 2);
    for reason in [Reason::Emergency, Reason::PeerRestart, Reason::RecoveredCrash] {
        let bundle = recorder.save(reason).unwrap();
        let m = manifest(&bundle);
        assert_eq!(
            m["requested_range_us"],
            serde_json::json!([end - AUTOMATIC_RANGE_US, end])
        );
        let rows = tests::timeline(&bundle.path, "merged.ndjson");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["original_utc_us"], end - 60_000_000);
    }
    let manual = recorder.save_range(None).unwrap();
    assert_eq!(tests::timeline(&manual.path, "merged.ndjson").len(), 2);
    assert_eq!(recorder.status().segments, 2);
    recorder.shutdown();
}

#[test]
fn manual_api_default_and_explicit_all_are_distinct() {
    use crate::api::Request;
    let parse = |params| {
        serde_json::from_value::<Request>(serde_json::json!({"method":"export_diagnostics","params":params})).unwrap()
    };
    assert_eq!(
        parse(serde_json::json!({})),
        Request::ExportDiagnostics { last_minutes: Some(30) }
    );
    assert_eq!(
        parse(serde_json::json!({"last_minutes":null})),
        Request::ExportDiagnostics { last_minutes: None }
    );
}

#[test]
fn invalid_payload_discards_only_affected_segment_and_restart_preserves_history() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = store::Store::open(dir.path().into(), "a".repeat(24), true).unwrap();
    s.recover_for_test().unwrap();
    let event = Event::PointerTransition {
        sequence: 4,
        holder: None,
        epoch: None,
        accounting: Default::default(),
    };
    let mut bad = row(utc_us(), 1);
    bad.event = event.clone();
    s.append(vec![bad], 0).unwrap();
    let meta = s.segments[0].clone();
    let segment = store::decode(&s.read(&meta).unwrap()).unwrap();
    let mut raw = serde_json::to_vec(&segment).unwrap();
    raw.pop();
    let incompatible = store::encode_raw(&raw).unwrap();
    assert!(store::decode(&incompatible).is_err());
    let path = dir
        .path()
        .join("buffer")
        .join(format!("{}-{}-{}.dseg", meta.device, meta.instance, meta.seq));
    std::fs::write(&path, incompatible).unwrap();
    s.append(vec![row(utc_us(), 2)], 0).unwrap();
    s.clean_shutdown().unwrap();
    drop(s);
    let mut s = store::Store::open(dir.path().into(), "a".repeat(24), true).unwrap();
    s.recover_for_test().unwrap();
    assert_eq!(s.corrupt, 1);
    assert_eq!(s.segments.len(), 1);
    assert!(!path.exists());
    let kept = s.read(&s.segments[0]).unwrap();
    assert_eq!(store::decode(&kept).unwrap().rows.len(), 1);
    s.clean_shutdown().unwrap();
    drop(s);
    let mut s = store::Store::open(dir.path().into(), "a".repeat(24), true).unwrap();
    s.recover_for_test().unwrap();
    assert_eq!(s.corrupt, 0);
    assert_eq!(s.segments.len(), 1);
    assert_eq!(s.read(&s.segments[0]).unwrap(), kept);
}
