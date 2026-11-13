//! Adversarial transfers use the real admitted Low channel and retained codec.
use super::*;
use crate::streams::lifecycle_tests::Fixture;
use oink_protocol::v2::schema::{WireDto, WireValue};

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
                assert_eq!(error.code().unwrap(), v2::ErrorCode::UnsupportedValue.number() as u16);
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
            ("code", (v2::ErrorCode::UnsupportedValue.number() as u16).write()),
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

#[test]
fn mobile_owner_uploads_to_the_desktop_proxy() {
    // This fixture combines both session startup futures and a complete
    // diagnostic operation. Debug builds need more than libtest's default stack.
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(run_mobile_owner_upload)
        .unwrap()
        .join()
        .unwrap();
}

fn run_mobile_owner_upload() {
    use crate::config_module::{ConfigDeps, ConfigModule, group_store};
    use oink_foundation::{realtime::Tier, session::Boot};
    use oink_mobile_core::{
        DiagnosticCode, DiagnosticEvent, DiagnosticKind, MobileDiagnostics, MobileNetwork, MobileNetworkConfig,
        MobileProcess,
    };
    use oink_network::{group_state::State, identity::Identity, link::LinkConfig, negotiation::OfferProfile};

    struct Application;
    impl crate::config_module::ConfigApplication for Application {
        fn apply_state(&self, _: &group_store::StoredGroupState) -> Result<(), String> {
            Ok(())
        }
    }

    crate::test_watchdog::run(
        module_path!(),
        "mobile_owner_uploads_to_the_desktop_proxy",
        Some(4),
        async {
            let desktop_dir = tempfile::tempdir().unwrap();
            let desktop_identity = Arc::new(Identity::from_seed([41; 32]));
            let phone_id = Identity::from_seed([42; 32]).id();
            let state = State::create(&desktop_identity, "desktop", "macos", 100).unwrap();
            let roster = state
                .roster()
                .add(&desktop_identity, phone_id, "phone", "android", 101)
                .unwrap();
            let state = state.with_roster(&roster).unwrap();
            let stored = group_store::StoredGroupState::new(desktop_identity.id(), state.wire().clone()).unwrap();
            group_store::save(&desktop_dir.path().join(group_store::STATE_FILE), &stored).unwrap();
            let network = oink_test_support::network_config(desktop_dir.path().into(), "desktop", 0);
            let (desktop, port) = LinkLayer::start(LinkConfig {
                profile: OfferProfile {
                    capabilities: v2::registry::CAPABILITIES.iter().map(|cap| (*cap).to_owned()).collect(),
                    formats: oink_clipboard::negotiation::formats(),
                    audio: oink_audio::negotiation::audio(),
                    ..Default::default()
                },
                identity: desktop_identity.clone(),
                boot: Boot {
                    device_id: desktop_identity.id(),
                    nonce: [41; 16],
                },
                bind: (network.bind_ip, network.port).into(),
                tier: Tier::Elevated,
            })
            .await
            .unwrap();
            let mut diagnostics_port = desktop.register(&[StreamKind::Diagnostics]);
            let config = ConfigModule::start_with_config(
                ConfigDeps {
                    config: Arc::new(Mutex::new(
                        crate::config::AgentConfig::load_from_with_device_name(desktop_dir.path(), "desktop".into())
                            .unwrap(),
                    )),
                    owner_gate: Arc::new(Mutex::new(())),
                    data_dir: desktop_dir.path().into(),
                    links: desktop.clone(),
                    port,
                    identity: desktop_identity,
                    application: Arc::new(Application),
                    pairing: true,
                },
                network,
            )
            .unwrap();
            let phone_dir = tempfile::tempdir().unwrap();
            let phone = MobileNetwork::start(MobileNetworkConfig {
                identity_seed: [42; 32],
                boot: Boot {
                    device_id: phone_id,
                    nonce: [42; 16],
                },
                bind: (std::net::Ipv4Addr::LOCALHOST, 0).into(),
                group_state: Some(state.wire().encode(v2::limits::CONFIG_STATE).unwrap()),
                group_state_path: phone_dir.path().join("group-state.v2"),
                pairing_journal_path: phone_dir.path().join("pairing-recovery.cbor"),
                device_name: "phone".into(),
                os: "android".into(),
                form_factor: None,
                trusted_peers: Vec::new(),
                trust_path: None,
                process: MobileProcess::App,
            })
            .await
            .unwrap();
            phone.dial(desktop.id(), vec![desktop.local_addr()]).unwrap();
            let ready = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if let Some(link) = desktop.links().by_peer(phone_id).filter(|entry| {
                        entry.state == LinkState::Ready
                            && config
                                .members()
                                .members
                                .iter()
                                .any(|member| member.device == phone_id && member.online)
                    }) {
                        break link.link;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            let mut owner = DiagnosticsStreams::new(desktop.clone(), config.clone());
            owner.link_ready(ready, phone_id);
            owner.set_hello("desktop-test".into(), true, None);
            tokio::time::timeout(Duration::from_secs(3), async {
                while phone.diagnostics_hello(desktop.id()).is_none() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            let dir = tempfile::tempdir().unwrap();
            let recorder = crate::diagnostics::tests::isolated(
                dir.path().into(),
                desktop.id(),
                Some(config.applied().group),
                true,
            )
            .unwrap();
            recorder.wait_recovery();
            let receiver = Collector {
                recorder: recorder.clone(),
                context: super::tests::context(desktop.clone(), config.clone()),
            };
            let collector = MobileDiagnostics::with_ids(
                Arc::new(phone.diagnostic_sink().unwrap()),
                phone.device_id(),
                "bd28300e-df4b-4e6f-9ce4-d2ae8082fb90",
                "9642ea15-bac5-4ce0-97e3-a4fdb01d0187",
                16 * 1024,
                16 * 1024,
            );
            collector
                .emit(DiagnosticEvent {
                    timestamp_ms: 100,
                    kind: DiagnosticKind::Status,
                    code: DiagnosticCode::Started,
                    metric: None,
                    count: 1,
                })
                .unwrap();
            collector.flush().unwrap();
            let incoming = tokio::time::timeout(Duration::from_secs(18), async {
                loop {
                    if let Some(ModuleEvent::InboundLow(incoming)) = diagnostics_port.next().await {
                        return *incoming;
                    }
                }
            })
            .await
            .expect("mobile uploads a Low operation");
            owner.inbound_low(incoming);
            let [operation]: [oink_network::link::InboundLow; 1] =
                owner.take_low().try_into().expect("admitted mobile upload");
            receiver.serve(operation.stream, operation.first).await.unwrap();
            let source = phone.device_id();
            assert!(
                lock(&recorder.inner.store)
                    .mobile_proxy
                    .has_source_in_range(&source, 0, i64::MAX)
            );
            phone.stop().await;
            recorder.shutdown();
            config.stop().await;
            desktop.stop().await;
        },
    );
}
