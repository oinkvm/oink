//! Opt-in real TCP/TLS Low + real diagnostics collection on a copied recorder.
//! No installed application, native input backend or non-loopback peer is used.
use super::*;
use std::fs;

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
                assert_eq!(after_value.first_us, 23 - id as i64);
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
    // Both encoders must stop at the fourth segment in newest-first order.
    let meta = &source.segments[source.segments.len() - 4];
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
