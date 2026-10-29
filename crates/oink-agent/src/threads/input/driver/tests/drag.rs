use super::*;
use oink_foundation::content::ClipId;
use oink_foundation::content::Holder;
use oink_foundation::{Point, clipboard::ClipData};
use oink_input::InputBackend;
use oink_protocol::v1::{
    self, dto,
    schema::{WireDto, WireValue},
};
fn drain_wire(read: &mut oink_network::link::RecvStream) -> Vec<v1::Message> {
    let mut messages = vec![];
    while let Ok(Some(bytes)) = read.take() {
        messages.push(
            v1::frame::OpaqueFrame::parse_exact(&bytes, v1::limits::RELIABLE_FRAME)
                .unwrap()
                .decode()
                .unwrap(),
        );
    }
    messages
}

fn id() -> ClipId {
    ClipId {
        origin: DeviceId([1; 32]),
        seq: 81,
    }
}
fn large_items() -> Vec<ClipData> {
    use oink_foundation::content::{ContentSize, FileNode, NodeId, NodeKind};
    vec![ClipData::FileRoots(
        (0..2000)
            .map(|n| FileNode {
                exec_bits: None,
                id: NodeId(n + 1),
                parent: None,
                name: format!("file-{n}-{}", "x".repeat(100)),
                kind: NodeKind::File,
                size: ContentSize::Known(2),
                version: 1,
                modified_ms: None,
            })
            .collect(),
    )]
}
fn notice(items: Vec<ClipData>) -> oink_routing::dnd::DragNotice {
    use oink_foundation::content::OfferId;
    use oink_routing::dnd::*;
    DragNotice {
        key: DragKey {
            offer: OfferId {
                clip: id(),
                generation: id().seq,
            },
            native_generation: id().seq,
        },
        revision: 1,
        input: DeviceId([3; 32]),
        state: DragState::Offered {
            holder: Holder {
                device: peer(),
                epoch: 71,
            },
            mode: SourceMode::Held,
        },
        metadata: MetadataVersion {
            revision: 1,
            readiness: MetadataReadiness::Ready,
        },
        preview: PreviewAvailability::Absent,
        items,
        retained_inventory: None,
    }
}
fn offer(items: Vec<ClipData>) -> Message {
    Message::Dnd(DndMsg::Notice(notice(items)))
}
fn focus(h: &Harness) -> Message {
    Message::Control(ControlMsg::FocusEnter {
        epoch: 71,
        display: "screen".into(),
        pos: Point::new(3., 4.),
        scope: Some(input_scope(&h.driver)),
        pointer_source: None,
    })
}
#[test]
fn prepared_large_drag_posts_head_before_focus_without_waiting_for_the_ordinary_consumer() {
    let mut h = Harness::new();
    let (mut read, _motion) = h.outbound(1);
    let (write, _datagrams) = h.inbound(1);
    opening(&write, &remote(), true);
    h.attach();
    drain(&mut read);
    while h._main.try_recv().is_ok() {}
    let items = large_items();
    let queued = super::super::super::drag::Queued::prepare(InputCommand::DndEvent(oink_routing::DndEvent::Detected {
        id: id(),
        items: items.clone(),
    }));
    assert!(queued.declaration.as_ref().unwrap().is_some());
    h.driver.queued(queued);
    let stream = h.driver.pairs[&peer()].outbound.as_ref().unwrap().0.id();
    h.driver.send(peer(), offer(items));
    h.driver.send(peer(), focus(&h));
    // No ordinary consumer has run. I0 already holds both messages in order.
    let messages = drain_wire(&mut read);
    assert_eq!(messages.len(), 2);
    let v1::Message::AttachmentHead(head) = &messages[0] else {
        panic!("head");
    };
    let header = dto::DragAttachment::from_raw(head.header().unwrap().body().unwrap())
        .unwrap()
        .notice()
        .unwrap();
    let notice = crate::dnd::wire::notice_header(&header).unwrap();
    assert_eq!(
        (
            notice.key.offer.clip,
            notice.state.holder().unwrap().epoch,
            notice.input
        ),
        (id(), 71, DeviceId([3; 32]))
    );
    let inventory = header.inventory().unwrap();
    let prefix = inventory.items().unwrap().0.len() + inventory.roots().unwrap().0.len();
    let remaining = head.remaining().unwrap() as usize;
    assert_eq!(prefix + remaining, 2000);
    assert!(remaining > 0);
    assert!(v1::frame::encode(&messages[0], v1::limits::RELIABLE_FRAME).is_ok());
    assert_eq!(
        decode(&v1::frame::encode(&messages[1], v1::limits::RELIABLE_FRAME).unwrap()).unwrap(),
        focus(&h)
    );
    assert!(h.driver.current_outbound(peer(), stream));
    let ordinary: Vec<_> = h._main.try_iter().collect();
    assert!(ordinary.iter().any(|command| matches!(command, MainCommand::Attachment { stream: s, message, .. } if *s == stream && *message == messages[0])));
    assert!(ordinary.iter().any(|command| matches!(command, MainCommand::Low { stream: s, message: v1::Message::AttachmentItems(tail), .. } if *s == stream && tail.items().unwrap().descriptor_count().unwrap() == remaining)));
}
#[test]
fn failed_head_never_launches_a_large_tail() {
    let mut h = Harness::new();
    let (mut read, _motion) = h.outbound(1);
    let (write, _datagrams) = h.inbound(1);
    opening(&write, &remote(), true);
    h.attach();
    drain(&mut read);
    while h._main.try_recv().is_ok() {}
    let items = large_items();
    h.driver
        .queued(super::super::super::drag::Queued::prepare(InputCommand::DndEvent(
            oink_routing::DndEvent::Detected {
                id: id(),
                items: items.clone(),
            },
        )));
    let bytes = encode(&focus(&h)).unwrap();
    while h.driver.pairs[&peer()]
        .outbound
        .as_ref()
        .unwrap()
        .0
        .post(bytes.as_slice())
        .is_ok()
    {}
    h.driver.send(peer(), offer(items));
    assert!(!h.driver.pairs[&peer()].attached);
    assert!(!h._main.try_iter().any(|command| matches!(
        command,
        MainCommand::Low {
            message: v1::Message::AttachmentItems(_),
            ..
        } | MainCommand::Attachment { .. }
    )));
}
#[test]
fn stale_drag_completion_cannot_install_a_declaration_for_the_new_pair() {
    let mut h = Harness::new();
    let (_read, _motion) = h.outbound(1);
    let (first, _datagrams) = h.inbound(1);
    opening(&first, &remote(), true);
    h.attach();
    let stream = h.driver.pairs[&peer()].inbound.as_ref().unwrap().0.id();
    let queued = super::super::super::drag::Queued::prepare(InputCommand::DndItems {
        peer: peer(),
        stream,
        notice: notice(large_items()),
        incomplete: false,
    });
    let (replacement, _motion2) = h.inbound(2);
    opening(&replacement, &remote(), true);
    h.attach();
    h.driver.queued(queued);
    assert!(h.driver.declarations.is_empty());
}

#[test]
fn attachment_refusals_follow_the_exact_input_pair_and_are_not_input_stream_failures() {
    let mut h = Harness::new();
    let (mut read, _motion) = h.outbound(1);
    let (write, _datagrams) = h.inbound(1);
    opening(&write, &remote(), true);
    h.attach();
    drain(&mut read);
    while h._main.try_recv().is_ok() {}
    let incoming = h.driver.pairs[&peer()].inbound.as_ref().unwrap().0.id();
    let outgoing = h.driver.pairs[&peer()].outbound.as_ref().unwrap().0.id();
    let result = v1::Message::AttachmentResult(
        dto::AttachmentResult::from_raw(
            [
                (1, 4u64.write()),
                (2, dto::AttachmentKind::Drag.write()),
                (3, dto::AttachmentOutcome::Busy.write()),
                (4, crate::dnd::wire::key(notice(vec![]).key).write()),
                (5, 0u32.write()),
            ]
            .into(),
        )
        .unwrap(),
    );
    write
        .post(v1::frame::encode(&result, v1::limits::RELIABLE_FRAME).unwrap())
        .unwrap();
    assert!(h.driver.read());
    assert!(h.driver.adapter.attached(peer()));
    assert!(h._main.try_iter().any(|command| matches!(command, MainCommand::Attachment { stream, message, .. } if stream == incoming && message == result)));
    h.driver.command(InputCommand::SendMessage {
        peer: peer(),
        stream: outgoing,
        message: result.clone(),
    });
    assert_eq!(drain_wire(&mut read), vec![result.clone()]);
    h.driver.command(InputCommand::OutboundGone { peer: peer() });
    while h._main.try_recv().is_ok() {}
    write
        .post(v1::frame::encode(&result, v1::limits::RELIABLE_FRAME).unwrap())
        .unwrap();
    h.driver.read();
    assert!(
        !h._main
            .try_iter()
            .any(|command| matches!(command, MainCommand::Attachment { .. }))
    );
    let mut wrong = match result {
        v1::Message::AttachmentResult(value) => value.into_raw(),
        _ => unreachable!(),
    };
    wrong.insert(2, dto::AttachmentKind::Offer.write());
    wrong.insert(4, oink_clipboard::wire::offer_id(notice(vec![]).key.offer).write());
    let wrong = v1::Message::AttachmentResult(dto::AttachmentResult::from_raw(wrong).unwrap());
    // A clipboard-family reply cannot be admitted on I0.
    assert!(
        wire::decode(
            &v1::frame::encode(&wrong, v1::limits::RELIABLE_FRAME).unwrap(),
            &fixture_admission(peer(), LinkId(1)),
            false
        )
        .is_err()
    );
}

#[test]
fn pending_artwork_never_fences_motion_while_ordinary_consumer_is_stalled() {
    let backend = Arc::new(oink_test_support::input::FakeInput::new());
    let mut h = Harness::with_backend(backend.clone());
    h.driver.dispatch_dnd(oink_routing::DndCmd::Begin {
        grant: crate::dnd::test_target(id(), 71),
        id: id(),
        epoch: 71,
        at: Point::default(),
        items: vec![],
        retained_inventory: None,
    });
    let begin = loop {
        let command = h._main.recv_timeout(Duration::from_secs(1)).unwrap();
        if matches!(command, MainCommand::Dnd { .. }) {
            break command;
        }
    };
    let image = oink_foundation::dnd::DragImage {
        png: vec![].into(),
        width: 1.,
        height: 1.,
        hotspot_x: 0.,
        hotspot_y: 0.,
    };
    h.driver.dispatch_dnd(oink_routing::DndCmd::UpdateImage {
        id: id(),
        epoch: 71,
        image,
    });
    let target = Point::new(81., 92.);
    h.driver.inject.push(InjectItem::Inject {
        event: InjectEvent::MoveTo { pos: target },
        from: None,
    });
    let deadline = Instant::now() + Duration::from_millis(100);
    while backend.cursor_position().unwrap() != target && Instant::now() < deadline {
        std::thread::yield_now();
    }
    let observed = backend.cursor_position().unwrap();
    drop(begin);
    while h._main.try_recv().is_ok() {} // Release a regression's DndCompletion before joining the injector.
    assert_eq!(
        observed, target,
        "artwork cannot wait for the ordinary consumer on the pointer queue"
    );
}

#[test]
fn preview_pixels_and_encoding_leave_input_through_an_ordinary_command() {
    use oink_routing::dnd::{DndMsg, PreviewBinding};
    let mut h = Harness::new();
    let (mut wire, _) = h.outbound(1);
    let (incoming, _) = h.inbound(1);
    opening(&incoming, &remote(), true);
    h.attach();
    drain(&mut wire);
    while h._main.try_recv().is_ok() {}
    // Deliberately invalid: input must not even run the encoder/validator.
    // The ordinary actor owns validation and rejects it before sending Low.
    let image = oink_foundation::dnd::DragImage {
        png: vec![0; 32 * 1024].into(),
        width: 256.,
        height: 192.,
        hotspot_x: 128.,
        hotspot_y: 96.,
    };
    let pixels = image.png.clone();
    let notice = notice(vec![]);
    h.driver.send(
        peer(),
        Message::Dnd(DndMsg::Preview {
            id: notice.key.offer.clip,
            epoch: 71,
            binding: PreviewBinding {
                key: notice.key,
                metadata_revision: 1,
                preview_revision: 1,
            },
            image: Some(image),
        }),
    );
    let queued = h
        ._main
        .try_iter()
        .find_map(|command| match command {
            MainCommand::DndPreview {
                message: DndMsg::Preview { image: Some(image), .. },
                ..
            } => Some(image),
            _ => None,
        })
        .expect("preview must be handed unchanged to an ordinary worker");
    assert!(Arc::ptr_eq(&pixels, &queued.png));
    assert!(drain_wire(&mut wire).is_empty(), "preview pixels never enter I0");
}

#[test]
fn exported_browser_artwork_larger_than_i0_does_not_detach_or_lose_focus() {
    use image::ImageEncoder;
    use oink_protocol::v1::RawValue;
    use oink_routing::dnd::{PreviewBinding, SourceMode};
    let mut pixels = vec![255u8; 64 * 96 * 4];
    let mut noise = 1u32;
    for pixel in pixels.as_chunks_mut::<4>().0 {
        for channel in &mut pixel[..3] {
            noise ^= noise << 13;
            noise ^= noise >> 17;
            noise ^= noise << 5;
            *channel = noise as u8;
        }
    }
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(&pixels, 64, 96, image::ExtendedColorType::Rgba8)
        .unwrap();
    assert!(png.len() > v1::limits::RELIABLE_FRAME && png.len() <= 32768);
    let image = oink_foundation::dnd::DragImage {
        png: png.into(),
        width: 64.,
        height: 96.,
        hotspot_x: 32.,
        hotspot_y: 48.,
    };
    assert!(image.valid());
    let mut h = Harness::new();
    let (mut read, _) = h.outbound(1);
    let (write, _) = h.inbound(1);
    opening(&write, &remote(), true);
    h.attach();
    drain(&mut read);
    while h._main.try_recv().is_ok() {}
    let items = vec![ClipData::FileRoots(vec![oink_foundation::content::FileNode {
        exec_bits: None,
        id: oink_foundation::content::NodeId(1),
        parent: None,
        name: "image.png".into(),
        kind: oink_foundation::content::NodeKind::File,
        size: oink_foundation::content::ContentSize::Unknown,
        version: 0,
        modified_ms: None,
    }])];
    let queued =
        super::super::super::drag::Queued::prepare(InputCommand::DndEvent(oink_routing::DndEvent::SourceExported {
            id: id(),
            epoch: 70,
            items: Ok(items.clone()),
        }));
    h.driver.declarations.insert(None, queued.declaration.unwrap().unwrap());
    let mut notice = notice(items);
    notice.state = oink_routing::dnd::DragState::Offered {
        holder: Holder {
            device: peer(),
            epoch: 71,
        },
        mode: SourceMode::Exported,
    };
    let binding = PreviewBinding {
        key: notice.key,
        metadata_revision: 1,
        preview_revision: 1,
    };
    h.driver.send(peer(), Message::Dnd(DndMsg::Notice(notice)));
    h.driver.send(
        peer(),
        Message::Dnd(DndMsg::Preview {
            id: id(),
            epoch: 71,
            binding,
            image: Some(image.clone()),
        }),
    );
    h.driver.send(peer(), focus(&h));
    let messages = drain_wire(&mut read);
    assert!(
        h.driver.pairs[&peer()].attached,
        "optional artwork detached the input stream"
    );
    assert_eq!(messages.len(), 2);
    assert!(matches!(&messages[0], v1::Message::DndNotice(_)));
    assert_eq!(
        decode(&v1::frame::encode(&messages[1], v1::limits::RELIABLE_FRAME).unwrap()).unwrap(),
        focus(&h)
    );
    assert!(
        h._main
            .try_iter()
            .any(|command| matches!(command, MainCommand::DndPreview { .. }))
    );

    // Replay the former inline field with the same otherwise-valid declaration.
    // A legal <=32 KiB thumbnail overflows the 16 KiB I0 frame before FocusEnter.
    let v1::Message::DndNotice(notice) = messages[0].clone() else {
        unreachable!()
    };
    let mut legacy = notice.into_raw();
    legacy.insert(
        8,
        dto::DragImage::from_raw(
            [
                (1, RawValue::Bytes(image.png.to_vec())),
                (2, 64f64.write()),
                (3, 96f64.write()),
                (4, 32f64.write()),
                (5, 48f64.write()),
            ]
            .into(),
        )
        .unwrap()
        .write(),
    );
    let legacy = v1::Message::DndNotice(dto::DndNotice::from_raw(legacy).unwrap());
    assert_eq!(
        v1::frame::encode(&legacy, v1::limits::RELIABLE_FRAME).unwrap_err().code,
        v1::ErrorCode::LimitExceeded
    );
}
