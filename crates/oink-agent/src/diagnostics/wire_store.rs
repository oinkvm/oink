//! Retained peer CBOR lives only in a detached export, never the local recorder.
use super::*;
use oink_protocol::v1::{
    self, RawMap, RawValue, dto as w,
    schema::{WireDto, WireValue, required},
};

pub(in crate::diagnostics) fn json(value: &RawValue) -> serde_json::Value {
    // The CBOR copies in the archive are authoritative. This tagged JSON view
    // makes unknown types and float widths visible without lossy coercion.
    use serde_json::json;
    match value {
        RawValue::Uint(n) => json!(n),
        RawValue::Negative(n) => json!({"negative_argument":n}),
        RawValue::Bytes(bytes) => json!({"bytes":bytes}),
        RawValue::Text(text) => json!(text),
        RawValue::Array(values) => values.iter().map(json).collect(),
        RawValue::Map(map) => json_map(map),
        RawValue::Bool(v) => json!(v),
        RawValue::Null => serde_json::Value::Null,
        RawValue::F32(v) => json!({"f32":v}),
        RawValue::F64(v) => json!({"f64":v}),
    }
}
pub(in crate::diagnostics) fn json_map(map: &RawMap) -> serde_json::Value {
    map.iter().map(|(k, v)| (k.to_string(), json(v))).collect()
}
// DiagnosticSegment::decode has already validated all nested row/event fields.
// Reading timestamps must not clone and validate the entire event list again.
pub(in crate::diagnostics) fn rows(segment: &w::DiagnosticSegment) -> v1::Result<&[RawValue]> {
    match segment.raw().get(&8) {
        Some(RawValue::Array(rows)) => Ok(rows),
        _ => Err(v1::ErrorCode::MalformedFrame.into()),
    }
}
pub(in crate::diagnostics) fn segment(bytes: &[u8]) -> anyhow::Result<w::DiagnosticSegment> {
    let segment = w::DiagnosticSegment::decode(bytes, MAX_SEGMENT_RAW)?;
    anyhow::ensure!(
        token_ok(&segment.device()?.0) && token_ok(&segment.instance()?.0),
        "invalid diagnostic aliases"
    );
    let rows = rows(&segment)?;
    anyhow::ensure!(!rows.is_empty(), "empty diagnostic segment");
    let epoch = required::<i64>(rows[0].as_map()?, 2)?;
    for row in rows {
        anyhow::ensure!(required::<i64>(row.as_map()?, 2)? == epoch, "mixed clock epochs");
    }
    Ok(segment)
}
pub(in crate::diagnostics) fn meta(s: &w::DiagnosticSegment, bytes: u64) -> anyhow::Result<Meta> {
    let rows = rows(s)?;
    anyhow::ensure!(!rows.is_empty(), "empty diagnostic segment");
    let first = rows[0].as_map()?;
    let mut first_mono_us = required::<u64>(first, 1)?;
    let mut last_mono_us = first_mono_us;
    let mut first_us = required::<i64>(first, 3)?;
    let mut last_us = first_us;
    for row in &rows[1..] {
        let row = row.as_map()?;
        let mono = required::<u64>(row, 1)?;
        let utc = required::<i64>(row, 3)?;
        first_mono_us = first_mono_us.min(mono);
        last_mono_us = last_mono_us.max(mono);
        first_us = first_us.min(utc);
        last_us = last_us.max(utc);
    }
    Ok(Meta {
        device: s.device()?.0,
        instance: s.instance()?.0,
        seq: s.seq()?,
        clock_id: required::<i64>(first, 2)?,
        first_mono_us,
        last_mono_us,
        first_us,
        last_us,
        bytes,
        received_us: utc_us(),
        network: true,
    })
}

impl Store {
    /// One shared budget for the whole multi-peer save, charged before buffering.
    pub(in crate::diagnostics) fn receive_bytes(&mut self, bytes: usize) -> anyhow::Result<()> {
        self.network_received = self
            .network_received
            .checked_add(bytes as u64)
            .ok_or_else(|| anyhow::anyhow!("member:size_limit"))?;
        anyhow::ensure!(
            self.network_received <= v1::limits::DIAGNOSTIC_COLLECTION as u64,
            "member:size_limit"
        );
        Ok(())
    }
    pub(in crate::diagnostics) fn receive_expanded(&mut self, bytes: usize) -> anyhow::Result<()> {
        self.network_expanded = self
            .network_expanded
            .checked_add(bytes as u64)
            .ok_or_else(|| anyhow::anyhow!("member:size_limit"))?;
        anyhow::ensure!(
            self.network_expanded <= v1::limits::DIAGNOSTIC_COLLECTION as u64,
            "member:size_limit"
        );
        Ok(())
    }
    pub(in crate::diagnostics) fn network_environment(
        &mut self,
        device: &str,
        instance: &str,
        snapshot: w::Snapshot,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            device != self.device && token_ok(device) && token_ok(instance),
            "invalid diagnostic origin"
        );
        anyhow::ensure!(
            !self.network_environments.contains_key(device),
            "duplicate diagnostic environment"
        );
        self.network_environments
            .insert(device.into(), (instance.into(), snapshot));
        Ok(())
    }
    pub(in crate::diagnostics) fn add_network(
        &mut self,
        device: &str,
        bytes: &[u8],
        segment: &w::DiagnosticSegment,
    ) -> anyhow::Result<()> {
        self.check_cancelled()?;
        anyhow::ensure!(
            self.segments.iter().filter(|m| m.network).count() < 4096,
            "member:size_limit"
        );
        let meta = meta(segment, bytes.len() as u64)?;
        anyhow::ensure!(
            meta.device == device && device != self.device,
            "diagnostic origin mismatch"
        );
        anyhow::ensure!(
            !self
                .segments
                .iter()
                .any(|m| m.device == meta.device && m.instance == meta.instance && m.seq == meta.seq),
            "duplicate diagnostic segment"
        );
        let directory = self
            .export_source
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("missing collection staging"))?;
        replace(&directory.path().join(meta.name()), bytes, false)?;
        self.bytes += meta.bytes;
        self.segments.push_back(meta);
        Ok(())
    }
    pub(in crate::diagnostics) fn restrict_network(
        &self,
        meta: &Meta,
        start: i64,
        end: i64,
    ) -> anyhow::Result<Option<Meta>> {
        let mut segment = segment(&self.read(meta)?)?;
        let mut rows = Vec::new();
        for row in segment.rows()?.0 {
            let utc = row.utc_us()?;
            if utc >= start && utc <= end {
                rows.push(row.write());
            }
        }
        let path = self
            .export_source
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("missing collection staging"))?
            .path()
            .join(meta.name());
        if rows.is_empty() {
            fs::remove_file(path)?;
            return Ok(None);
        }
        segment.edit(|raw| {
            raw.insert(8, RawValue::Array(rows));
        })?;
        let bytes = segment.encode(MAX_SEGMENT_RAW)?;
        replace(&path, &bytes, false)?;
        Ok(Some(self::meta(&segment, bytes.len() as u64)?))
    }
}

pub(in crate::diagnostics) fn unknown_snapshot_fields(snapshot: &w::Snapshot) -> Vec<u32> {
    snapshot
        .raw()
        .keys()
        .filter(|key| !w::Snapshot::SCHEMA.fields.iter().any(|field| field.key == **key))
        .copied()
        .collect()
}
impl Store {
    pub(super) fn normalized_peer_coverage(
        &self,
        local_clock: i64,
    ) -> anyhow::Result<BTreeMap<String, serde_json::Value>> {
        self.network_ends.iter().map(|(device,end)| {
            let instance=self.network_environments.get(device).map(|(instance,_)|instance);
            let sample=self.clocks.iter().filter(|c|&c.device==device && Some(&c.instance)==instance && c.local_clock==local_clock && c.local_instance==self.instance).min_by_key(|c|c.uncertainty_us);
            let mut ranges=Vec::new();
            for coverage in end.coverage()?.0 {
                let first=coverage.start_us()?;let last=coverage.end_us()?;
                let offset=sample.map(|c|c.offset_us);let uncertainty=sample.map(|c|c.uncertainty_us);
                ranges.push(serde_json::json!({"source_start_us":first,"source_end_us":last,"start_us":offset.map(|o|first.saturating_sub(o)),"end_us":offset.map(|o|last.saturating_sub(o)),"uncertainty_us":uncertainty,"complete":coverage.complete()?,"failure":coverage.failure()?,"clock_calibrated":sample.is_some()}));
            }
            Ok((device.clone(),serde_json::json!(ranges)))
        }).collect()
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    fn archive_collection_end(ranges: usize) {
        let dir = tempfile::tempdir().unwrap();
        let mut local = Store::open(dir.path().into(), "a".repeat(24), true).unwrap();
        local.recover_for_test().unwrap();
        let mut local = local.export_snapshot().unwrap();
        let peer = peer_segment();
        local
            .add_network(&"b".repeat(24), &peer.encode(MAX_SEGMENT_RAW).unwrap(), &peer)
            .unwrap();
        let coverage = (0..ranges)
            .map(|i| {
                let start = 1_792_551_885_000_000 + i as u64 * 1_000_000;
                w::Coverage::from_raw(
                    [
                        (1, RawValue::Uint(start)),
                        (2, RawValue::Uint(start + 500_000)),
                        (3, RawValue::Bool(true)),
                    ]
                    .into(),
                )
                .unwrap()
                .write()
            })
            .collect();
        // These are the existing v1 fields emitted by released peers. A Low
        // transfer's end can exceed the unrelated reliable-channel ceiling.
        let end = w::DiagnosticsEnd::from_raw(
            [
                (1, RawValue::Bytes(vec![1; 16])),
                (2, RawValue::Array(coverage)),
                (3, RawValue::Bool(false)),
                (4, RawValue::Uint(0)),
            ]
            .into(),
        )
        .unwrap();
        let expected = end.encode(v1::limits::LOW_FRAME).unwrap();
        assert!(expected.len() > v1::limits::RELIABLE_FRAME);
        local.network_ends.insert("b".repeat(24), end);
        let bundle = local.export(Reason::Manual, 0, 0).unwrap();
        let mut zip = zip::ZipArchive::new(File::open(bundle.path).unwrap()).unwrap();
        let mut bytes = Vec::new();
        zip.by_name("devices/device-B/collection-end.cbor")
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        assert_eq!(
            bytes, expected,
            "retain the peer's complete coverage without coalescing"
        );
        let retained = w::DiagnosticsEnd::decode(&bytes, v1::limits::LOW_FRAME).unwrap();
        assert_eq!(retained.coverage().unwrap().0.len(), ranges);
    }
    #[test]
    fn collection_end_with_1024_ranges_survives_archive() {
        archive_collection_end(1024);
    }
    #[test]
    fn collection_end_with_1586_ranges_survives_archive() {
        archive_collection_end(1586);
    }
    fn peer_segment() -> w::DiagnosticSegment {
        let mut snapshot = Snapshot::current_build();
        snapshot.version = "retained-build".into();
        snapshot.source_tree_sha256 = "1".repeat(64);
        let local = Segment {
            schema: SCHEMA,
            device: "b".repeat(24),
            instance: "c".repeat(24),
            seq: 1,
            dropped: 0,
            snapshot,
            clock_samples: vec![],
            rows: [10, 20, 30]
                .into_iter()
                .map(|utc| Row {
                    mono_us: utc as u64,
                    clock_id: 0,
                    utc_us: utc,
                    event: Event::State {
                        kind: StateKind::RecorderTick,
                        active: true,
                        count: 1,
                        value: 0.,
                    },
                })
                .collect(),
        };
        let mut segment = super::super::super::wire_values::segment(&local, true).unwrap();
        let unknown = RawValue::Map(
            [
                (1, RawValue::F32(1.25)),
                (2, RawValue::F64(1.25)),
                (3, RawValue::Bytes(vec![0, 255])),
            ]
            .into(),
        );
        segment
            .edit(|raw| {
                raw.insert(900, unknown.clone());
                let RawValue::Map(snapshot) = raw.get_mut(&6).unwrap() else {
                    panic!()
                };
                snapshot.insert(901, unknown.clone());
                let RawValue::Array(rows) = raw.get_mut(&8).unwrap() else {
                    panic!()
                };
                for row in rows {
                    let RawValue::Map(row) = row else { panic!() };
                    row.insert(902, unknown.clone());
                    row.insert(
                        4,
                        RawValue::Map(
                            [
                                (1, RawValue::from("future-safe-event/2")),
                                (2, RawValue::Map([(1, unknown.clone())].into())),
                                (903, unknown.clone()),
                            ]
                            .into(),
                        ),
                    );
                }
            })
            .unwrap();
        segment
    }
    #[test]
    fn unknown_events_and_snapshot_trees_survive_clipping_and_archive_without_relabelling() {
        let dir = tempfile::tempdir().unwrap();
        let mut local = Store::open(dir.path().into(), "a".repeat(24), true).unwrap();
        local.recover_for_test().unwrap();
        let mut local = local.export_snapshot().unwrap();
        let original = peer_segment();
        let bytes = original.encode(MAX_SEGMENT_RAW).unwrap();
        local
            .add_network(&"b".repeat(24), &bytes, &segment(&bytes).unwrap())
            .unwrap();
        let mut current = original.snapshot().unwrap();
        current
            .edit(|raw| {
                raw.insert(2, RawValue::from("current-build"));
                raw.insert(3, RawValue::Text("2".repeat(64)));
            })
            .unwrap();
        local
            .network_environment(&"b".repeat(24), &"d".repeat(24), current.clone())
            .unwrap();
        local.clocks.push(ClockSample {
            device: "b".repeat(24),
            instance: "c".repeat(24),
            at_us: 0,
            source_clock: 0,
            local_clock: local.export_clock().clock_id,
            local_instance: local.instance.clone(),
            first_source_mono_us: 0,
            last_source_mono_us: u64::MAX,
            offset_us: 0,
            uncertainty_us: 10,
        });
        local.restrict_range(15, 25).unwrap();
        let retained = segment(&local.read(local.segments.back().unwrap()).unwrap()).unwrap();
        assert_eq!(retained.rows().unwrap().0.len(), 1);
        assert_eq!(retained.rows().unwrap().0[0], original.rows().unwrap().0[1]);
        assert_eq!(retained.raw().get(&900), original.raw().get(&900));
        assert_eq!(retained.snapshot().unwrap(), original.snapshot().unwrap());
        let bundle = local.export(Reason::Manual, 0, 0).unwrap();
        let mut archive = zip::ZipArchive::new(File::open(bundle.path).unwrap()).unwrap();
        let manifest: serde_json::Value = serde_json::from_reader(archive.by_name("manifest.json").unwrap()).unwrap();
        assert_eq!(manifest["versions"]["b".repeat(24)]["version"], "retained-build");
        assert_eq!(manifest["unrecognized_diagnostic_events"]["future-safe-event/2"], 1);
        assert!(manifest["unrecognized_snapshot_fields"]["901"].as_u64().unwrap() > 0);
        let mut bytes = Vec::new();
        archive
            .by_name("devices/device-B/environment.cbor")
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        assert_eq!(
            w::Snapshot::decode(&bytes, MAX_SEGMENT_RAW).unwrap(),
            original.snapshot().unwrap()
        );
        bytes.clear();
        archive
            .by_name("devices/device-B/current-environment.cbor")
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        assert_eq!(w::Snapshot::decode(&bytes, MAX_SEGMENT_RAW).unwrap(), current);
        bytes.clear();
        archive
            .by_name(&format!(
                "devices/device-B/segments/{}",
                local.segments.back().unwrap().name()
            ))
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        assert_eq!(segment(&bytes).unwrap(), retained);
        let timeline: String = {
            let mut text = String::new();
            archive
                .by_name("merged.ndjson")
                .unwrap()
                .read_to_string(&mut text)
                .unwrap();
            text
        };
        let row: serde_json::Value = serde_json::from_str(timeline.trim()).unwrap();
        assert_eq!(row["event"]["recognized"], false);
    }
    #[test]
    fn borrowed_rows_preserve_time_bounds_and_reject_mixed_epochs() {
        let mut source = peer_segment();
        source
            .edit(|raw| {
                let RawValue::Array(rows) = raw.get_mut(&8).unwrap() else {
                    panic!()
                };
                for (row, (mono, utc)) in rows.iter_mut().zip([(30, 10), (10, 30), (20, 20)]) {
                    let RawValue::Map(row) = row else { panic!() };
                    row.insert(1, RawValue::Uint(mono));
                    row.insert(3, RawValue::Uint(utc));
                }
            })
            .unwrap();
        let bytes = source.encode(MAX_SEGMENT_RAW).unwrap();
        let parsed = segment(&bytes).unwrap();
        let bounds = meta(&parsed, bytes.len() as u64).unwrap();
        assert_eq!((bounds.first_mono_us, bounds.last_mono_us), (10, 30));
        assert_eq!((bounds.first_us, bounds.last_us), (10, 30));
        assert_eq!(bounds.bytes, bytes.len() as u64);
        source
            .edit(|raw| {
                let RawValue::Array(rows) = raw.get_mut(&8).unwrap() else {
                    panic!()
                };
                let RawValue::Map(row) = &mut rows[1] else { panic!() };
                row.insert(2, RawValue::Uint(1));
            })
            .unwrap();
        assert!(
            segment(&source.encode(MAX_SEGMENT_RAW).unwrap())
                .unwrap_err()
                .to_string()
                .contains("mixed clock epochs")
        );
    }
    #[test]
    fn all_peers_share_one_compressed_and_expanded_collection_budget() {
        let dir = tempfile::tempdir().unwrap();
        let mut local = Store::open(dir.path().into(), "a".repeat(24), true).unwrap();
        local.receive_bytes(v1::limits::DIAGNOSTIC_COLLECTION - 1).unwrap();
        local.receive_bytes(1).unwrap();
        assert!(local.receive_bytes(1).is_err());
        local.receive_expanded(v1::limits::DIAGNOSTIC_COLLECTION - 1).unwrap();
        local.receive_expanded(1).unwrap();
        assert!(local.receive_expanded(1).is_err());
        assert!(local.segments.is_empty());
        assert_eq!(local.bytes, 0);
    }
}
