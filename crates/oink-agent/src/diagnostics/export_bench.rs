//! Opt-in benchmark on a read-only copy of a recorder, without starting a node or
//! contacting peers. Run optimized tests with debug assertions enabled, setting
//! OINK_DIAGNOSTICS_BENCH_SOURCE to the copied Diagnostics directory and
//! OINK_DIAGNOSTICS_BENCH_OUTPUT to an evidence directory outside the repository.
//! OINK_DIAGNOSTICS_BENCH_LEGACY_STAGING compares the previous all-history
//! staging path in the same build; package bytes should be identical.
use super::*;

#[test]
#[ignore = "requires an explicit copied recorder and evidence directory"]
fn copied_recorder_export_benchmark() -> anyhow::Result<()> {
    let source = PathBuf::from(std::env::var("OINK_DIAGNOSTICS_BENCH_SOURCE")?);
    let output = PathBuf::from(std::env::var("OINK_DIAGNOSTICS_BENCH_OUTPUT")?);
    let legacy = std::env::var_os("OINK_DIAGNOSTICS_BENCH_LEGACY_STAGING").is_some();
    let temp = tempfile::tempdir_in(&output)?;
    let device = fs::read_to_string(source.join("domain"))?;
    let mut store = Store::open(temp.path().into(), device, true)?;
    let (raw_bytes, row_count) = load_copy(&source, &mut store, false)?;
    let latest = store.segments.back().unwrap();
    let cutoff = clock::Stamp {
        utc_us: latest.last_us,
        mono_us: latest.last_mono_us,
        clock_id: latest.clock_id,
    };
    println!(
        "inventory segments={} compressed_bytes={} raw_segment_bytes={raw_bytes} rows={row_count}",
        store.segments.len(),
        store.bytes
    );
    let subscriber = tracing_subscriber::fmt().with_ansi(false).without_time().finish();
    tracing::subscriber::with_default(subscriber, || -> anyhow::Result<()> {
        for (label, minutes) in [("30min", Some(30)), ("all", None)] {
            let started = Instant::now();
            let range = (!legacy).then(|| Store::save_range(Reason::Manual, minutes, cutoff.utc_us));
            let mut snapshot = store.export_snapshot_at(cutoff, range)?;
            let linked = started.elapsed();
            snapshot.prepare_range(Reason::Manual, minutes)?;
            let prepared = started.elapsed();
            let bundle = snapshot.export_sync(Reason::Manual, 0, 0, Some(true))?;
            let exported = started.elapsed();
            let segments = snapshot.segments.len();
            let source_bytes = snapshot.bytes;
            drop(snapshot);
            let elapsed = started.elapsed();
            fs::copy(&bundle.path, output.join(format!("{label}.zip")))?;
            println!(
                "result {}",
                serde_json::json!({"range":label,"legacy_staging":legacy,"segments":segments,"compressed_source_bytes":source_bytes,"hardlink_ms":linked.as_secs_f64()*1000.,"range_ms":(prepared-linked).as_secs_f64()*1000.,"export_ms":(exported-prepared).as_secs_f64()*1000.,"cleanup_ms":(elapsed-exported).as_secs_f64()*1000.,"total_ms":elapsed.as_secs_f64()*1000.,"zip_bytes":bundle.bytes})
            );
        }
        Ok(())
    })
}

pub(in crate::diagnostics) fn load_copy(
    source: &Path,
    store: &mut Store,
    remap_device: bool,
) -> anyhow::Result<(usize, usize)> {
    let mut raw_bytes = 0;
    let mut row_count = 0;
    for file in fs::read_dir(source.join("buffer"))? {
        let path = file?.path();
        if path.extension().is_none_or(|ext| ext != "dseg") {
            continue;
        }
        let bytes = fs::read(&path)?;
        let mut segment = decode(&bytes)?;
        if remap_device {
            // Only the producing segment alias is adapted to the isolated
            // fixture's membership. Original safe events and snapshots remain.
            segment.device.clone_from(&store.device);
        }
        let bytes = if remap_device { encode(&segment)? } else { bytes };
        raw_bytes += serde_json::to_vec(&segment)?.len();
        row_count += segment.rows.len();
        let mut meta = Store::meta(&segment, bytes.len() as u64);
        // Local receive metadata is not persisted in .dseg. Give this
        // reconstructed index a stable timestamp for byte-for-byte A/B checks.
        meta.received_us = meta.last_us;
        fs::write(store.dir.join("buffer").join(meta.name()), &bytes)?;
        store.restore_clocks(&segment);
        store.bytes += meta.bytes;
        store.segments.push_back(meta);
    }
    store.segments.make_contiguous().sort_by_key(|m| m.last_us);
    let latest = store.segments.back().unwrap();
    let segment = decode(&store.read(latest)?)?;
    if !remap_device {
        store.instance = segment.instance;
    }
    store.snapshot = segment.snapshot;
    store.next = latest.seq + 1;
    Ok((raw_bytes, row_count))
}
