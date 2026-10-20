//! Exercises the native-loop owner with real bounded stream queues and a fake injector.
//! It never installs a hook/tap or calls a platform input API.
use super::*;
use oink_network::link::{LinkId, QueueSpec};
use oink_protocol::frame::{StreamKind, StreamTag};
use oink_routing::SimpleLayout;
use oink_test_support::network::{datagram_pair, reliable_pair};
use std::sync::mpsc;

fn fixture_boot(device_id: DeviceId) -> oink_foundation::session::Boot {
    oink_foundation::session::Boot {
        device_id,
        nonce: [device_id.0[0]; 16],
    }
}
fn fixture_admission(peer: DeviceId, link: LinkId) -> wire::PeerAdmission {
    wire::PeerAdmission {
        boot: fixture_boot(peer),
        link,
        agreement: Arc::new(
            oink_protocol::v1::admission::Agreement::new(
                [
                    "oink.core/1",
                    "oink.group/1",
                    "oink.clipboard/1",
                    "oink.low/1",
                    "oink.input/1",
                    "oink.input.relative/1",
                    "oink.input.gestures/1",
                    "oink.input.contacts/1",
                    "oink.input.trail/1",
                    "oink.drag/1",
                    "oink.audio.exit-volume/1",
                ]
                .into_iter()
                .map(str::to_owned),
            )
            .unwrap(),
        ),
    }
}
fn encode(message: &Message) -> oink_protocol::v1::Result<Vec<u8>> {
    let mut bytes = vec![0; oink_protocol::v1::limits::RELIABLE_FRAME];
    let used = wire::encode(message, &mut bytes)?;
    bytes.truncate(used);
    Ok(bytes)
}
fn decode(bytes: &[u8]) -> oink_protocol::v1::Result<Message> {
    let admission = fixture_admission(peer(), LinkId(1));
    let frame = oink_protocol::v1::frame::OpaqueFrame::parse_exact(bytes, oink_protocol::v1::limits::RELIABLE_FRAME)?;
    match wire::decode(bytes, &admission, frame.header.type_id == 0x0209)? {
        wire::Decoded::Packet(packet) => Ok(packet),
        wire::Decoded::Deferred(message) => crate::dnd::wire::decode(message).map(Message::Dnd),
    }
}
struct Harness {
    // Close the ordinary queue before joining injection, including unconsumed DnD fences.
    _main: crate::threads::MainReceiver,
    driver: Driver,
}
fn input_scope(driver: &Driver) -> oink_routing::control::FocusScope {
    let Some(ControlMsg::FocusState {
        session: Some(session), ..
    }) = driver
        .adapter
        .receive_state(peer())
        .and_then(|state| state.focus.as_ref())
    else {
        panic!("peer opening scope missing")
    };
    oink_routing::control::FocusScope {
        members: vec![(driver.local, driver.adapter.routing_session()), (peer(), *session)],
    }
}
fn fixture_version() -> oink_protocol::v1::dto::ConfigVersion {
    use oink_protocol::v1::{
        RawValue,
        schema::{WireDto, WireValue},
    };
    oink_protocol::v1::dto::ConfigVersion::from_raw(
        [
            (1, RawValue::from(0_u64)),
            (3, oink_network::wire_identity::device(DeviceId([1; 32])).write()),
        ]
        .into(),
    )
    .unwrap()
}
fn peer() -> DeviceId {
    DeviceId([2; 32])
}
fn tag(ordinal: u32) -> StreamTag {
    StreamTag {
        kind: StreamKind::Input,
        ordinal,
    }
}
fn remote() -> Adapter {
    Adapter::new(Router::new(
        RouterConfig::new(fixture_boot(peer()), "peer", Os::Windows),
        Arc::new(SimpleLayout::new()),
    ))
}
// Session fixtures have geometry even when their display facts have not arrived.
// An empty effective layout now intentionally has no focus targets.
fn fixture_display() -> oink_foundation::display::DisplayInfo {
    oink_foundation::display::DisplayInfo {
        key: "screen".into(),
        name: "screen".into(),
        bounds: oink_foundation::Rect::new(0., 0., 1000., 600.),
        pixel_width: 1000,
        pixel_height: 600,
        reference_extent: Some(oink_foundation::Size::new(250., 150.)),
        rotation_deg: 0,
        is_primary: true,
        is_builtin: false,
        is_virtual: false,
    }
}

fn fixture_layout(local: DeviceId) -> oink_display::layout::Layout {
    let display = fixture_display();
    let devices = BTreeSet::from([local, DeviceId([1; 32]), peer()]);
    oink_display::layout::Layout::auto(
        &devices
            .into_iter()
            .map(|d| (d, vec![display.clone()]))
            .collect::<Vec<_>>(),
    )
}
impl Harness {
    fn new() -> Self {
        Self::with_backend(Arc::new(oink_test_support::input::FakeInput::new()))
    }
    fn with_backend(backend: Arc<dyn oink_input::InputBackend>) -> Self {
        Self::with_local(DeviceId([1; 32]), backend)
    }
    fn with_local(local: DeviceId, backend: Arc<dyn oink_input::InputBackend>) -> Self {
        Self::with_boot(fixture_boot(local), backend)
    }
    fn with_boot(boot: oink_foundation::session::Boot, backend: Arc<dyn oink_input::InputBackend>) -> Self {
        let local = boot.device_id;
        let layout = fixture_layout(local);
        let devices = layout.screens.iter().map(|s| s.id.device).collect();
        let snapshots = InputSnapshots {
            mode_switch_in_callback: true,
            capture_reports_key_repeat: false,
            layout: Arc::new(Snapshot::new(RouterLayout {
                group: oink_foundation::GroupId([0; 16]),
                owners: Arc::default(),
                version: fixture_version(),
                topology: oink_routing::TopologyLayout::from_layout(&layout, &Default::default()),
                layout: Arc::new(layout),
                devices,
            })),
            settings: Arc::new(Snapshot::default()),
            environment: Arc::new(Snapshot::default()),
        };
        let (done, received) = mpsc::channel();
        let shared = Arc::new(Shared {
            commands: super::super::commands::Commands::new(),
            status: Snapshot::default(),
            status_changed: tokio::sync::Notify::new(),
            scheduler: Snapshot::default(),
            opening: Snapshot::default(),
            wake: Default::default(),
            stopped: std::sync::atomic::AtomicBool::new(false),
            backlog_requested: std::sync::atomic::AtomicBool::new(false),
            done: std::sync::Mutex::new(received),
            jitter: Arc::default(),
        });
        let inject = super::super::super::inject::start_with_backend(backend, Box::new(|_| {})).unwrap();
        let (main, recv) = crate::threads::main_channel();
        let notices = super::super::super::notices::Notices::new();
        let mut driver = Driver::new(boot, snapshots, inject, main, shared, notices, done);
        driver.set_wake(Arc::new(|| {}));
        Self { driver, _main: recv }
    }
    fn outbound(&mut self, ordinal: u32) -> (oink_network::link::RecvStream, oink_network::link::DatagramRecv) {
        let (input, read) = reliable_pair(LinkId(1), tag(ordinal), QueueSpec::reliable(StreamKind::Input).0);
        let (motion, datagrams) = datagram_pair(LinkId(1), tag(ordinal), QueueSpec::datagram(StreamKind::Input));
        self.driver
            .command(InputCommand::PeerAdmission(fixture_admission(peer(), LinkId(1))));
        self.driver.command(InputCommand::Outbound {
            peer: peer(),
            input,
            motion: Some(motion),
        });
        (read, datagrams)
    }
    fn inbound(&mut self, ordinal: u32) -> (oink_network::link::SendStream, oink_network::link::DatagramSend) {
        let (write, input) = reliable_pair(LinkId(1), tag(ordinal), QueueSpec::reliable(StreamKind::Input).1);
        let (motion, read_motion) = datagram_pair(LinkId(1), tag(ordinal), QueueSpec::datagram(StreamKind::Input));
        self.driver
            .command(InputCommand::PeerAdmission(fixture_admission(peer(), LinkId(1))));
        self.driver.command(InputCommand::Inbound {
            peer: peer(),
            input,
            motion: Some(read_motion),
            peer_os: Os::Windows,
        });
        (write, motion)
    }
    fn poll(&mut self) {
        self.driver.poll(Instant::now());
    }
    fn attach(&mut self) {
        let until = Instant::now() + Duration::from_secs(2);
        while !self.driver.adapter.attached(peer()) && Instant::now() < until {
            self.poll();
            std::thread::yield_now();
        }
        assert!(self.driver.adapter.attached(peer()));
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        self.driver.command(InputCommand::Stop);
        while !self.driver.stopped() {
            self.poll();
            std::thread::yield_now();
        }
    }
}
fn opening(write: &oink_network::link::SendStream, state: &Adapter, controllable: bool) {
    for message in state.opening_state(controllable) {
        write.post(encode(&message).unwrap()).unwrap();
    }
}
fn drain(read: &mut oink_network::link::RecvStream) -> Vec<Message> {
    let mut messages = Vec::new();
    while let Ok(Some(bytes)) = read.take() {
        messages.push(decode(&bytes).unwrap());
    }
    messages
}

#[test]
fn coalesced_motion_keeps_latest_source_time_and_button_barrier_order() {
    use oink_foundation::input::MouseButton;
    let mut h = Harness::new();
    let (mut read, _) = h.outbound(1);
    drain(&mut read);
    h.driver.budget.consume(h.driver.now);
    h.driver.budget.consume(h.driver.now);
    for (seq, time) in [(1, 100), (2, 200)] {
        h.driver.input_time = Some(CaptureTime(time));
        h.driver
            .send(peer(), Message::Input(InputMsg::Motion { seq, dx: 1., dy: 2. }));
    }
    assert!(drain(&mut read).is_empty());
    h.driver.input_time = Some(CaptureTime(300));
    h.driver.send(
        peer(),
        Message::Input(InputMsg::Button {
            button: MouseButton::Left,
            pressed: true,
        }),
    );
    assert_eq!(
        drain(&mut read),
        vec![
            Message::TimedInput {
                time: CaptureTime(200),
                input: InputMsg::Motion { seq: 2, dx: 2., dy: 4. }
            },
            Message::TimedInput {
                time: CaptureTime(300),
                input: InputMsg::Button {
                    button: MouseButton::Left,
                    pressed: true
                }
            },
        ]
    );
    h.driver.input_time = None;
}

#[test]
fn timed_forwarded_motion_stays_lossy_and_retains_original_clock() {
    let mut h = Harness::new();
    let (mut read, datagrams) = h.outbound(1);
    drain(&mut read);
    let input = InputMsg::Forwarded {
        origin: fixture_boot(peer()),
        hops: 1,
        msg: Box::new(InputMsg::RelativeMotion {
            epoch: 4,
            generation: 2,
            seq: 9,
            x: 12.,
            y: 3.,
        }),
    };
    h.driver.input_time = Some(CaptureTime(u32::MAX));
    h.driver.send_from(peer(), Message::Input(input.clone()), Some(peer()));
    let decoded = decode(&datagrams.take().unwrap().unwrap()).unwrap();
    assert_eq!(
        decoded,
        Message::TimedInput {
            time: CaptureTime(u32::MAX),
            input
        }
    );
    assert!(valid_datagram(&decoded));
    assert!(!valid_reliable(&decoded));
    assert!(read.take().unwrap().is_none());
    h.driver.input_time = None;
}

#[test]
fn input_timestamps_never_cross_the_outgoing_origins_clock_domain() {
    let mut h = Harness::new();
    let (mut read, _) = h.outbound(1);
    drain(&mut read);
    let local_input = InputMsg::Key {
        code: oink_foundation::input::KeyCode::LEFT_SHIFT,
        pressed: true,
    };
    let forwarded = InputMsg::Forwarded {
        origin: fixture_boot(peer()),
        hops: 1,
        msg: Box::new(local_input.clone()),
    };
    // A received event can trigger a local modifier snapshot or release. Its
    // timestamp belongs to the remote source, not to this outgoing plain input.
    h.driver.input_time = Some(CaptureTime(1_200_000_000));
    h.driver
        .send_from(peer(), Message::Input(local_input.clone()), Some(peer()));
    // Conversely, a local capture can release another origin's forwarded key.
    h.driver.send_from(peer(), Message::Input(forwarded.clone()), None);
    // A genuine relay must preserve the matching remote source timestamp.
    h.driver
        .send_from(peer(), Message::Input(forwarded.clone()), Some(peer()));
    assert_eq!(
        drain(&mut read),
        vec![
            Message::Input(local_input),
            Message::Input(forwarded.clone()),
            Message::TimedInput {
                time: CaptureTime(1_200_000_000),
                input: forwarded
            },
        ]
    );
}

#[test]
fn receiver_summarises_timed_sources_and_resets_on_connection_replacement() {
    let mut h = Harness::new();
    let (_reader, _) = h.outbound(1);
    let (writer, _) = h.inbound(1);
    opening(&writer, &remote(), true);
    h.attach();
    for lossy in [false, true] {
        let input = if lossy {
            InputMsg::RelativeMotion {
                epoch: 1,
                generation: 1,
                seq: 1,
                x: 1.,
                y: 0.,
            }
        } else {
            InputMsg::Motion { seq: 1, dx: 1., dy: 0. }
        };
        h.driver.received(
            peer(),
            Message::TimedInput {
                time: CaptureTime(7),
                input,
            },
            h.driver.start,
            lossy,
        );
    }
    let summary = h.driver.shared.jitter.take();
    assert_eq!(summary.reliable.ingress.count, 1);
    assert_eq!(summary.lossy.ingress.count, 1);
    assert_eq!(h.driver.jitter_sources.len(), 2);
    h.driver.detach(DeviceId([9; 32]));
    assert_eq!(h.driver.jitter_sources.len(), 2);
    let _ = h.inbound(2);
    assert!(h.driver.jitter_sources.is_empty());
}

#[test]
fn a_relay_preserves_capture_time_and_uses_each_origins_clock_baseline() {
    let mut h = Harness::with_local(DeviceId([3; 32]), Arc::new(oink_test_support::input::FakeInput::new()));
    h.driver.command(InputCommand::PeerAdmission(fixture_admission(
        DeviceId([1; 32]),
        LinkId(3),
    )));
    let (mut reader, _) = h.outbound(1);
    let (writer, _) = h.inbound(1);
    let state = remote();
    opening(&writer, &state, true);
    h.attach();
    drain(&mut reader);
    // Peer holds focus; this device relays stale-view input towards it.
    let scope = oink_routing::control::FocusScope {
        members: vec![
            (h.driver.local, h.driver.adapter.router().routing_session()),
            (peer(), state.router().routing_session()),
        ],
    };
    h.driver.message(
        peer(),
        Message::Control(ControlMsg::FocusState {
            lock_state: None,
            epoch: 5,
            holder: peer(),
            scope: Some(scope),
            session: Some(state.router().routing_session()),
        }),
    );
    assert_eq!(h.driver.adapter.focus().holder, peer());
    drain(&mut reader);
    for (origin, time, at) in [
        (DeviceId([1; 32]), 3_000_000, 100_000),
        (peer(), 9, 100_000),
        (DeviceId([1; 32]), 3_008_000, 108_000),
    ] {
        let input = InputMsg::Forwarded {
            origin: fixture_boot(origin),
            hops: 1,
            msg: Box::new(InputMsg::Motion { seq: 1, dx: 1., dy: 0. }),
        };
        h.driver.received(
            peer(),
            Message::TimedInput {
                time: CaptureTime(time),
                input,
            },
            h.driver.start + Duration::from_micros(at),
            false,
        );
    }
    let sample = h.driver.shared.jitter.take().reliable;
    assert_eq!(sample.ingress.count, 3);
    assert_eq!(sample.baseline_resets, 2);
    assert_eq!(sample.ingress.max_us, 0);
    let forwarded = drain(&mut reader);
    assert!(forwarded.iter().any(|message| matches!(message,
        Message::TimedInput { time: CaptureTime(3_008_000), input: InputMsg::Forwarded { origin, .. } } if origin.device_id == DeviceId([1; 32]))));
}

#[test]
fn pair_requires_both_directions_and_all_five_opening_records() {
    let mut h = Harness::new();
    let (write, _motion) = h.inbound(1);
    let state = remote();
    for message in state.opening_state(true).into_iter().take(4) {
        write.post(encode(&message).unwrap()).unwrap();
    }
    h.poll();
    let (mut read, _datagrams) = h.outbound(1);
    assert!(!h.driver.adapter.attached(peer()));
    let initial = drain(&mut read);
    assert_eq!(initial.len(), 5);
    assert!(
        initial
            .iter()
            .enumerate()
            .all(|(index, message)| opening_at(index, message))
    );
    write.post(encode(&state.opening_state(true)[4]).unwrap()).unwrap();
    h.attach();
    assert!(h.driver.shared.status.load().peers[0].online);
}

#[test]
fn status_retains_the_peers_advertised_contacts_capacity() {
    for capacity in [None, Some(2), Some(5)] {
        let mut h = Harness::new();
        let (_read, _datagrams) = h.outbound(1);
        let (write, _motion) = h.inbound(1);
        let mut state = remote();
        state.router_mut().set_touchpad_contacts(capacity);
        opening(&write, &state, true);
        h.attach();
        assert_eq!(h.driver.shared.status.load().peers[0].touchpad_contacts, capacity);
    }
}

#[test]
fn outbound_reopen_retains_newest_inbound_state_and_waits_for_release() {
    let mut h = Harness::new();
    let (_old_read, _old_motion) = h.outbound(1);
    let (write, _motion) = h.inbound(1);
    let state = remote();
    opening(&write, &state, true);
    h.attach();
    h.driver.command(InputCommand::OutboundGone { peer: peer() });
    assert!(!h.driver.shared.status.load().peers[0].online);
    let old = h.driver.adapter.receive_state(peer()).unwrap().clone();
    let mut feel = old.feel.clone().unwrap();
    feel.key_repeat_delay_ms += 25;
    write
        .post(encode(&Message::Control(ControlMsg::Feel { profile: feel.clone() })).unwrap())
        .unwrap();
    h.driver.read();
    let (_read, _datagrams) = h.outbound(2);
    assert!(!h.driver.adapter.attached(peer()));
    assert!(h.driver.pairs[&peer()].releasing);
    h.attach();
    assert_eq!(h.driver.adapter.receive_state(peer()).unwrap().feel, Some(feel));
    assert_eq!(h.driver.pairs[&peer()].inbound.as_ref().unwrap().0.id().tag.ordinal, 1);
}

#[test]
fn inbound_replacement_does_not_replace_outbound_and_late_close_is_ignored() {
    let mut h = Harness::new();
    let (mut read, _datagrams) = h.outbound(1);
    let (first, _first_motion) = h.inbound(1);
    opening(&first, &remote(), true);
    h.attach();
    drain(&mut read);
    let old = h.driver.pairs[&peer()].inbound.as_ref().unwrap().0.id();
    let (second, _second_motion) = h.inbound(2);
    assert!(h.driver.adapter.receive_state(peer()).is_none());
    assert!(!h.driver.adapter.attached(peer()));
    h.driver.command(InputCommand::InboundGone {
        peer: peer(),
        stream: old,
    });
    opening(&second, &remote(), true);
    h.attach();
    assert_eq!(h.driver.pairs[&peer()].outbound.as_ref().unwrap().0.id().tag.ordinal, 1);
    let sent = drain(&mut read);
    assert!(
        sent.iter()
            .any(|m| matches!(m, Message::Control(ControlMsg::Feel { .. })))
    );
    assert!(sent.iter().any(|m| matches!(m, Message::Dnd(DndMsg::Caps { .. }))));
}

#[test]
fn pause_discards_both_streams_and_resume_requires_fresh_openings() {
    let mut h = Harness::new();
    let (_read, _datagrams) = h.outbound(1);
    let (write, _motion) = h.inbound(1);
    opening(&write, &remote(), true);
    h.attach();
    let instance = h.driver.adapter.routing_session();
    h.driver.command(InputCommand::Paused(true));
    h.poll();
    assert_ne!(instance, h.driver.adapter.routing_session());
    assert!(h.driver.pairs[&peer()].inbound.is_none());
    assert!(h.driver.pairs[&peer()].outbound.is_none());
    assert!(
        write
            .post(encode(&Message::Dnd(DndMsg::Caps { enabled: false })).unwrap())
            .is_err()
    );
    h.driver.command(InputCommand::Paused(false));
    h.poll();
    let (_new_read, _new_datagrams) = h.outbound(2);
    let (fresh, _) = h.inbound(2);
    opening(&fresh, &remote(), true);
    h.attach();
    assert_eq!(h.driver.pairs[&peer()].inbound.as_ref().unwrap().0.id().tag.ordinal, 2);
}

#[test]
fn invalid_opening_fault_identifies_only_the_exact_inbound_stream() {
    let mut h = Harness::new();
    let (_read, _datagrams) = h.outbound(1);
    let (write, _motion) = h.inbound(7);
    write
        .post(encode(&Message::Dnd(DndMsg::Caps { enabled: true })).unwrap())
        .unwrap();
    h.poll();
    assert!(h.driver.pairs[&peer()].inbound.is_none());
    assert_eq!(h.driver.pairs[&peer()].outbound.as_ref().unwrap().0.id().tag.ordinal, 1);
    assert!(
        matches!(h.driver.notices.take(), Some(InputNotice::Fault { which: Which::Inbound, code: StreamCode::Invalid, stream, .. }) if stream.tag.ordinal == 7)
    );
}

#[test]
fn forwarded_game_motion_uses_only_the_following_datagram_flow() {
    let mut h = Harness::new();
    let (mut read, datagrams) = h.outbound(1);
    drain(&mut read);
    let message = Message::Input(InputMsg::Forwarded {
        origin: fixture_boot(DeviceId([3; 32])),
        hops: 1,
        msg: Box::new(InputMsg::RelativeMotion {
            epoch: 4,
            generation: 2,
            seq: 5,
            x: 12.0,
            y: 3.0,
        }),
    });
    assert!(valid_datagram(&message));
    assert!(!valid_reliable(&message));
    h.driver.send(peer(), message.clone());
    assert_eq!(decode(&datagrams.take().unwrap().unwrap()).unwrap(), message);
    assert!(read.take().unwrap().is_none());
}

#[test]
fn new_focus_epoch_discards_passive_capture_even_when_holder_stays_local() {
    use oink_input::pointer_capture::CaptureSample;
    let mut detector = ScopedCaptureDetector::default();
    let me = DeviceId([1; 32]);
    let mut sample = CaptureSample {
        windows: None,
        hidden: true,
        application_hidden: Some(true),
        confined: false,
        position: oink_foundation::Point::new(400., 400.),
        motion_serial: 0,
        motion_observable: true,
        foreground: 1,
    };
    // Real movement in a stationary hidden cursor establishes passive lock.
    for now in (0..=700_000).step_by(50_000) {
        sample.motion_serial += 1;
        detector.update((326, me), Some(sample), true, true, now);
    }
    assert!(detector.update((326, me), Some(sample), true, true, 750_000));
    // Restart gossip advances the epoch without taking focus away locally.
    // Idle hidden state must not carry evidence from the old game scope.
    for now in (800_000..=1_500_000).step_by(50_000) {
        assert!(!detector.update((327, me), Some(sample), true, true, now));
    }
    // Fresh movement can establish capture again normally.
    for now in (1_550_000..=2_250_000).step_by(50_000) {
        sample.motion_serial += 1;
        detector.update((327, me), Some(sample), true, true, now);
    }
    assert!(detector.update((327, me), Some(sample), true, true, 2_300_000));
}

#[test]
fn real_send_queue_full_detaches_only_its_outbound_and_reuses_inbound_opening() {
    let mut h = Harness::new();
    let (_blocked_reader, _datagrams) = h.outbound(1);
    let (write, _motion) = h.inbound(1);
    let remote = remote();
    opening(&write, &remote, true);
    h.attach();
    let inbound = h.driver.pairs[&peer()].inbound.as_ref().unwrap().0.id();
    let state = h.driver.adapter.receive_state(peer()).unwrap().clone();
    let message = remote.opening_state(true)[2].clone();
    let mut count = 0;
    while h.driver.pairs[&peer()].outbound.is_some() && count < 100_000 {
        h.driver.send(peer(), message.clone());
        count += 1;
    }
    assert!(
        count < 100_000,
        "bounded stream must eventually reject the stopped consumer"
    );
    assert!(!h.driver.adapter.attached(peer()));
    assert_eq!(h.driver.pairs[&peer()].inbound.as_ref().unwrap().0.id(), inbound);
    assert_eq!(h.driver.adapter.receive_state(peer()).unwrap(), &state);
    assert!(!h.driver.shared.status.load().peers[0].online);
    let mut full = false;
    while let Some(notice) = h.driver.notices.take() {
        full |= matches!(notice, InputNotice::Fault { full: true, which: Which::Outbound, stream, .. } if stream.tag.ordinal == 1);
    }
    assert!(full);
    let (_replacement_reader, _replacement_motion) = h.outbound(2);
    h.attach();
    assert_eq!(h.driver.pairs[&peer()].inbound.as_ref().unwrap().0.id(), inbound);
}

#[test]
fn capability_change_does_not_rotate_routing_instance_or_close_inbound() {
    let mut h = Harness::new();
    let (_reader, _datagrams) = h.outbound(1);
    let (write, _motion) = h.inbound(1);
    opening(&write, &remote(), true);
    h.attach();
    let instance = h.driver.adapter.routing_session();
    let inbound = h.driver.pairs[&peer()].inbound.as_ref().unwrap().0.id();
    h.driver.command(InputCommand::Controllable(false));
    assert!(h.driver.adapter.router().paused());
    assert!(h.driver.adapter.attached(peer()));
    assert_eq!(h.driver.adapter.routing_session(), instance);
    assert_eq!(h.driver.pairs[&peer()].inbound.as_ref().unwrap().0.id(), inbound);
    assert!(matches!(
        h.driver.adapter.opening_state(false)[0],
        Message::Control(ControlMsg::InputCapability {
            controllable: false,
            touchpad_contacts: None
        })
    ));
    h.driver.command(InputCommand::Controllable(true));
    assert!(!h.driver.adapter.router().paused());
    assert_eq!(h.driver.adapter.routing_session(), instance);
}

#[test]
fn datagram_on_reliable_input_faults_its_exact_stream() {
    let mut h = Harness::new();
    let (_reader, _datagrams) = h.outbound(1);
    let (write, _motion) = h.inbound(8);
    opening(&write, &remote(), true);
    h.attach();
    write
        .post(
            encode(&Message::Input(InputMsg::RelativeMotion {
                epoch: 1,
                generation: 1,
                seq: 1,
                x: 1.,
                y: 2.,
            }))
            .unwrap(),
        )
        .unwrap();
    h.poll();
    assert!(h.driver.pairs[&peer()].inbound.is_none());
    assert!(h.driver.pairs[&peer()].outbound.is_some());
    let mut invalid = false;
    while let Some(notice) = h.driver.notices.take() {
        invalid |= matches!(notice, InputNotice::Fault { which: Which::Inbound, code: StreamCode::InvalidChannel, stream, .. } if stream.tag.ordinal == 8);
    }
    assert!(invalid);
}

#[test]
fn ordinary_command_flood_has_a_defined_nonblocking_outcome() {
    let mut h = Harness::new();
    // No native callback consumes the queue during this burst. Use the actual public
    // command API; the harness owns and drains the native owner at the end.
    let handle = std::mem::ManuallyDrop::new(InputThread {
        source_start: h.driver.inject.source_start(),
        #[cfg(debug_assertions)]
        source_motion_for_test: h.driver.inject.source_motion_counter_for_test(),
        shared: h.driver.shared.clone(),
        backend: h.driver.inject.backend(),
        inject_scheduler: h.driver.inject.scheduler_snapshot(),
        inject_timing: h.driver.inject.timing(),
        inject_failures: h.driver.inject.failure_counter(),
    });
    for _ in 0..10_000 {
        handle.send(InputCommand::FocusLock(true));
    }
    handle.send(InputCommand::FocusLock(false));
    assert_eq!(h.driver.shared.commands.queue_len(), 1);
    h.poll();
    assert!(!h.driver.adapter.router().locked());
    assert_eq!(h.driver.shared.commands.take_rejected(), 0);
}

#[test]
fn full_action_queue_rejects_and_counts_but_keeps_latest_state_and_shutdown() {
    let mut h = Harness::new();
    let handle = std::mem::ManuallyDrop::new(InputThread {
        source_start: h.driver.inject.source_start(),
        #[cfg(debug_assertions)]
        source_motion_for_test: h.driver.inject.source_motion_counter_for_test(),
        shared: h.driver.shared.clone(),
        backend: h.driver.inject.backend(),
        inject_scheduler: h.driver.inject.scheduler_snapshot(),
        inject_timing: h.driver.inject.timing(),
        inject_failures: h.driver.inject.failure_counter(),
    });
    for _ in 0..1031 {
        handle.send(InputCommand::ReturnPointerHome);
    }
    handle.send(InputCommand::FocusLock(true));
    handle.send(InputCommand::Paused(false));
    handle.send(InputCommand::Paused(true));
    assert_eq!(h.driver.shared.commands.queue_len(), 1025);
    assert_eq!(h.driver.shared.commands.take_rejected(), 7);
    assert_eq!(h.driver.shared.commands.take_rejected(), 0);
    h.poll();
    assert!(!h.driver.adapter.router().locked()); // Pausing visibly clears the lock.
    assert!(h.driver.paused);
    for _ in 0..1024 {
        handle.send(InputCommand::ReturnPointerHome);
    }
    handle.send(InputCommand::Stop);
    h.poll();
    assert!(h.driver.stopping);
}

#[test]
fn stalled_main_cancellation_must_precede_native_left_up() {
    use oink_foundation::input::MouseButton;
    let backend = Arc::new(oink_test_support::input::FakeInput::new());
    let (completed, completion) = mpsc::channel();
    let inject = super::super::super::inject::start_with_backend(
        backend.clone(),
        Box::new(move |_| {
            completed.send(()).unwrap();
        }),
    )
    .unwrap();
    let (main, stalled_main) = crate::threads::main_channel();
    // These are the actual queue items emitted by Driver::outputs.
    inject.push(InjectItem::Dnd {
        command: oink_routing::DndCmd::Cancel {
            id: oink_foundation::content::ClipId { origin: peer(), seq: 1 },
            epoch: 1,
            physical: false,
        },
        main,
    });
    inject.push(InjectItem::Inject {
        event: InjectEvent::Button {
            button: MouseButton::Left,
            pressed: false,
        },
        from: Some(peer()),
    });
    inject.push(InjectItem::ReleaseAll { peer: peer() });
    let command = stalled_main.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(matches!(
        command,
        MainCommand::Dnd {
            command: oink_routing::DndCmd::Cancel { .. },
            ..
        }
    ));
    assert!(completion.recv_timeout(Duration::from_millis(50)).is_err());
    assert!(backend.take_injected().is_empty(), "Up overtook native cancellation");
    // The worker finishing (or rejecting) the native command releases the fence.
    drop(command);
    completion.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(
        backend.take_injected(),
        vec![InjectEvent::Button {
            button: MouseButton::Left,
            pressed: false,
        }]
    );
}

#[test]
fn blocked_fake_injector_leaves_i0_queued_while_the_input_owner_keeps_running() {
    use oink_foundation::input::{CaptureMode, FeelProfile};
    use oink_input::{EventHandler, InputBackend, InputError, InputPermissions};
    struct Blocked {
        fake: oink_test_support::input::FakeInput,
        release: std::sync::Mutex<Option<mpsc::Receiver<()>>>,
        entered: mpsc::Sender<()>,
    }
    impl InputBackend for Blocked {
        fn start(&self, handler: EventHandler) -> Result<(), InputError> {
            self.fake.start(handler)
        }
        fn stop(&self) {
            self.fake.stop();
        }
        fn set_mode(&self, mode: CaptureMode) -> Result<(), InputError> {
            self.fake.set_mode(mode)
        }
        fn inject(&self, event: &InjectEvent) -> Result<(), InputError> {
            if let Some(release) = self.release.lock().unwrap().take() {
                self.entered.send(()).unwrap();
                // Bounded even if an assertion unwinds before the release signal.
                let _ = release.recv_timeout(Duration::from_secs(2));
            }
            self.fake.inject(event)
        }
        fn cursor_position(&self) -> Result<oink_foundation::Point, InputError> {
            self.fake.cursor_position()
        }
        fn feel_profile(&self) -> FeelProfile {
            self.fake.feel_profile()
        }
        fn permissions(&self) -> InputPermissions {
            self.fake.permissions()
        }
    }
    let (release, blocked) = mpsc::channel();
    let (entered, entered_rx) = mpsc::channel();
    let backend = Arc::new(Blocked {
        fake: oink_test_support::input::FakeInput::new(),
        release: std::sync::Mutex::new(Some(blocked)),
        entered,
    });
    let mut h = Harness::with_backend(backend);
    let (_reader, _datagrams) = h.outbound(1);
    let (writer, _motion) = h.inbound(1);
    opening(&writer, &remote(), true);
    h.attach();
    h.driver.inject.push(InjectItem::Inject {
        event: InjectEvent::MoveRelative { dx: 0., dy: 0. },
        from: Some(peer()),
    });
    entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    while h.driver.inject.available() >= 4096 {
        h.driver.inject.push(InjectItem::SetMode(CaptureMode::Local));
    }
    let previous = h.driver.adapter.receive_state(peer()).unwrap().feel.clone().unwrap();
    let mut changed = previous.clone();
    changed.key_repeat_delay_ms += 99;
    writer
        .post(
            encode(&Message::Control(ControlMsg::Feel {
                profile: changed.clone(),
            }))
            .unwrap(),
        )
        .unwrap();
    let started = Instant::now();
    h.poll();
    assert!(
        started.elapsed() < Duration::from_millis(100),
        "input owner waited for the blocked injector"
    );
    assert_eq!(h.driver.adapter.receive_state(peer()).unwrap().feel, Some(previous));
    assert!(h.driver.adapter.attached(peer()));
    release.send(()).unwrap();
    let until = Instant::now() + Duration::from_secs(2);
    while h.driver.adapter.receive_state(peer()).unwrap().feel != Some(changed.clone()) && Instant::now() < until {
        h.poll();
        std::thread::yield_now();
    }
    assert_eq!(h.driver.adapter.receive_state(peer()).unwrap().feel, Some(changed));
}

#[test]
fn emergency_clears_online_and_releases_holds_before_reporting_its_barrier() {
    use oink_foundation::input::KeyCode;
    let backend = Arc::new(oink_test_support::input::FakeInput::new());
    let mut h = Harness::with_backend(backend.clone());
    let (_reader, _datagrams) = h.outbound(1);
    let (writer, _motion) = h.inbound(1);
    opening(&writer, &remote(), true);
    h.attach();
    h.driver.message(
        peer(),
        Message::Control(ControlMsg::FocusEnter {
            epoch: 2,
            display: "screen".into(),
            pos: oink_foundation::Point::new(5., 5.),
            scope: Some(input_scope(&h.driver)),
            pointer_source: Some(peer()),
        }),
    );
    h.driver.message(
        peer(),
        Message::Input(InputMsg::Key {
            code: KeyCode::keyboard(4),
            pressed: true,
        }),
    );
    let until = Instant::now() + Duration::from_secs(2);
    let mut down = false;
    while !down && Instant::now() < until {
        down = backend
            .take_injected()
            .iter()
            .any(|event| matches!(event, InjectEvent::Key { pressed: true, .. }));
        std::thread::yield_now();
    }
    assert!(down);
    let instance = h.driver.adapter.routing_session();
    h.driver.adapter.trigger_emergency();
    h.driver.outputs(None);
    assert!(!h.driver.shared.status.load().peers[0].online);
    assert!(!h.driver.adapter.attached(peer()));
    assert!(h.driver.adapter.fully_paused());
    assert_ne!(h.driver.adapter.routing_session(), instance);
    assert!(h.driver.pairs[&peer()].outbound.is_none());
    assert!(h.driver.pairs[&peer()].inbound.is_some());
    let until = Instant::now() + Duration::from_secs(2);
    let mut released = false;
    while !released && Instant::now() < until {
        released = h.driver.inject.take_released() == Some(peer());
        std::thread::yield_now();
    }
    assert!(released);
    assert_eq!(
        backend
            .take_injected()
            .iter()
            .filter(|event| matches!(event, InjectEvent::Key { pressed: false, .. }))
            .count(),
        1
    );
    let mut emergency = false;
    while let Some(notice) = h.driver.notices.take() {
        emergency |= notice == InputNotice::EmergencyPause;
    }
    assert!(emergency);
}

#[test]
fn unrelated_environment_refresh_does_not_erase_scoped_capture_detection() {
    let mut h = Harness::new();
    h.driver.adapter.set_pointer_captured(true);
    assert!(h.driver.adapter.game_mode().detected);
    h.driver.snapshots.environment.publish(Environment {
        secure_input: true,
        ..Default::default()
    });
    h.driver.refresh();
    assert!(h.driver.adapter.game_mode().detected);
}

#[test]
fn simultaneous_reopens_converge_in_each_delivery_order_without_extra_reopen() {
    // O = the local writer fails, N = its replacement, I = the peer replacement.
    // N must follow O; I can precede, interleave, or follow those two notices.
    for order in [[0, 1, 2], [0, 2, 1], [2, 0, 1]] {
        let mut h = Harness::new();
        let (old_read, old_motion) = h.outbound(1);
        let (old_write, old_datagrams) = h.inbound(1);
        let state = remote();
        opening(&old_write, &state, true);
        h.attach();
        let mut reader = Some((old_read, old_motion));
        let mut writer = Some((old_write, old_datagrams));
        for operation in order {
            match operation {
                0 => h.driver.fault(peer(), Which::Outbound, true, StreamCode::None),
                1 => reader = Some(h.outbound(2)),
                2 => {
                    writer = Some(h.inbound(2));
                    opening(&writer.as_ref().unwrap().0, &state, true);
                }
                _ => unreachable!(),
            }
            h.poll();
        }
        h.attach();
        assert_eq!(h.driver.pairs[&peer()].outbound.as_ref().unwrap().0.id().tag.ordinal, 2);
        assert_eq!(h.driver.pairs[&peer()].inbound.as_ref().unwrap().0.id().tag.ordinal, 2);
        assert!(h.driver.shared.status.load().peers[0].online);
        let faults: Vec<_> = std::iter::from_fn(|| h.driver.notices.take())
            .filter(|notice| matches!(notice, InputNotice::Fault { .. }))
            .collect();
        assert_eq!(faults.len(), 1, "delivery order {order:?}");
        assert!(matches!(
            faults[0],
            InputNotice::Fault {
                which: Which::Outbound,
                full: true,
                ..
            }
        ));
        assert!(reader.is_some() && writer.is_some());
    }
}

#[test]
fn paused_inbound_actions_cannot_reuse_previous_session() {
    use oink_foundation::{Point, input::KeyCode};
    let backend = Arc::new(oink_test_support::input::FakeInput::new());
    let mut h = Harness::with_backend(backend.clone());
    let (_reader, _motion) = h.outbound(1);
    let (writer, _datagrams) = h.inbound(1);
    opening(&writer, &remote(), true);
    h.attach();
    let old_scope = input_scope(&h.driver);
    h.driver.command(InputCommand::Paused(true));
    h.poll();
    let until = Instant::now() + Duration::from_secs(2);
    while h.driver.pairs[&peer()].releasing && Instant::now() < until {
        h.poll();
        std::thread::yield_now();
    }
    assert!(!h.driver.pairs[&peer()].releasing);
    backend.take_injected();
    let input = [
        Message::Control(ControlMsg::FocusEnter {
            epoch: 200,
            display: "screen".into(),
            pos: Point::new(5., 5.),
            scope: Some(old_scope.clone()),
            pointer_source: Some(peer()),
        }),
        Message::Input(InputMsg::Key {
            code: KeyCode::keyboard(4),
            pressed: true,
        }),
        Message::Input(InputMsg::Key {
            code: KeyCode::keyboard(4),
            pressed: false,
        }),
        Message::Dnd(DndMsg::Caps { enabled: false }),
    ];
    for message in input {
        assert!(writer.post(encode(&message).unwrap()).is_err());
    }
    while h.driver.read() {}
    assert!(backend.take_injected().is_empty());
    assert_eq!(h.driver.adapter.focus().holder, h.driver.local);
    assert_ne!(h.driver.adapter.focus().epoch, 200);
    assert!(h.driver.pairs[&peer()].inbound.is_none());
    assert!(h.driver.pairs[&peer()].outbound.is_none());
}

#[test]
fn delayed_ordinary_drag_work_cannot_cross_an_input_stream_replacement() {
    let mut h = Harness::new();
    let (mut old_reader, _old_motion) = h.outbound(1);
    let (old_writer, _old_datagrams) = h.inbound(1);
    opening(&old_writer, &remote(), true);
    h.attach();
    drain(&mut old_reader);
    let status = h.driver.shared.status.load();
    let old_inbound = status.peers[0].inbound.unwrap();
    let old_outbound = status.peers[0].outbound.unwrap();
    let (mut new_reader, _new_motion) = h.outbound(2);
    let (new_writer, _new_datagrams) = h.inbound(2);
    opening(&new_writer, &remote(), true);
    h.attach();
    drain(&mut new_reader);
    let original = h.driver.adapter.receive_state(peer()).unwrap().feel.clone().unwrap();
    let mut changed = original.clone();
    changed.key_repeat_delay_ms += 42;
    let message = Message::Control(ControlMsg::Feel {
        profile: changed.clone(),
    });
    h.driver.command(InputCommand::PeerMessage {
        peer: peer(),
        stream: old_inbound,
        message: message.clone(),
    });
    h.driver.command(InputCommand::SendMessage {
        peer: peer(),
        stream: old_outbound,
        message: oink_protocol::v1::frame::OpaqueFrame::parse_exact(
            &encode(&message).unwrap(),
            oink_protocol::v1::limits::RELIABLE_FRAME,
        )
        .unwrap()
        .decode()
        .unwrap(),
    });
    assert_eq!(h.driver.adapter.receive_state(peer()).unwrap().feel, Some(original));
    assert!(drain(&mut new_reader).is_empty());
    let status = h.driver.shared.status.load();
    h.driver.command(InputCommand::PeerMessage {
        peer: peer(),
        stream: status.peers[0].inbound.unwrap(),
        message: message.clone(),
    });
    h.driver.command(InputCommand::SendMessage {
        peer: peer(),
        stream: status.peers[0].outbound.unwrap(),
        message: oink_protocol::v1::frame::OpaqueFrame::parse_exact(
            &encode(&message).unwrap(),
            oink_protocol::v1::limits::RELIABLE_FRAME,
        )
        .unwrap()
        .decode()
        .unwrap(),
    });
    assert_eq!(h.driver.adapter.receive_state(peer()).unwrap().feel, Some(changed));
    assert!(drain(&mut new_reader).contains(&message));
}

mod capture_modes;
mod network;

#[test]
fn review_codex_r1_stalled_main_bounds_pointer_updates() {
    use oink_foundation::{
        Point, Rect,
        display::DisplayInfo,
        input::{InputEvent, SourceKind},
    };
    let mut h = Harness::new();
    h.driver.command(InputCommand::LocalDisplays(vec![DisplayInfo {
        key: "screen".into(),
        name: "screen".into(),
        bounds: Rect::new(0., 0., 1000., 1000.),
        pixel_width: 1000,
        pixel_height: 1000,
        reference_extent: None,
        rotation_deg: 0,
        is_primary: true,
        is_builtin: false,
        is_virtual: false,
    }]));
    h.driver.adapter.router_mut().set_cursor(Point::new(500., 500.));
    // Simulate a native callback while the ordinary command worker is stalled.
    // The stream input queues stay empty: this is the reverse handoff from the
    // realtime owner, distinct from the recorded ordinary-command overflow gap.
    h._main.try_iter().for_each(drop);
    for _ in 0..32_768 {
        h.driver.event(&CapturedEvent {
            physical: true,
            event: InputEvent::PointerDelta { dx: 1., dy: 0. },
            source: SourceKind::Mouse,
            time_us: 0,
            cursor: Some(Point::new(500., 500.)),
        });
    }
    let pending = h
        ._main
        .try_iter()
        .filter(|command| matches!(command, MainCommand::TrailPointer { .. }))
        .count();
    // Deliberately generous: even the native injection queue holds only 8192.
    // Latest pointer state should have a fixed bound under consumer backpressure.
    assert!(
        pending <= 8192,
        "stalled main accumulated {pending} pointer commands from 32768 callbacks"
    );
}

#[test]
fn config_geometry_and_input_pair_availability_commute() {
    use oink_foundation::{
        Point, Rect, Size,
        display::DisplayInfo,
        input::{InputEvent, SourceKind},
    };
    let local = DeviceId([1; 32]);
    let display = DisplayInfo {
        key: "screen".into(),
        name: "screen".into(),
        bounds: Rect::new(0., 0., 1000., 1000.),
        pixel_width: 1000,
        pixel_height: 1000,
        reference_extent: Some(Size::new(300., 300.)),
        rotation_deg: 0,
        is_primary: true,
        is_builtin: false,
        is_virtual: false,
    };
    let layout = Arc::new(oink_display::layout::Layout::auto(&[
        (local, vec![display.clone()]),
        (peer(), vec![display.clone()]),
    ]));
    for config_first in [false, true] {
        let mut h = Harness::new();
        // This scenario explicitly starts without any adopted geometry.
        let empty = Arc::new(oink_display::layout::Layout::default());
        h.driver.snapshots.layout.publish(RouterLayout {
            group: oink_foundation::GroupId([0; 16]),
            owners: Arc::default(),
            version: fixture_version(),
            topology: oink_routing::TopologyLayout::from_layout(&empty, &Default::default()),
            layout: empty,
            devices: Default::default(),
        });
        h.driver.refresh();
        h.driver.command(InputCommand::LocalDisplays(vec![display.clone()]));
        let publish = |h: &mut Harness, enabled: bool| {
            let devices = if enabled {
                BTreeSet::from([local, peer()])
            } else {
                BTreeSet::from([local])
            };
            h.driver.snapshots.layout.publish(RouterLayout {
                group: oink_foundation::GroupId([0; 16]),
                owners: Arc::default(),
                version: fixture_version(),
                topology: oink_routing::TopologyLayout::new(
                    layout.topology_online(&Default::default(), |d| devices.contains(d)),
                ),
                layout: layout.clone(),
                devices,
            });
            h.driver.refresh();
        };
        let cross = |h: &mut Harness| {
            h.driver.adapter.router_mut().set_cursor(Point::new(995., 500.));
            for _ in 0..20 {
                h.driver.event(&CapturedEvent {
                    physical: true,
                    event: InputEvent::PointerDelta { dx: 100., dy: 0. },
                    source: SourceKind::Mouse,
                    time_us: 0,
                    cursor: Some(Point::new(999., 500.)),
                });
                if h.driver.adapter.router().focus().holder == peer() {
                    break;
                }
            }
        };
        if config_first {
            publish(&mut h, true);
            cross(&mut h);
            assert_eq!(
                h.driver.adapter.router().focus().holder,
                local,
                "config alone must not grant input"
            );
        }
        let (mut read, _motion) = h.outbound(1);
        let (write, _datagrams) = h.inbound(1);
        opening(&write, &remote(), true);
        h.attach();
        if !config_first {
            cross(&mut h);
            assert_eq!(
                h.driver.adapter.router().focus().holder,
                local,
                "input alone must not create geometry"
            );
            publish(&mut h, true);
        }
        assert!(h.driver.online.contains(&peer()));
        drain(&mut read);
        cross(&mut h);
        assert_eq!(h.driver.adapter.router().focus().holder, peer(), "order={config_first}");
        publish(&mut h, false);
        assert_eq!(h.driver.adapter.router().focus().holder, local);
        assert!(!h.driver.online.contains(&peer()));
        publish(&mut h, true);
        assert!(h.driver.online.contains(&peer()));
        h.driver.command(InputCommand::OutboundGone { peer: peer() });
        assert!(!h.driver.online.contains(&peer()));
        // A config republish cannot make the detached input pair available again.
        publish(&mut h, true);
        assert!(!h.driver.online.contains(&peer()));
    }
}

mod drag;

mod lifecycle;

mod config;

mod review;

mod motion;

mod capture;

#[test]
fn trail_duration_snapshot_updates_capture_probe_and_native_renderer_command() {
    let mut h = Harness::new();
    assert!(
        h._main
            .try_iter()
            .any(|cmd| matches!(cmd, MainCommand::TrailDuration(270)))
    );
    let mut value = (*h.driver.settings).clone();
    value.cursor_trail = false;
    value.cursor_trail_duration_ms = 10_000;
    h.driver.snapshots.settings.publish(value);
    h.driver.refresh();
    assert!(
        h._main
            .try_iter()
            .any(|cmd| matches!(cmd, MainCommand::TrailDuration(10_000)))
    );
}

#[test]
fn contacts_snapshot_disables_receiver_before_processing_more_input() {
    use oink_foundation::contacts::{Contact, Frame, Phase, Sample, Surface};
    let fake = Arc::new(oink_test_support::input::FakeInput::new());
    let mut h = Harness::with_local(peer(), fake.clone());
    let (_, target) = capture::pair_target(Os::Windows);
    h.driver.adapter = Adapter::new(target);
    let origin = DeviceId([1; 32]);
    let r = h.driver.adapter.router_mut();
    r.set_touchpad_contacts(Some(5));
    let mut sample = Sample {
        device: 1,
        time_us: 1000,
        len: 3,
        surface: Surface {
            width: 15600,
            height: 9600,
        },
        ..Default::default()
    };
    for (i, point) in sample.contacts.iter_mut().take(3).enumerate() {
        *point = Contact {
            id: i as u32 + 1,
            x: 1000 + i as i32 * 1000,
            y: 1000,
        };
    }
    let begin = Message::Input(InputMsg::TouchpadContacts(Frame::between(None, &sample)));
    r.input_message(origin, begin.clone(), 0);
    h.driver.outputs(Some(origin));
    let until = Instant::now() + Duration::from_secs(2);
    while !fake
        .injected()
        .iter()
        .any(|e| matches!(e, InjectEvent::TouchpadContacts(f) if f.phase == Phase::Begin))
    {
        assert!(Instant::now() < until, "initial contacts were not injected");
        std::thread::yield_now();
    }
    let mut settings = (*h.driver.snapshots.settings.load()).clone();
    settings.accept_contacts = false;
    h.driver.snapshots.settings.publish(settings);
    h.poll();
    let until = Instant::now() + Duration::from_secs(2);
    loop {
        let count = fake
            .injected()
            .iter()
            .filter(|e| matches!(e, InjectEvent::TouchpadContacts(f) if f.phase == Phase::Cancel))
            .count();
        if count == 1 {
            break;
        }
        assert!(Instant::now() < until, "setting snapshot did not release contacts");
        std::thread::yield_now();
    }
    let r = h.driver.adapter.router_mut();
    r.set_touchpad_contacts(Some(5)); // Preference does not withdraw native capability.
    assert!(matches!(
        &r.input_opening(true)[0],
        Message::Control(ControlMsg::InputCapability {
            touchpad_contacts: Some(5),
            ..
        })
    ));
    r.input_message(origin, begin, 1001);
    assert!(
        !r.drain_outputs()
            .iter()
            .any(|o| matches!(o, Output::Inject(InjectEvent::TouchpadContacts(_))))
    );
}

#[test]
fn input_status_change_notifies_ui_owner_before_any_timer_tick() {
    let mut h = Harness::new();
    h.driver.publish();
    while h._main.try_recv().is_ok() {}
    let now = h.driver.now;
    h.driver.command(InputCommand::FocusLock(true));
    h.driver.publish();
    assert_eq!(h.driver.now, now);
    assert!(h.driver.shared.status.load().locked_device.is_some());
    assert!(std::iter::from_fn(|| h._main.try_recv().ok()).any(|c| matches!(c, MainCommand::InputStatusChanged)));
    h.driver.publish();
    assert!(h._main.try_recv().is_err(), "unchanged input state must not notify UI");
}

#[test]
fn keyboard_guard_releases_old_keys_when_active_and_detaches_during_full_pause() {
    // Memory backend only: no native injection or desktop operation.
    let backend = Arc::new(oink_test_support::input::FakeInput::new());
    let a = oink_foundation::input::KeyCode::keyboard(4);
    backend.set_held_session_keys(vec![a, oink_foundation::input::KeyCode::LEFT_SHIFT]);
    let mut h = Harness::with_backend(backend.clone());
    h.driver.command(InputCommand::Paused(false));
    h.driver.command(InputCommand::SessionLocked(Some(false)));
    backend.take_injected();
    h.driver.command(InputCommand::KeyboardDisabled(true));
    assert!(h.driver.capture_requested());
    loop {
        let events = backend.injected();
        if [a, oink_foundation::input::KeyCode::LEFT_SHIFT].iter().all(|code| {
            events.contains(&InjectEvent::Key {
                code: *code,
                pressed: false,
            })
        }) {
            break;
        }
        std::thread::yield_now();
    }
    assert!(!h.driver.adapter.router().keyboard_pointer_modifiers().is_empty());
    // Snapshots cannot arm the physical release chord or overwrite a later command.
    assert!(h.driver.adapter.router().next_deadline().is_none());
    h.driver.command(InputCommand::SessionLocked(Some(true)));
    assert!(h.driver.adapter.router().keyboard_disabled());
    assert_eq!(
        h.driver.keyboard_guard_suspended(),
        h.driver.adapter.router().config().os == Os::Windows
    );
    h.driver.command(InputCommand::SessionLocked(Some(false)));
    assert!(h.driver.adapter.router().keyboard_disabled());
    assert!(!h.driver.keyboard_guard_suspended());
    h.driver.command(InputCommand::Paused(true));
    assert!(!h.driver.capture_requested());
}

#[test]
fn local_modes_publish_immediately_with_secure_input_and_keep_native_routes_independent() {
    use oink_foundation::input::InputMode;
    let mut h = Harness::new();
    let mut settings = (*h.driver.snapshots.settings.load()).clone();
    settings.mouse_mode = InputMode::Local;
    settings.keyboard_mode = InputMode::Local;
    h.driver.snapshots.settings.publish(settings);
    h.driver.snapshots.environment.publish(Environment {
        secure_input: true,
        ..Default::default()
    });
    h.poll();
    let status = h.driver.shared.status.load();
    assert_eq!(status.mouse_mode, InputMode::Local);
    assert_eq!(status.keyboard_mode, InputMode::Local);
    assert!(!h.driver.relative_capture);
    assert!(!h.driver.keyboard_remote);
    assert!(h.driver.environment.secure_input);
}

#[test]
fn manual_update_interrupts_held_input_locked_and_secure_session() {
    use oink_foundation::input::{InputEvent, KeyCode};
    let mut h = Harness::new();
    h.driver.command(InputCommand::Paused(false));
    h.driver.event(&CapturedEvent {
        event: InputEvent::Key {
            code: KeyCode(4),
            pressed: true,
        },
        physical: true,
        source: oink_foundation::input::SourceKind::Keyboard,
        time_us: 0,
        cursor: None,
    });
    h.driver.command(InputCommand::SessionLocked(Some(true)));
    h.driver.snapshots.environment.publish(Environment {
        secure_input: true,
        ..Default::default()
    });
    h.driver.refresh();
    let (reply, wait) = mpsc::sync_channel(1);
    h.driver.command(InputCommand::PrepareUpdate { generation: 1, reply });
    let fence = wait.recv().unwrap().unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while fence.load(Ordering::Acquire) == 0 && Instant::now() < deadline {
        std::thread::yield_now();
    }
    assert_eq!(fence.load(Ordering::Acquire), 1);
    assert!(h.driver.adapter.router().fully_paused());
    assert!(!h.driver.adapter.router().update_input_held());
    assert_eq!(h.driver.adapter.router().focus().holder, h.driver.local);
    h.driver.command(InputCommand::AbortUpdate { generation: 1 });
    assert!(h.driver.update_hold.is_none());
}

#[test]
fn manual_update_interrupts_remote_control_and_fences_native_key_release() {
    use oink_foundation::input::KeyCode;
    let backend = Arc::new(oink_test_support::input::FakeInput::new());
    let mut h = Harness::with_backend(backend.clone());
    let (_reader, _datagrams) = h.outbound(1);
    let (writer, _motion) = h.inbound(1);
    opening(&writer, &remote(), true);
    h.attach();
    h.driver.message(
        peer(),
        Message::Control(ControlMsg::FocusEnter {
            epoch: 2,
            display: "screen".into(),
            pos: oink_foundation::Point::new(5., 5.),
            scope: Some(input_scope(&h.driver)),
            pointer_source: Some(peer()),
        }),
    );
    h.driver.message(
        peer(),
        Message::Input(InputMsg::Key {
            code: KeyCode::keyboard(4),
            pressed: true,
        }),
    );
    let until = Instant::now() + Duration::from_secs(2);
    let mut down = false;
    while !down && Instant::now() < until {
        down = backend
            .take_injected()
            .iter()
            .any(|event| matches!(event, InjectEvent::Key { pressed: true, .. }));
        std::thread::yield_now();
    }
    assert!(down);
    assert!(h.driver.adapter.router().update_remote_control(h.driver.micros()));
    let (reply, wait) = mpsc::sync_channel(1);
    h.driver.command(InputCommand::PrepareUpdate { generation: 19, reply });
    let fence = wait.recv().unwrap().unwrap();
    let until = Instant::now() + Duration::from_secs(2);
    while fence.load(Ordering::Acquire) == 0 && Instant::now() < until {
        std::thread::yield_now();
    }
    assert_eq!(fence.load(Ordering::Acquire), 1);
    assert!(
        backend
            .take_injected()
            .iter()
            .any(|event| matches!(event, InjectEvent::Key { pressed: false, .. }))
    );
    assert_eq!(h.driver.adapter.router().focus().holder, h.driver.local);
    assert!(!h.driver.adapter.router().update_input_held());
    h.driver.command(InputCommand::AbortUpdate { generation: 19 });
}

#[test]
fn manual_update_interrupts_pending_native_drag_and_keeps_injection_dormant_until_abort() {
    let backend = Arc::new(oink_test_support::input::FakeInput::new());
    let mut h = Harness::with_backend(backend.clone());
    h.driver.command(InputCommand::Paused(false));
    h.driver.inject.push(InjectItem::Dnd {
        command: oink_routing::DndCmd::Cancel {
            id: oink_foundation::content::ClipId { origin: peer(), seq: 1 },
            epoch: 1,
            physical: false,
        },
        main: h.driver.main.clone(),
    });
    let pending = loop {
        let command = h._main.recv_timeout(Duration::from_secs(2)).unwrap();
        if matches!(command, MainCommand::Dnd { .. }) {
            break command;
        }
    };
    let (reply, wait) = mpsc::sync_channel(1);
    h.driver.command(InputCommand::PrepareUpdate { generation: 23, reply });
    let fence = wait.recv().unwrap().unwrap();
    let until = Instant::now() + Duration::from_secs(2);
    while fence.load(Ordering::Acquire) == 0 && Instant::now() < until {
        std::thread::yield_now();
    }
    assert_eq!(
        fence.load(Ordering::Acquire),
        1,
        "an interrupted drag cannot hold the install fence"
    );
    h.poll();
    assert!(h.driver.inject.suspended());
    assert!(!backend.injection_active());
    // Even completion of old ordinary work cannot restart the old session.
    drop(pending);
    h.driver.command(InputCommand::AbortUpdate { generation: 23 });
    let until = Instant::now() + Duration::from_secs(2);
    while h.driver.inject.suspended() && Instant::now() < until {
        h.poll();
        std::thread::yield_now();
    }
    assert!(!h.driver.inject.suspended());
}

#[test]
fn update_prepare_waits_for_injection_fence_and_abort_restores_admission() {
    let mut h = Harness::new();
    h.driver.command(InputCommand::SessionLocked(Some(false)));
    h.driver.command(InputCommand::Paused(false));
    let (reply, wait) = mpsc::sync_channel(1);
    h.driver.command(InputCommand::PrepareUpdate { generation: 9, reply });
    let fence = wait.recv().unwrap().unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while fence.load(Ordering::Acquire) == 0 && Instant::now() < deadline {
        std::thread::yield_now();
    }
    assert_eq!(fence.load(Ordering::Acquire), 1);
    assert!(h.driver.adapter.router().fully_paused());
    h.driver.command(InputCommand::AbortUpdate { generation: 8 });
    assert!(h.driver.update_hold.is_some());
    h.driver.command(InputCommand::AbortUpdate { generation: 9 });
    assert!(h.driver.update_hold.is_none());
    assert!(!h.driver.adapter.router().fully_paused());
}

#[test]
fn screen_identification_uses_attached_input_stream_and_obeys_pause() {
    let mut h = Harness::new();
    let (mut read, _) = h.outbound(1);
    drain(&mut read);
    h.driver.command(InputCommand::IdentifyScreens(11));
    assert!(drain(&mut read).is_empty(), "unattached peers must not receive actions");
    let (write, _) = h.inbound(1);
    opening(&write, &remote(), true);
    h.attach();
    drain(&mut read);
    h.driver.command(InputCommand::IdentifyScreens(12));
    assert!(
        drain(&mut read)
            .iter()
            .any(|message| matches!(message, Message::Control(ControlMsg::IdentifyScreens { id: 12 })))
    );
    write
        .post(encode(&Message::Control(ControlMsg::IdentifyScreens { id: 13 })).unwrap())
        .unwrap();
    h.poll();
    assert!(
        h._main
            .try_iter()
            .any(|command| matches!(command, MainCommand::IdentifyScreens(13)))
    );
    h.driver.command(InputCommand::Paused(true));
    h.driver.command(InputCommand::IdentifyScreens(14));
    assert!(
        !drain(&mut read)
            .iter()
            .any(|message| matches!(message, Message::Control(ControlMsg::IdentifyScreens { .. })))
    );
    write
        .post(encode(&Message::Control(ControlMsg::IdentifyScreens { id: 15 })).unwrap())
        .unwrap();
    h.poll();
    assert!(
        !h._main
            .try_iter()
            .any(|command| matches!(command, MainCommand::IdentifyScreens(_)))
    );
}

#[test]
fn resource_faults_keep_unknown_and_separate_capture_secure_input_and_cleanup() {
    use oink_foundation::input::KeyCode;
    use oink_protocol::v1::dto::ResourceFaultCode as Fault;
    let mut h = Harness::new();
    h.driver.publish();
    assert_eq!(
        h.driver.shared.status.load().resource_faults,
        super::super::ResourceFaults::default()
    );
    h.driver.capture_available(false);
    let faults = h.driver.shared.status.load().resource_faults;
    assert_eq!(faults.mouse, Some(Fault::CaptureUnavailable));
    assert_eq!(faults.keyboard, Some(Fault::CaptureUnavailable));
    assert_eq!(faults.screen, None);
    let displays = vec![oink_foundation::display::DisplayInfo {
        key: "screen".into(),
        name: "screen".into(),
        bounds: oink_foundation::Rect::new(0., 0., 1000., 1000.),
        pixel_width: 1000,
        pixel_height: 1000,
        reference_extent: Some(oink_foundation::Size::new(1., 1.)),
        rotation_deg: 0,
        is_primary: true,
        is_builtin: false,
        is_virtual: false,
    }];
    for (observed, expected) in [
        (displays.clone(), Fault::None),
        (vec![], Fault::DeviceUnavailable),
        (displays.clone(), Fault::None),
    ] {
        h.driver.command(InputCommand::LocalDisplays(observed));
        h.driver.publish();
        assert_eq!(h.driver.shared.status.load().resource_faults.screen, Some(expected));
    }
    h.driver.capture_available(true);
    h.driver.snapshots.environment.publish(Environment {
        secure_input: true,
        ..Default::default()
    });
    h.poll();
    let faults = h.driver.shared.status.load().resource_faults;
    assert_eq!(faults.mouse, Some(Fault::None));
    assert_eq!(faults.keyboard, Some(Fault::SecureInput));
    h.driver.inject.push(InjectItem::Inject {
        event: InjectEvent::Key {
            code: KeyCode::keyboard(4),
            pressed: true,
        },
        from: Some(peer()),
    });
    let (reply, result) = mpsc::sync_channel(1);
    h.driver.command(InputCommand::ApplyOwnerPause {
        paused: true,
        expected_emergency_sequence: 0,
        reply,
    });
    let until = Instant::now() + Duration::from_secs(2);
    loop {
        h.poll();
        if let Ok(result) = result.try_recv() {
            assert!(!result.unwrap().cleanup_pending);
            h.driver.command(InputCommand::LocalDisplays(displays.clone()));
            h.driver.publish();
            assert_eq!(h.driver.shared.status.load().resource_faults.screen, Some(Fault::None));
            break;
        }
        assert!(Instant::now() < until);
        std::thread::yield_now();
    }
    for (event, pending) in [
        (InjectEvent::MoveRelative { dx: 1., dy: 0. }, false),
        (
            InjectEvent::Key {
                code: KeyCode::keyboard(4),
                pressed: false,
            },
            false,
        ),
    ] {
        h.driver.inject.push(InjectItem::Inject {
            event,
            from: Some(peer()),
        });
        let (reply, received) = mpsc::sync_channel(1);
        h.driver.command(InputCommand::ApplyOwnerPause {
            paused: true,
            expected_emergency_sequence: 0,
            reply,
        });
        let until = Instant::now() + Duration::from_secs(2);
        loop {
            h.poll();
            if let Ok(result) = received.try_recv() {
                assert_eq!(result.unwrap().cleanup_pending, pending);
                assert_eq!(
                    h.driver.shared.status.load().resource_faults.screen,
                    Some(if pending { Fault::CleanupFailed } else { Fault::None })
                );
                break;
            }
            assert!(Instant::now() < until);
            std::thread::yield_now();
        }
    }
}

#[test]
fn owner_resume_cannot_cross_an_emergency_sequence() {
    use oink_protocol::v1::dto::ControlFailure;
    let mut h = Harness::new();
    // A resume already queued to the native owner is invalidated by a later emergency.
    let (reply, result) = mpsc::sync_channel(1);
    h.driver.command(InputCommand::ApplyOwnerPause {
        paused: false,
        expected_emergency_sequence: 0,
        reply,
    });
    h.driver.adapter.trigger_emergency();
    h.driver.outputs(None);
    assert_eq!(h.driver.shared.status.load().emergency_sequence, 1);
    let until = Instant::now() + Duration::from_secs(2);
    loop {
        h.poll();
        if let Ok(result) = result.try_recv() {
            assert_eq!(result, Err(ControlFailure::StaleRevision));
            break;
        }
        assert!(Instant::now() < until);
        std::thread::yield_now();
    }
    assert!(h.driver.shared.status.load().paused);
    // A delayed ordinary command is rejected before changing router/native state.
    let (reply, result) = mpsc::sync_channel(1);
    h.driver.command(InputCommand::ApplyOwnerPause {
        paused: false,
        expected_emergency_sequence: 0,
        reply,
    });
    assert_eq!(result.try_recv().unwrap(), Err(ControlFailure::StaleRevision));
    assert!(h.driver.shared.status.load().paused);
}

#[test]
fn received_emergency_invalidates_resume_without_originating_another_group_event() {
    let mut h = Harness::new();
    h.driver.command(InputCommand::KeyboardDisabled(true));
    let (reply, received) = mpsc::sync_channel(1);
    h.driver.command(InputCommand::ReceiveEmergency { reply });
    let until = Instant::now() + Duration::from_secs(2);
    loop {
        h.poll();
        if let Ok(result) = received.try_recv() {
            let applied = result.unwrap();
            assert_eq!(applied.emergency_sequence, 1);
            assert_eq!(applied.local_emergency_sequence, 0);
            break;
        }
        assert!(Instant::now() < until);
        std::thread::yield_now();
    }
    assert!(h.driver.shared.status.load().paused);
    assert!(!h.driver.shared.status.load().keyboard_disabled);
    assert!(!std::iter::from_fn(|| h.driver.notices.take()).any(|notice| notice == InputNotice::EmergencyPause));
    let (reply, received) = mpsc::sync_channel(1);
    h.driver.command(InputCommand::ApplyOwnerPause {
        paused: false,
        expected_emergency_sequence: 0,
        reply,
    });
    assert_eq!(
        received.try_recv().unwrap(),
        Err(oink_protocol::v1::dto::ControlFailure::StaleRevision)
    );
}

#[test]
fn stale_keyboard_disable_cannot_cross_an_emergency() {
    let mut h = Harness::new();
    h.driver.adapter.trigger_emergency();
    h.driver.outputs(None);
    let (reply, result) = mpsc::sync_channel(1);
    h.driver.command(InputCommand::ApplyOwnerPreferences {
        expected_emergency_sequence: 0,
        routing: (*h.driver.settings).clone(),
        keyboard_disabled: true,
        input_transform: oink_foundation::input_transform::InputTransform::initial("macos"),
        accept_contacts: false,
        reply,
    });
    assert_eq!(
        result.try_recv().unwrap(),
        Err(oink_protocol::v1::dto::ControlFailure::StaleRevision)
    );
    assert!(!h.driver.adapter.router().keyboard_disabled());
}

#[test]
fn custom_emergency_hotkey_does_not_keep_paused_capture_active() {
    let mut h = Harness::new();
    let mut settings = (*h.driver.settings).clone();
    settings.emergency_combo = false;
    settings.emergency_hold_ms = 0;
    h.driver.snapshots.settings.publish(settings);
    h.poll();
    h.driver.command(InputCommand::Paused(true));
    assert!(!h.driver.capture_requested());
    h.driver
        .adapter
        .router_mut()
        .set_hotkeys(vec![oink_routing::HotkeyBinding {
            chord: oink_routing::Chord::parse(&["F12".to_owned()]).unwrap(),
            action: "pause_all".into(),
        }]);
    assert!(!h.driver.capture_requested());
}

#[test]
fn baseline_input_attaches_without_optional_opening_records_or_datagrams() {
    let mut h = Harness::new();
    let admission = wire::PeerAdmission {
        agreement: Arc::new(oink_protocol::v1::admission::Agreement::new(["oink.input/1".into()]).unwrap()),
        ..fixture_admission(peer(), LinkId(1))
    };
    h.driver.command(InputCommand::PeerAdmission(admission));
    let (input, mut read) = reliable_pair(LinkId(1), tag(1), QueueSpec::reliable(StreamKind::Input).0);
    h.driver.command(InputCommand::Outbound {
        peer: peer(),
        input,
        motion: None,
    });
    assert_eq!(drain(&mut read).len(), 3);
    let (write, input) = reliable_pair(LinkId(1), tag(1), QueueSpec::reliable(StreamKind::Input).1);
    h.driver.command(InputCommand::Inbound {
        peer: peer(),
        input,
        motion: None,
        peer_os: Os::Windows,
    });
    for message in remote().opening_state(true).into_iter().take(3) {
        write.post(encode(&message).unwrap()).unwrap();
    }
    h.attach();
    assert_eq!(h.driver.pairs[&peer()].opening, 3);
    drain(&mut read);
    h.driver.send(
        peer(),
        Message::Input(InputMsg::Gesture {
            source: oink_foundation::input::SourceKind::Touchpad,
            gesture: oink_foundation::input::Gesture::SmartMagnify,
        }),
    );
    assert!(read.take().unwrap().is_none());
    write.post(encode(&remote().opening_state(true)[3]).unwrap()).unwrap();
    h.driver.read();
    assert!(!h.driver.adapter.attached(peer()));
}

#[test]
fn replacement_boot_rejects_old_stream_handles_and_old_retirement() {
    let mut h = Harness::new();
    let (_read, _) = h.outbound(1);
    let (writer, _) = h.inbound(1);
    opening(&writer, &remote(), true);
    h.attach();
    let previous = fixture_boot(peer());
    let mut replacement = fixture_admission(peer(), LinkId(2));
    replacement.boot.nonce = [99; 16];
    h.driver.command(InputCommand::PeerAdmission(replacement.clone()));
    assert!(!h.driver.adapter.attached(peer()));
    assert!(h.driver.adapter.receive_state(peer()).is_none());
    let (input, _read) = reliable_pair(LinkId(1), tag(2), QueueSpec::reliable(StreamKind::Input).0);
    h.driver.command(InputCommand::Outbound {
        peer: peer(),
        input,
        motion: None,
    });
    assert!(h.driver.pairs[&peer()].outbound.is_none());
    h.driver.command(InputCommand::RetirePeer(previous));
    assert_eq!(
        h.driver.pairs[&peer()].admission.as_ref().unwrap().boot,
        replacement.boot
    );
    h.driver.command(InputCommand::RetirePeer(replacement.boot));
    assert!(h.driver.pairs[&peer()].admission.is_none());
}

#[test]
fn queued_writer_uses_current_native_permission_and_not_its_old_snapshot() {
    let mut h = Harness::new();
    h.driver.command(InputCommand::Controllable(false));
    assert_eq!(h.driver.notices.take(), Some(InputNotice::Controllable(false)));
    let (mut read, _) = h.outbound(1);
    assert!(matches!(
        drain(&mut read)[0],
        Message::Control(ControlMsg::InputCapability {
            controllable: false,
            ..
        })
    ));
    assert!(!h.driver.controllable);
    h.driver.command(InputCommand::Controllable(true));
    assert_eq!(h.driver.notices.take(), Some(InputNotice::Controllable(true)));
    let (mut read, _) = h.outbound(2);
    assert!(matches!(
        drain(&mut read)[0],
        Message::Control(ControlMsg::InputCapability { controllable: true, .. })
    ));
}

#[test]
fn unsupported_native_action_reports_numeric_violation_before_detaching() {
    use oink_protocol::v1::{
        self, RawValue as V, dto,
        schema::{WireDto, WireValue},
    };
    let mut h = Harness::new();
    let (_read, _) = h.outbound(1);
    let (write, _) = h.inbound(1);
    opening(&write, &remote(), true);
    h.attach();
    let value = v1::Message::InputButton(
        dto::InputButton::from_raw([(1, V::Map([(1, V::Uint(99))].into())), (2, true.write())].into()).unwrap(),
    );
    write
        .post(v1::frame::encode(&value, v1::limits::RELIABLE_FRAME).unwrap())
        .unwrap();
    h.poll();
    assert!(!h.driver.adapter.attached(peer()));
    assert!(std::iter::from_fn(|| h.driver.notices.take()).any(|notice| matches!(
        notice,
        InputNotice::ProtocolViolation {
            type_id: Some(0x0203),
            code: v1::ErrorCode::UnsupportedValue,
            datagram: false,
            ..
        }
    )));
}

/// Facts can arrive while the peer's I0 opening is still in flight. Attachment
/// must retain that OS observation for source-owned Caps tap/hold selection.
#[test]
#[cfg(target_os = "macos")]
fn migration_peer_facts_before_opening_preserve_caps_tap_mapping() {
    use oink_foundation::input::{InputEvent, KeyCode, SourceKind};
    for initial_os in [Os::Windows, Os::Other] {
        let mut h = Harness::new();
        let (mut reader, _datagrams) = h.outbound(1);
        let (writer, input) = reliable_pair(LinkId(1), tag(1), QueueSpec::reliable(StreamKind::Input).1);
        h.driver.command(InputCommand::Inbound {
            peer: peer(),
            input,
            motion: None,
            peer_os: initial_os, // Unknown when admission preceded the first DeviceFacts.
        });
        h.driver.command(InputCommand::PeerFacts {
            boot: fixture_boot(peer()),
            os: Os::Windows,
            displays: Vec::new(),
            transform: Some(oink_foundation::input_transform::InputTransform::initial("windows")),
        });
        let state = remote();
        opening(&writer, &state, true);
        h.attach();
        h.driver.message(
            peer(),
            Message::Control(ControlMsg::FocusState {
                lock_state: None,
                epoch: 5,
                holder: peer(),
                scope: Some(input_scope(&h.driver)),
                session: Some(state.router().routing_session()),
            }),
        );
        assert_eq!(h.driver.adapter.focus().holder, peer());
        drain(&mut reader);
        h.driver.adapter.router_mut().set_mac_caps_switch(true);
        for (pressed, time_us) in [(true, 10), (false, 100_000)] {
            h.driver.adapter.router_mut().on_local_event(
                &CapturedEvent {
                    event: InputEvent::Key {
                        code: KeyCode::CAPS_LOCK,
                        pressed,
                    },
                    physical: true,
                    source: SourceKind::Keyboard,
                    cursor: None,
                    time_us,
                },
                time_us,
            );
            h.driver.outputs(None);
        }
        let keys: Vec<_> = drain(&mut reader)
            .into_iter()
            .filter_map(|message| match message {
                Message::Input(InputMsg::CapsKey { code, pressed } | InputMsg::Key { code, pressed }) => {
                    Some((code, pressed))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            keys,
            [(KeyCode::LEFT_SHIFT, true), (KeyCode::LEFT_SHIFT, false)],
            "late I0 attachment must not overwrite Windows DeviceFacts with the earlier unknown OS"
        );
    }
}

#[test]
fn queued_capture_sample_cannot_borrow_the_new_focus_epoch() {
    use oink_input::pointer_capture::CaptureSample;
    for kind in 0..4 {
        let mut h = Harness::new();
        h.driver.command(InputCommand::LocalDisplays(vec![fixture_display()]));
        let focus = h.driver.adapter.router().focus();
        let current = Some((focus.epoch, focus.holder));
        let sample = CaptureSample {
            windows: None,
            hidden: true,
            application_hidden: Some(true),
            confined: false,
            position: oink_foundation::Point::new(400., 400.),
            motion_serial: 0,
            motion_observable: true,
            foreground: 1,
        };
        // The observer sampled our frozen source in the preceding focus epoch.
        // Delivery is delayed until the router has returned focus here. Replaying
        // old evidence must never start the current holder's capture detector.
        h.driver.command(InputCommand::PointerSample {
            application: None,
            scope: current,
            sample: Some(CaptureSample {
                hidden: false,
                application_hidden: Some(false),
                position: oink_foundation::Point::new(200., 400.),
                ..sample
            }),
            hidden: false,
        });
        h.driver.now += Duration::from_millis(50);
        // A hidden no-event warp is the macOS passive capture signature. The
        // same signature from our old handoff must be rejected before detection.
        let observed_scope = match kind {
            0 => None,
            1 => Some((focus.epoch.wrapping_sub(1), focus.holder)),
            2 => Some((focus.epoch, peer())),
            _ => current,
        };
        for _ in 0..2 {
            h.driver.command(InputCommand::PointerSample {
                application: None,
                scope: observed_scope,
                sample: Some(sample),
                hidden: true,
            });
            h.driver.now += Duration::from_millis(400);
        }
        assert_eq!(
            h.driver.adapter.router().game_mode().detected,
            observed_scope == current
        );
        assert_eq!(h.driver.adapter.router().game_mode().active, observed_scope == current);
    }
}

#[test]
fn capture_transitions_reach_typed_recorder_with_native_tracing_disabled() {
    use crate::diagnostics::{Reason, Recorder, capture::Application, input_trace::InputTrace};
    use oink_input::pointer_capture::CaptureSample;
    use std::io::Read;
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new();
    h.driver.command(InputCommand::LocalDisplays(vec![fixture_display()]));
    let focus = h.driver.adapter.router().focus();
    let recorder = Recorder::start(dir.path().into(), focus.holder, None, true).unwrap();
    recorder.wait_recovery();
    let trace = InputTrace::for_recorder(&recorder);
    h.driver.inject.trace = Some(trace.clone());
    let mut sample = CaptureSample {
        windows: None,
        hidden: false,
        application_hidden: Some(false),
        confined: false,
        position: oink_foundation::Point::new(200., 400.),
        motion_serial: 0,
        motion_observable: true,
        foreground: 42,
    };
    let application = Application::new(42, "org.example.Game");
    // This is the native owner's production subscriber policy. The regression
    // must exercise Driver -> typed queue -> recorder, with no tracing fallback.
    tracing::subscriber::with_default(tracing::subscriber::NoSubscriber::default(), || {
        let observe = |h: &mut Harness, sample| {
            h.driver.command(InputCommand::PointerSample {
                scope: Some((focus.epoch, focus.holder)),
                application,
                sample: Some(sample),
                hidden: sample.hidden,
            });
            h.driver.outputs(None);
        };
        observe(&mut h, sample);
        sample.hidden = true;
        sample.application_hidden = Some(true);
        sample.position.x = 400.;
        h.driver.now += Duration::from_millis(50);
        observe(&mut h, sample);
        h.driver.now += Duration::from_millis(300);
        observe(&mut h, sample);
        assert!(h.driver.adapter.router().game_mode().active);
        // Unchanged evidence must not turn this into a per-sample trace.
        for _ in 0..10 {
            h.driver.now += Duration::from_millis(50);
            observe(&mut h, sample);
        }
        sample.hidden = false;
        sample.application_hidden = Some(false);
        observe(&mut h, sample);
        h.driver.now += Duration::from_millis(200);
        observe(&mut h, sample);
        assert!(!h.driver.adapter.router().game_mode().active);
    });
    trace.drain(&recorder);
    let bundle = recorder.save(Reason::Manual).unwrap();
    let mut zip = zip::ZipArchive::new(std::fs::File::open(bundle.path).unwrap()).unwrap();
    let mut text = String::new();
    zip.by_name("merged.ndjson").unwrap().read_to_string(&mut text).unwrap();
    let rows: Vec<serde_json::Value> = text
        .lines()
        .map(|r| serde_json::from_str::<serde_json::Value>(r).unwrap())
        .filter_map(|r| r["event"].get("Log").cloned())
        .filter(|r| r["module"] == "oink_pointer_capture")
        .collect();
    assert_eq!(
        rows.len(),
        5,
        "only evidence/enter/exit transitions are recorded: {rows:?}"
    );
    let entered = rows
        .iter()
        .find(|r| {
            r["fields"]
                .as_array()
                .unwrap()
                .iter()
                .any(|f| f[0] == "active" && f[1] == 1.0)
        })
        .unwrap();
    for name in [
        "epoch",
        "sample_epoch",
        "hidden",
        "application_hidden",
        "locked",
        "foreground_pid",
        "warp_lock",
        "stationary_us",
        "movement_samples",
    ] {
        assert!(
            entered["fields"].as_array().unwrap().iter().any(|f| f[0] == name),
            "missing {name}"
        );
    }
    for name in ["reason", "holder", "sample_holder", "screen", "foreground_bundle_id"] {
        assert!(
            entered["labels"].as_array().unwrap().iter().any(|l| l[0] == name),
            "missing {name}"
        );
    }
    assert!(
        entered["labels"]
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l[0] == "foreground_bundle_id" && l[1] == "org.example.Game")
    );
    assert!(
        rows.last().unwrap()["fields"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f[0] == "active" && f[1] == 0.0)
    );
    assert!(Application::new(42, "/private/window title").is_none());
    assert!(Application::new(42, &"x".repeat(257)).is_none());
}

#[test]
fn negative_display_entry_with_visible_blank_glyph_never_counts_as_capture() {
    use oink_input::pointer_capture::CaptureSample;
    let mut h = Harness::new();
    let mut external = fixture_display();
    external.key = "external".into();
    external.bounds = oink_foundation::Rect::new(1560., -1080., 1920., 1080.);
    h.driver
        .command(InputCommand::LocalDisplays(vec![fixture_display(), external]));
    let focus = h.driver.adapter.router().focus();
    let mut sample = CaptureSample {
        windows: None,
        hidden: false,
        application_hidden: Some(false),
        confined: false,
        position: oink_foundation::Point::new(2520.5, -0.02),
        motion_serial: 0,
        motion_observable: true,
        foreground: 42,
    };
    for x in [3455.999995, 3114.112144, 2520.5] {
        sample.position.x = x;
        // Cover observed external entries and the subpoint boundary that used
        // to break AppKit hit testing. macOS capture evidence has no glyph-image
        // or NSCursor input: a rendered blank with a visible flag stays visible.
        for y in [-4.683405, -8.028689, -0.02, -540.] {
            sample.position.y = y;
            for _ in 0..20 {
                sample.motion_serial += 1;
                h.driver.now += Duration::from_millis(50);
                h.driver.command(InputCommand::PointerSample {
                    scope: Some((focus.epoch, focus.holder)),
                    application: None,
                    sample: Some(sample),
                    hidden: false,
                });
                assert!(!h.driver.adapter.router().game_mode().detected);
                assert!(!h.driver.adapter.router().game_mode().active);
            }
        }
    }
    // Genuine hidden stationary movement can still enter on that display.
    sample.hidden = true;
    sample.application_hidden = Some(true);
    sample.position = oink_foundation::Point::new(2520., -540.);
    for _ in 0..20 {
        sample.motion_serial += 1;
        h.driver.now += Duration::from_millis(50);
        h.driver.command(InputCommand::PointerSample {
            scope: Some((focus.epoch, focus.holder)),
            application: None,
            sample: Some(sample),
            hidden: true,
        });
    }
    assert!(h.driver.adapter.router().game_mode().active);
}
