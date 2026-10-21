//! Opt-in real TCP/TLS Low + real diagnostics collection on a copied recorder.
//! No installed application, native input backend or non-loopback peer is used.
use super::*;
use crate::streams::lifecycle_tests::Fixture;
use std::{fs, path::Path};

pub(super) fn legacy_contains(end: &w::DiagnosticsEnd, store: &store::Store, device: &str) -> bool {
    store.segments.iter().filter(|m| m.device == device).all(|m| {
        end.coverage().is_ok_and(|ranges| {
            ranges.0.iter().any(|r| {
                r.complete() == Ok(true)
                    && r.start_us().is_ok_and(|s| s <= m.first_us)
                    && r.end_us().is_ok_and(|e| e >= m.last_us)
            })
        })
    })
}

struct Frozen(i64);
impl clock::Source for Frozen {
    fn read(&self) -> (u64, i64) {
        (0, self.0)
    }
}

#[test]
fn indexed_coverage_preserves_single_interval_semantics() {
    for count in [0, 1, 1024, 1586] {
        let mut ranges: Vec<_> = (0..count).map(|i| (i * 7 - 100, i * 7 - 96)).collect();
        ranges.extend([(0, 10), (5, 20), (20, 30), (-200, -190), (i64::MIN, i64::MIN)]);
        ranges.reverse();
        let index = CoverageIndex::new(ranges.clone());
        for first in -205..count * 7 + 5 {
            for length in [0, 1, 4, 5, 12, 30] {
                let last = first + length;
                assert_eq!(
                    index.contains(first, last),
                    ranges.iter().any(|&(s, e)| s <= first && e >= last)
                );
            }
        }
        assert!(index.contains(i64::MIN, i64::MIN));
        assert!(!index.contains(i64::MIN, 0));
        assert!(!index.contains(0, 30), "overlapping ranges must not be unioned");
    }
}

#[tokio::test]
async fn parallel_encoding_preserves_bytes_order_and_first_failure() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut source = store::Store::open_with_clock(
        dir.path().into(),
        "a".repeat(24),
        true,
        clock::Clock::new(Arc::new(Frozen(1000))),
    )?;
    source.recover_for_test()?;
    for count in 0..24 {
        source.append(
            vec![Row {
                mono_us: count,
                utc_us: count as i64,
                clock_id: 0,
                event: Event::State {
                    kind: StateKind::RecorderTick,
                    active: true,
                    count,
                    value: 0.,
                },
            }],
            0,
        )?;
    }
    for rich in [false, true] {
        for compression in [NONE, DEFLATE] {
            let mut before = SegmentEncoder::new(source.export_snapshot()?, rich, compression, 0);
            let mut after = SegmentEncoder::new(source.export_snapshot()?, rich, compression, ENCODERS);
            for id in 0..24 {
                let before = before.next().await.unwrap()?;
                let after_value = after.next().await.unwrap()?;
                assert_eq!(after_value.segment_id, id);
                assert_eq!(
                    (before.first_us, before.last_us, before.raw_len, before.bytes),
                    (
                        after_value.first_us,
                        after_value.last_us,
                        after_value.raw_len,
                        after_value.bytes
                    )
                );
                assert!(after.pending.len() <= ENCODERS);
            }
            assert!(before.next().await.is_none());
            assert!(after.next().await.is_none());
        }
    }
    let meta = &source.segments[3];
    fs::write(
        dir.path()
            .join("buffer")
            .join(format!("{}-{}-{}.dseg", meta.device, meta.instance, meta.seq)),
        b"invalid",
    )?;
    for width in [0, ENCODERS] {
        let mut encoder = SegmentEncoder::new(source.export_snapshot()?, true, DEFLATE, width);
        for id in 0..3 {
            assert_eq!(encoder.next().await.unwrap()?.segment_id, id);
        }
        assert!(encoder.next().await.unwrap().is_err());
    }
    Ok(())
}
fn cutoff(source: &Path) -> anyhow::Result<i64> {
    let instance = fs::read_to_string(source.join("running"))?;
    let path = fs::read_dir(source.join("buffer"))?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            let name = path.file_stem()?.to_str()?;
            if path.extension()? != "dseg" || !name.contains(&format!("-{instance}-")) {
                return None;
            }
            Some((name.rsplit('-').next()?.parse::<u64>().ok()?, path))
        })
        .max_by_key(|(seq, _)| *seq)
        .ok_or_else(|| anyhow::anyhow!("copied recorder has no current-instance segments"))?
        .1;
    Ok(store::decode(&fs::read(path)?)?
        .rows
        .iter()
        .map(|row| row.utc_us)
        .max()
        .unwrap())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an explicit copied recorder and external evidence directory"]
async fn copied_peer_collection_benchmark() -> anyhow::Result<()> {
    let source = PathBuf::from(std::env::var("OINK_DIAGNOSTICS_BENCH_SOURCE")?);
    let output = PathBuf::from(std::env::var("OINK_DIAGNOSTICS_BENCH_OUTPUT")?);
    let end = cutoff(&source)?;
    let _ = tracing::subscriber::set_global_default(
        tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_env_filter("warn,oink_collection_diagnostics=info,oink_local_transfer=debug")
            .finish(),
    );
    let mut fixture = Fixture::<DiagnosticsStreams>::start().await;
    let (receiver, receiver_config) = fixture.context(0);
    let (sender, sender_config) = fixture.context(1);
    let active = std::env::var_os("OINK_DIAGNOSTICS_BENCH_ACTIVE").is_some();
    let activity = active.then(|| {
        let (sender, receiver) = (sender.clone(), receiver.clone());
        tokio::spawn(async move {
            loop {
                sender.user_activity(oink_network::UserActivitySource::Group);
                receiver.user_activity(oink_network::UserActivitySource::Group);
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        })
    });
    let sender_dir = tempfile::tempdir_in(&output)?;
    let recorder = Recorder::start_inner(
        sender_dir.path().into(),
        sender.id(),
        Some(sender_config.applied().group),
        true,
        true,
        clock::Clock::new(Arc::new(Frozen(end))),
        None,
        None,
        false,
    )?;
    recorder.wait_recovery();
    {
        let mut store = lock(&recorder.inner.store);
        let (raw, rows) = store::export_bench::load_copy(&source, &mut store, true)?;
        println!(
            "PEER_INVENTORY {}",
            serde_json::json!({"segments":store.segments.len(),"compressed_bytes":store.bytes,"raw_json_bytes":raw,"rows":rows})
        );
    }
    let mut receiver_context = tests::context(receiver.clone(), receiver_config);
    receiver_context.legacy_coverage = std::env::var_os("OINK_DIAGNOSTICS_BENCH_LEGACY_COVERAGE").is_some();
    let mut sender_context = tests::context(sender.clone(), sender_config);
    sender_context.legacy_sender = std::env::var_os("OINK_DIAGNOSTICS_BENCH_LEGACY_SENDER").is_some();
    let device = recorder.anonymizer().device(sender.id());
    let compare = std::env::var_os("OINK_DIAGNOSTICS_BENCH_COMPARE").is_some();
    let modes = if compare {
        vec!["serial", "parallel"]
    } else {
        vec!["configured"]
    };
    for (label, start) in [("30min", end - 30 * 60 * 1_000_000), ("all", i64::MIN)] {
        let mut reference = None;
        for repeat in 1..=3 {
            for mode in &modes {
                if compare {
                    receiver_context.legacy_coverage = *mode == "serial";
                    sender_context.legacy_sender = *mode == "serial";
                }
                let destination = tempfile::tempdir_in(&output)?;
                let mut store = store::Store::open_with_clock(
                    destination.path().into(),
                    recorder.anonymizer().device(receiver.id()),
                    true,
                    clock::Clock::new(Arc::new(Frozen(end))),
                )?;
                store.recover_for_test()?;
                let snapshot = Arc::new(Mutex::new(store.export_snapshot()?));
                let link = receiver.links().by_peer(sender.id()).unwrap().link;
                let context = receiver_context.clone();
                let target = snapshot.clone();
                let alias = device.clone();
                let peer = sender.id();
                let started = Instant::now();
                let receiving =
                    tokio::spawn(async move { context.gather(peer, link, &alias, start, end, target).await });
                let incoming = tests::incoming(fixture.port(1)).await;
                let collector = Collector {
                    recorder: recorder.clone(),
                    context: sender_context.clone(),
                };
                let serving = collector.serve(incoming.stream, incoming.first).await;
                let received = receiving.await?;
                let elapsed = started.elapsed();
                serving?;
                if label == "30min" {
                    received.as_ref().map_err(|e| anyhow::anyhow!(e.to_string()))?;
                }
                let store = lock(&snapshot);
                // Compare exact received CBOR and preserved End bytes in one
                // fixture. Only the request nonce is intentionally different.
                let segments = store
                    .segments
                    .iter()
                    .map(|m| store.read(m))
                    .collect::<anyhow::Result<Vec<_>>>()?;
                let ends = store
                    .network_ends
                    .values()
                    .map(|end| {
                        let mut raw = end.raw().clone();
                        raw.insert(1, RawValue::Bytes(vec![0; 16]));
                        w::DiagnosticsEnd::from_raw(raw)?.encode(v1::limits::LOW_FRAME)
                    })
                    .collect::<v1::Result<Vec<_>>>()?;
                let signature = (segments, ends, received.is_ok());
                if let Some(expected) = &reference {
                    assert_eq!(&signature, expected);
                } else {
                    reference = Some(signature);
                }
                println!(
                    "PEER_RESULT {}",
                    serde_json::json!({"range":label,"mode":mode,"active":active,"repeat":repeat,"elapsed_ms":elapsed.as_secs_f64()*1000.,"wire_payload_bytes":store.network_received,"raw_bytes":store.network_expanded,"segments":store.segments.len(),"complete":received.is_ok()})
                );
            }
        }
    }
    // Transport ceiling control: same admitted DiagnosticsChunk messages and
    // actual TCP/TLS Low adapter, without segment projection/decode/disk work.
    // This is separate from the real Collector::serve / Context::gather above.
    for repeat in 1..=3 {
        let link = receiver.links().by_peer(sender.id()).unwrap().link;
        let mut stream = receiver
            .open_low(link, StreamKind::Diagnostics, collection_options())
            .await?;
        let stamp = clock::Stamp {
            mono_us: 0,
            utc_us: end,
            clock_id: 0,
        };
        stream
            .send(&Message::DiagnosticsRequest(tests::request(stamp, end, end)))
            .await?;
        let incoming = tests::incoming(fixture.port(1)).await;
        let mut outgoing = incoming.stream;
        let reading = tokio::spawn(async move {
            let mut received = 0;
            while let Some(message) = stream.recv().await? {
                let Message::DiagnosticsChunk(chunk) = message else {
                    anyhow::bail!("unexpected control frame")
                };
                received += chunk.bytes()?.0.len();
            }
            stream.finish();
            Ok::<_, anyhow::Error>(received)
        });
        let started = Instant::now();
        for sequence in 0..4096u64 {
            let chunk = dto::<w::DiagnosticsChunk>([
                ("bytes", RawValue::Bytes(vec![42; CHUNK])),
                ("request_id", RawValue::Bytes(vec![1; 16])),
                ("segment_id", sequence.wire()?),
                ("offset", 0u32.wire()?),
            ])?;
            outgoing.send(&Message::DiagnosticsChunk(chunk)).await?;
        }
        outgoing.finish();
        assert!(outgoing.recv().await?.is_none());
        let bytes = reading.await??;
        assert_eq!(bytes, 64 * 1024 * 1024);
        println!(
            "LOW_CONTROL {}",
            serde_json::json!({"repeat":repeat,"bytes":bytes,"messages":4096,"elapsed_ms":started.elapsed().as_secs_f64()*1000.})
        );
    }
    if let Some(activity) = activity {
        activity.abort();
    }
    recorder.shutdown();
    fixture.stop().await;
    Ok(())
}
