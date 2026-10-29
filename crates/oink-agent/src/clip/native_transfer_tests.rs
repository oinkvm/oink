use super::*;
use crate::streams::{
    clipboard::{ClipboardStreams, files::FileLinks},
    lifecycle_tests::Fixture,
};
use oink_clipboard::content::{Provider, Remote, Translator};
use oink_clipboard::sync::files::BulkChannel;
use oink_clipboard::{ClipData, model::*};
use oink_foundation::clipboard::ClipPlatform;
use oink_network::link::{ModuleEvent, QueueSpec};
use oink_protocol::{
    frame::{StreamKind, StreamTag},
    v1::{self, Message},
};
use std::io::{self, Read};

struct Raw {
    inventory: Inventory,
    bytes: Vec<u8>,
}
struct NoTranslation;
impl Translator for NoTranslation {
    fn standard(&self, _: &Representation, _: &[u8]) -> io::Result<Vec<ClipData>> {
        unreachable!()
    }
}
impl Provider for Raw {
    fn inventory(&self) -> Inventory {
        self.inventory.clone()
    }
    fn open(&self, id: RepresentationId) -> io::Result<oink_clipboard::FileReader> {
        assert_eq!(id, RepresentationId(1));
        Ok(Box::new(io::Cursor::new(self.bytes.clone())))
    }
    fn translator(&self) -> Arc<dyn Translator> {
        Arc::new(NoTranslation)
    }
}
#[test]
fn native_representation_uses_authorized_bounded_tcp_operation_and_verified_local_reader() {
    crate::test_watchdog::run(
        module_path!(),
        "native_representation_uses_authorized_bounded_tcp_operation_and_verified_local_reader",
        Some(4),
        scenario(),
    );
}
async fn scenario() {
    let root = tempfile::tempdir().unwrap();
    let mut fixture = Fixture::<ClipboardStreams>::start().await;
    let (source, config) = fixture.context(0);
    let (target, _) = fixture.context(1);
    let source_link = source.links().by_peer(target.id()).unwrap().link;
    let target_link = target.links().by_peer(source.id()).unwrap().link;
    let links = FileLinks::new(target.clone(), StreamKind::Clipboard);
    links.link_ready(source.id(), target_link, source.boot());
    let id = ClipId {
        origin: source.id(),
        seq: 31,
    };
    let bytes: Vec<u8> = (0..1_700_013usize).map(|i| i.wrapping_mul(17) as u8).collect();
    let inventory = Inventory {
        roots: Vec::new(),
        id: OfferId {
            clip: id,
            generation: id.seq,
        },
        platform: if cfg!(target_os = "macos") {
            ClipPlatform::Mac
        } else {
            ClipPlatform::Windows
        },
        items: vec![ContentItem {
            virtual_files: false,
            id: ItemId(1),
            file: None,
            representations: vec![Representation {
                id: RepresentationId(1),
                format: if cfg!(target_os = "macos") {
                    NativeFormat::Mac("example.test.raw".into())
                } else {
                    NativeFormat::WindowsRegistered("Oink Test Raw".into())
                },
                medium: Medium::Bytes,
                aspect: 1,
                index: -1,
                standard: None,
                data: RepresentationData::Reader {
                    size: ContentSize::Known(bytes.len() as u64),
                },
            }],
        }],
    };
    let (tx, _) = source
        .open_stream(
            source_link,
            StreamKind::Clipboard.priority(),
            StreamTag {
                kind: StreamKind::Clipboard,
                ordinal: 1,
            },
            QueueSpec::reliable(StreamKind::Clipboard).0,
            None,
        )
        .unwrap();
    tx.post(
        v1::frame::encode(
            &Message::ClipboardOffer(
                oink_clipboard::sync::Ordering::new(source.id(), 30)
                    .capture(
                        oink_clipboard::wire::inventory(&inventory).unwrap(),
                        1,
                        oink_clipboard::Sensitivity::Normal,
                        0,
                    )
                    .unwrap()
                    .unwrap(),
            ),
            v1::limits::RELIABLE_FRAME,
        )
        .unwrap(),
    )
    .unwrap();
    let inbound = loop {
        if let Some(ModuleEvent::Inbound(stream)) = fixture.port(1).next().await {
            break stream;
        }
    };
    let (rx, _) = inbound.accept(
        QueueSpec::reliable(StreamKind::Clipboard).1,
        QueueSpec::datagram(StreamKind::Clipboard),
    );
    links.inbound(source.id(), rx.id());
    links.accept(source.id(), id);
    let registry = Arc::new(oink_clipboard::content::Registry::default());
    let lease = registry
        .register(Arc::new(Raw {
            inventory: inventory.clone(),
            bytes: bytes.clone(),
        }))
        .unwrap();
    let remote = Remote::new(
        root.path(),
        oink_clipboard::store::BatchId([51; 16]),
        inventory.clone(),
        inventory.platform,
        links,
        Demand::Clipboard,
    )
    .unwrap();
    let reader = remote.clone();
    let native_read = tokio::task::spawn_blocking(move || {
        let mut bytes = Vec::new();
        reader
            .open(RepresentationId(1))
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        bytes
    });
    let operation = loop {
        if let Some(ModuleEvent::InboundLow(operation)) = fixture.port(0).next().await {
            break operation;
        }
    };
    // Exercise the real module owner: a direct Registry test would miss a
    // Content request rejected before it reached the file service.
    use crate::streams::StreamModule;
    let mut owner = ClipboardStreams::new(source.clone(), config);
    owner.link_ready(source_link, target.id());
    owner.inbound_low(*operation);
    let mut admitted = owner.take_low();
    assert_eq!(admitted.len(), 1);
    let operation = admitted.pop().unwrap();
    let request = oink_clipboard::wire::local_content(operation.first).unwrap();
    let mut channel = runtime::LinkBulk(operation.stream);
    registry.serve(target.id(), request, &mut channel).await.unwrap();
    channel.finish();
    assert!(channel.recv().await.unwrap().is_none());
    assert_eq!(native_read.await.unwrap(), bytes);
    lease.cancel();
    drop(lease);
    // No live source or network request is needed after the first verified read.
    let local = tokio::task::spawn_blocking(move || {
        let mut bytes = Vec::new();
        remote
            .open(RepresentationId(1))
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        bytes
    })
    .await
    .unwrap();
    assert_eq!(local, bytes);
    drop(rx);
    drop(tx);
    fixture.stop().await;
}

#[test]
fn dnd_content_low_opens_when_input_status_precedes_link_notification() {
    crate::test_watchdog::run(
        module_path!(),
        "dnd_content_low_opens_when_input_status_precedes_link_notification",
        Some(4),
        async {
            use crate::streams::dnd::DndStreams;
            use oink_clipboard::sync::files::BulkLink;
            let mut fixture = Fixture::<ClipboardStreams>::start().await;
            let (source, config) = fixture.context(0);
            let (target, _) = fixture.context(1);
            let source_link = source.links().by_peer(target.id()).unwrap().link;
            let target_link = target.links().by_peer(source.id()).unwrap().link;
            let links = FileLinks::new(target.clone(), StreamKind::Dnd);
            // The input actor and the DnD module report on separate queues.
            // Status can already contain an attached I0 before LinkReady reaches
            // this module; FileLinks has no peer entry at this first observation.
            links.inbound(
                source.id(),
                oink_network::link::StreamId {
                    link: target_link,
                    dir: oink_network::link::StreamDir::In,
                    tag: StreamTag {
                        kind: StreamKind::Input,
                        ordinal: 1,
                    },
                },
            );
            links.link_ready(source.id(), target_link, source.boot());
            let clip = ClipId {
                origin: source.id(),
                seq: 32,
            };
            links.accept(source.id(), clip);
            let bound = links.bind(source.id(), clip);
            // Three-role drags need content from the source even when the
            // physical input owner has the only I0 stream to that source.
            let mut channel = bound
                .open_with_options(
                    source.id(),
                    oink_foundation::transfer::LowOptions {
                        send_max: 4096,
                        receive_max: 4096,
                        operation_max: 8192,
                    },
                )
                .await
                .expect("DnD content authority is not a direct I0 attachment");
            let request = ContentMsg::SelectFile {
                offer: OfferId { clip, generation: 32 },
                node: NodeId(1),
                demand: Demand::Drop(oink_foundation::content::Holder {
                    device: target.id(),
                    epoch: 1,
                }),
            };
            let diagnostic_id = channel.diagnostic_id();
            assert_ne!(diagnostic_id, [0; 16], "a real Low operation has a diagnostic nonce");
            channel.send(&request).await.unwrap();
            let operation = loop {
                if let Some(ModuleEvent::InboundLow(op)) = fixture.port(0).next().await {
                    break op;
                }
            };
            assert_eq!(
                operation.stream.diagnostic_id(),
                diagnostic_id,
                "the same existing nonce joins both endpoints"
            );
            use crate::streams::StreamModule;
            let mut owner = DndStreams::new(source, config);
            owner.link_ready(source_link, target.id());
            owner.inbound_low(*operation);
            let mut admitted = owner.take_low();
            assert_eq!(admitted.len(), 1);
            let operation = admitted.pop().unwrap();
            assert_eq!(oink_clipboard::wire::local_content(operation.first).unwrap(), request);
            let mut response = runtime::LinkBulk(operation.stream);
            response
                .send(&ContentMsg::FileFailed {
                    offer: OfferId { clip, generation: 32 },
                    node: NodeId(1),
                    reason: Unavailable::NoSuchItem,
                })
                .await
                .unwrap();
            assert!(matches!(
                channel.recv().await.unwrap(),
                Some(ContentMsg::FileFailed {
                    reason: Unavailable::NoSuchItem,
                    ..
                })
            ));
            channel.finish();
            response.finish();
            assert!(channel.recv().await.unwrap().is_none());
            assert!(response.recv().await.unwrap().is_none());
            fixture.stop().await;
        },
    );
}

#[test]
fn persistent_cloud_read_uses_current_member_link_without_offer_or_input_stream() {
    crate::test_watchdog::run(
        module_path!(),
        "persistent_cloud_read_uses_current_member_link_without_offer_or_input_stream",
        Some(4),
        async {
            use crate::streams::StreamModule;
            use oink_clipboard::sync::files::BulkLink;
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("source-file");
            std::fs::write(&path, b"cloud payload").unwrap();
            let mut fixture = Fixture::<ClipboardStreams>::start().await;
            let (source, config) = fixture.context(0);
            let (target, _) = fixture.context(1);
            let source_link = source.links().by_peer(target.id()).unwrap().link;
            let target_link = target.links().by_peer(source.id()).unwrap().link;
            let grants = config.source_grants().unwrap();
            let files = content::namespace::Local::new(vec![(NodeId(1), path)]).unwrap();
            let token = grants.issue(target.id(), files.as_ref(), &[NodeId(1)]).unwrap();
            drop(files);
            let entry = grants.open(target.id(), token).unwrap().select(NodeId(1)).unwrap();
            let registry = Arc::new(content::Registry::with_source_grants(grants));
            let links = FileLinks::new(target.clone(), StreamKind::Clipboard);
            links.link_ready(source.id(), target_link, source.boot());
            let persistent = links.bind_persistent(source.id()).unwrap();
            let offer = OfferId {
                clip: ClipId {
                    origin: source.id(),
                    seq: 99,
                },
                generation: 1,
            };
            // No accepted clipboard offer or inbound I0 exists in this adapter.
            let mut channel = persistent
                .open_with_options(
                    source.id(),
                    oink_foundation::transfer::LowOptions {
                        send_max: 4096,
                        receive_max: 8192,
                        operation_max: 12288,
                    },
                )
                .await
                .unwrap();
            channel
                .send(&ContentMsg::CloudLinkRead {
                    offer,
                    token,
                    node: entry.id,
                    version: entry.version,
                    offset: 0,
                })
                .await
                .unwrap();
            let operation = loop {
                if let Some(ModuleEvent::InboundLow(op)) = fixture.port(0).next().await {
                    break op;
                }
            };
            let mut owner = ClipboardStreams::new(source.clone(), config);
            owner.link_ready(source_link, target.id());
            owner.inbound_low(*operation);
            let operation = owner
                .take_low()
                .pop()
                .expect("cloud Low request admitted by clipboard module");
            let request = oink_clipboard::wire::local_content(operation.first).unwrap();
            let mut response = runtime::LinkBulk(operation.stream);
            registry.serve(target.id(), request, &mut response).await.unwrap();
            response.finish();
            assert!(
                matches!(channel.recv().await.unwrap(), Some(ContentMsg::FileBlock { bytes, .. }) if bytes == b"cloud payload")
            );
            assert!(matches!(
                channel.recv().await.unwrap(),
                Some(ContentMsg::FileEnd { size: 13, .. })
            ));
            assert!(channel.recv().await.unwrap().is_none());
            channel.finish();
            assert!(response.recv().await.unwrap().is_none());
            fixture.stop().await;
        },
    );
}
