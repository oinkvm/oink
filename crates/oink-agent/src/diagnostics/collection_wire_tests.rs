//! Adversarial transfers use the real admitted Low channel and retained codec.
use super::*;
use crate::streams::lifecycle_tests::Fixture;
use oink_protocol::v1::schema::{WireDto, WireValue};

#[test]
fn unsupported_format_and_compression_refuse_only_the_low_operation() {
    crate::test_watchdog::run(
        module_path!(),
        "unsupported_format_and_compression_refuse_only_the_low_operation",
        Some(4),
        async {
            let mut fixture = Fixture::<DiagnosticsStreams>::start().await;
            let (links, config) = fixture.context(0);
            let (remote, remote_config) = fixture.context(1);
            let dir = tempfile::tempdir().unwrap();
            let recorder = crate::diagnostics::tests::isolated(
                dir.path().into(),
                remote.id(),
                Some(remote_config.applied().group),
                true,
            )
            .unwrap();
            recorder.wait_recovery();
            let mut errors = config.subscribe();
            for field in [5, 6] {
                let stamp = lock(&recorder.inner.store).current_stamp();
                let mut raw = super::tests::request(stamp, i64::MIN, stamp.utc_us).raw().clone();
                raw.insert(field, strings(&["future-value/2"]));
                let request = w::DiagnosticsRequest::from_raw(raw).unwrap();
                let request_id = request.request_id().unwrap();
                let link = links.links().by_peer(remote.id()).unwrap().link;
                let mut stream = links
                    .open_low(link, StreamKind::Diagnostics, collection_options())
                    .await
                    .unwrap();
                stream.send(&Message::DiagnosticsRequest(request)).await.unwrap();
                let incoming = super::tests::incoming(fixture.port(1)).await;
                let collector = Collector {
                    recorder: recorder.clone(),
                    context: super::tests::context(remote.clone(), remote_config.clone()),
                };
                let serving = tokio::spawn(async move { collector.serve(incoming.stream, incoming.first).await });
                let Message::DiagnosticsUnavailable(refusal) = stream.recv().await.unwrap().unwrap() else {
                    panic!("unsupported negotiation must return a correlated refusal")
                };
                assert_eq!(refusal.request_id().unwrap(), request_id);
                assert_eq!(refusal.reason().unwrap(), w::CollectionFailure::Unsupported);
                let error = tokio::time::timeout(Duration::from_secs(3), async {
                    loop {
                        if let Some(ConfigChange::ProtocolError {
                            link: source,
                            peer,
                            error,
                        }) = errors.next().await
                        {
                            assert_eq!(source, link);
                            assert_eq!(peer, remote.id());
                            break error;
                        }
                    }
                })
                .await
                .unwrap();
                assert_eq!(error.code().unwrap(), v1::ErrorCode::UnsupportedValue.number() as u16);
                assert_eq!(error.scope().unwrap(), w::ErrorScope::LowOperation);
                assert_eq!(error.offending_type().unwrap(), Some(0x0a02));
                assert_eq!(error.operation().unwrap().unwrap().0, request_id.0);
                stream.finish();
                assert!(stream.recv().await.unwrap().is_none());
                serving.await.unwrap().unwrap();
                assert!(config.peer_agreement(remote.id()).is_some(), "C0 remains admitted");
            }
            recorder.shutdown();
            fixture.stop().await;
        },
    );
}

async fn respond(
    stream: &mut LowStream,
    request: w::DiagnosticsRequest,
    device: String,
    mode: &str,
    config: &crate::config_module::ConfigModule,
) -> anyhow::Result<()> {
    let id = request.request_id()?;
    if mode.starts_with("c0-") {
        let error = dto::<w::ProtocolError>([
            ("code", (v1::ErrorCode::UnsupportedValue.number() as u16).write()),
            (
                "scope",
                if mode == "c0-other-scope" {
                    w::ErrorScope::Message
                } else {
                    w::ErrorScope::LowOperation
                }
                .write(),
            ),
            ("offending_type", 0x0a02u16.write()),
            (
                "operation",
                if mode == "c0-unrelated" {
                    RawValue::Bytes(vec![9; 16])
                } else {
                    id.write()
                },
            ),
        ])?;
        config.send_protocol_error(stream.link(), error)?;
        if mode == "c0-unsupported" {
            return Ok(());
        }
    }
    let compression = if mode == "deflate" { DEFLATE } else { NONE };
    let response = dto::<w::DiagnosticsResponse>([
        ("instance", RawValue::Text("c".repeat(24))),
        ("evicted_segments", 0u64.write()),
        ("t2", request.t1()?.write()),
        ("t3", request.t1()?.write()),
        (
            "request_id",
            if mode == "response-id" {
                RawValue::Bytes(vec![9; 16])
            } else {
                id.write()
            },
        ),
        (
            "format",
            RawValue::from(if mode == "format" { "future-format/2" } else { FORMAT }),
        ),
        ("compression", RawValue::from(compression)),
    ])?;
    stream.send(&Message::DiagnosticsResponse(response)).await?;
    if ["response-id", "format"].contains(&mode) {
        return Ok(());
    }
    let snapshot = wire_values::snapshot(&Snapshot::current_build(), true)?;
    let environment = Message::DiagnosticsEnvironment(dto::<w::DiagnosticsEnvironment>([
        ("snapshot", snapshot.write()),
        ("request_id", id.write()),
        ("format", RawValue::from(FORMAT)),
    ])?);
    if mode == "partial-environment" {
        stream
            .send(&Message::DiagnosticsEnd(dto::<w::DiagnosticsEnd>([
                ("request_id", id.write()),
                (
                    "coverage",
                    RawValue::Array(vec![
                        dto::<w::Coverage>([
                            ("start_us", 0i64.write()),
                            ("end_us", 0i64.write()),
                            ("complete", false.write()),
                            ("failure", 12u16.write()),
                        ])?
                        .write(),
                    ]),
                ),
                ("partial", true.write()),
                ("evicted_segments", 0u64.write()),
            ])?))
            .await?;
        return Ok(());
    }
    stream.send(&environment).await?;
    if mode == "duplicate-environment" {
        stream.send(&environment).await?;
        return Ok(());
    }
    let at = request.end_us()?;
    let segment = store::Segment {
        schema: SCHEMA,
        device,
        instance: "c".repeat(24),
        seq: 1,
        dropped: 0,
        snapshot: Snapshot::current_build(),
        clock_samples: vec![],
        rows: vec![Row {
            mono_us: 0,
            clock_id: 0,
            utc_us: at,
            event: Event::State {
                kind: StateKind::RecorderTick,
                active: true,
                count: 1,
                value: 0.,
            },
        }],
    };
    let raw = wire_values::segment(&segment, true)?.encode(MAX_SEGMENT_RAW)?;
    let bytes = compress(&raw, compression)?;
    let chunk = dto::<w::DiagnosticsChunk>([
        (
            "request_id",
            if mode == "chunk-request" {
                RawValue::Bytes(vec![8; 16])
            } else {
                id.write()
            },
        ),
        (
            "segment_id",
            if mode == "segment-id" {
                1u64.write()
            } else {
                0u64.write()
            },
        ),
        ("offset", if mode == "offset" { 1u32.write() } else { 0u32.write() }),
        (
            "bytes",
            RawValue::Bytes(if mode == "empty-chunk" { vec![] } else { bytes.clone() }),
        ),
    ])?;
    stream.send(&Message::DiagnosticsChunk(chunk)).await?;
    if ["chunk-request", "segment-id", "offset", "empty-chunk"].contains(&mode) {
        return Ok(());
    }
    let end = dto::<w::DiagnosticsSegmentEnd>([
        ("request_id", id.write()),
        ("segment_id", 0u64.write()),
        (
            "compressed_length",
            (bytes.len() as u32 + u32::from(mode == "compressed-length")).write(),
        ),
        (
            "raw_length",
            (raw.len() as u32 + u32::from(mode == "raw-length")).write(),
        ),
    ])?;
    stream.send(&Message::DiagnosticsSegmentEnd(end)).await?;
    if ["compressed-length", "raw-length"].contains(&mode) {
        return Ok(());
    }
    let coverage = if mode == "missing-coverage" {
        vec![]
    } else {
        vec![
            dto::<w::Coverage>([
                ("start_us", at.write()),
                ("end_us", at.write()),
                ("complete", true.write()),
            ])?
            .write(),
        ]
    };
    stream
        .send(&Message::DiagnosticsEnd(dto::<w::DiagnosticsEnd>([
            ("request_id", id.write()),
            ("coverage", RawValue::Array(coverage)),
            ("partial", false.write()),
            ("evicted_segments", 0u64.write()),
        ])?))
        .await?;
    Ok(())
}
#[test]
fn admitted_low_rejects_mixed_ids_offsets_and_lengths_before_commit() {
    crate::test_watchdog::run(
        module_path!(),
        "admitted_low_rejects_mixed_ids_offsets_and_lengths_before_commit",
        Some(4),
        async {
            let mut fixture = Fixture::<DiagnosticsStreams>::start().await;
            let (links, config) = fixture.context(0);
            let (remote, remote_config) = fixture.context(1);
            for mode in [
                "response-id",
                "format",
                "duplicate-environment",
                "chunk-request",
                "segment-id",
                "offset",
                "empty-chunk",
                "compressed-length",
                "raw-length",
                "missing-coverage",
                "partial-environment",
                "c0-unsupported",
                "c0-unrelated",
                "c0-other-scope",
                "none",
                "deflate",
            ] {
                let dir = tempfile::tempdir().unwrap();
                let source = store::Store::open(
                    dir.path().into(),
                    Anonymizer::new(config.applied().group).device(links.id()),
                    true,
                )
                .unwrap();
                let destination = Arc::new(Mutex::new(source.export_snapshot().unwrap()));
                let context = super::tests::context(links.clone(), config.clone());
                let shared = destination.clone();
                let cutoff = lock(&destination).current_stamp().utc_us;
                let device = Anonymizer::new(config.applied().group).device(remote.id());
                let peer = device.clone();
                let link = links.links().by_peer(remote.id()).unwrap().link;
                let remote_id = remote.id();
                let gathering =
                    tokio::spawn(async move { context.gather(remote_id, link, &peer, i64::MIN, cutoff, shared).await });
                let mut operation = super::tests::incoming(fixture.port(1)).await;
                let Message::DiagnosticsRequest(request) = operation.first else {
                    panic!("request")
                };
                // The request end is within t1 in production. This adversarial peer
                // echoes it, so the receiver's validation is independent of clocks.
                let response = respond(&mut operation.stream, request, device, mode, &remote_config).await;
                // The receiver can reject a malformed frame and revoke the
                // operation before the sender's final flush returns. Validate
                // the receiver's exact rejection below, not sender completion.
                if let Err(error) = &response {
                    assert!(
                        matches!(
                            error.downcast_ref::<oink_network::link::LowError>(),
                            Some(oink_network::link::LowError::Interrupted)
                        ),
                        "{mode}: {response:?}"
                    );
                }
                // A C0-only refusal completes the receiver without relying on a
                // Low EOF winning its race with the correlated control event.
                if mode != "c0-unsupported" {
                    operation.stream.finish();
                }
                let result = gathering.await.unwrap();
                let committed = lock(&destination).segments.iter().filter(|m| m.network).count();
                if ["none", "deflate", "c0-unrelated", "c0-other-scope"].contains(&mode) {
                    assert!(response.is_ok(), "{mode}: {response:?}");
                    assert!(result.is_ok(), "{mode}: {result:?}");
                    assert_eq!(committed, 1);
                } else {
                    assert!(result.is_err(), "{mode} accepted");
                    assert_eq!(committed, usize::from(mode == "missing-coverage"), "{mode}");
                }
                let expected_error = match mode {
                    "none" | "deflate" | "c0-unrelated" | "c0-other-scope" => None,
                    "partial-environment" => Some("member:partial_coverage"),
                    "c0-unsupported" => Some("member:unsupported"),
                    "raw-length" => Some("invalid diagnostic expansion length"),
                    _ => Some("invalid diagnostic operation"),
                };
                if let Some(expected) = expected_error {
                    assert_eq!(result.unwrap_err().to_string(), expected, "{mode}");
                }
                let _ = tokio::time::timeout(Duration::from_secs(2), operation.stream.recv()).await;
            }
            fixture.stop().await;
        },
    );
}
