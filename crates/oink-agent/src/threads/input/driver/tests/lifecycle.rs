//! Starts the real input owner through InputThread using only the memory platform backend.
use super::*;
use oink_foundation::Point;
use oink_input::InputBackend;

#[test]
fn pause_returns_source_home_mid_key_or_drag_and_passes_local_input() {
    use oink_input::{InputEvent, KeyCode, MouseButton, SourceKind};
    for event in [
        InputEvent::Key {
            code: KeyCode::LEFT_SHIFT,
            pressed: true,
        },
        InputEvent::Button {
            button: MouseButton::Left,
            pressed: true,
        },
    ] {
        let fake = Arc::new(oink_test_support::input::FakeInput::new());
        let mut h = Harness::with_backend(fake.clone());
        let (mut read, _motion) = h.outbound(1);
        let (write, _incoming_motion) = h.inbound(1);
        let remote = remote();
        opening(&write, &remote, true);
        h.attach();
        h.driver.command(InputCommand::Paused(false));
        h.driver.message(
            peer(),
            Message::Control(ControlMsg::FocusState {
                lock_state: None,
                epoch: 5,
                holder: peer(),
                scope: Some(input_scope(&h.driver)),
                session: Some(remote.router().routing_session()),
            }),
        );
        assert_eq!(h.driver.adapter.focus().holder, peer());
        drain(&mut read);
        let captured = CapturedEvent {
            event,
            physical: true,
            source: SourceKind::Mouse,
            time_us: 1,
            cursor: Some(Point::new(20., 20.)),
        };
        assert_eq!(h.driver.event(&captured), Disposition::Swallow);
        h.driver.command(InputCommand::Paused(true));
        // The event callback must pass immediately, before the owner has polled.
        assert_eq!(h.driver.event(&captured), Disposition::Pass);
        h.poll();
        assert_eq!(h.driver.adapter.focus().holder, h.driver.local);
        assert!(!h.driver.capture_requested());
        assert!(!h.driver.adapter.attached(peer()));
        // Closing I0 releases the origin at its receiver, including a lost Up.
        assert!(h.driver.pairs[&peer()].outbound.is_none());
        assert!(h.driver.adapter.fully_paused());
        h.driver.command(InputCommand::Paused(false));
        wait(|| {
            h.poll();
            h.driver.capture_requested()
        });
        assert!(h.driver.capture_requested());
        assert!(
            !h.driver.adapter.attached(peer()),
            "resume must not resurrect an old lease"
        );
    }
}

#[test]
fn pause_and_resume_before_poll_discards_old_inbound_bytes() {
    use oink_input::KeyCode;
    let fake = Arc::new(oink_test_support::input::FakeInput::new());
    let mut h = Harness::with_backend(fake.clone());
    let (_read, _motion) = h.outbound(1);
    let (write, _incoming_motion) = h.inbound(1);
    opening(&write, &remote(), true);
    h.attach();
    for message in [
        Message::Control(ControlMsg::FocusEnter {
            epoch: 200,
            display: "screen".into(),
            pos: Point::new(5., 5.),
            scope: Some(input_scope(&h.driver)),
            pointer_source: Some(peer()),
        }),
        Message::Input(InputMsg::Key {
            code: KeyCode::keyboard(4),
            pressed: true,
        }),
    ] {
        write.post(encode(&message).unwrap()).unwrap();
    }
    fake.set_input_paused(true);
    fake.set_input_paused(false);
    h.poll();
    assert!(!h.driver.adapter.attached(peer()));
    assert!(h.driver.pairs[&peer()].inbound.is_none());
    assert!(h.driver.pairs[&peer()].outbound.is_none());
    drop(h);
    assert!(
        !fake
            .injected()
            .iter()
            .any(|event| matches!(event, InjectEvent::Key { pressed: true, .. }))
    );
}

#[test]
fn safety_fence_reports_intentional_stream_cancellation() {
    let fake = Arc::new(oink_test_support::input::FakeInput::new());
    let mut h = Harness::with_backend(fake.clone());
    let (_read, _motion) = h.outbound(1);
    let (write, _incoming_motion) = h.inbound(1);
    opening(&write, &remote(), true);
    h.attach();
    // Both native transitions precede the next owner poll and C0 publication.
    fake.set_input_paused(true);
    fake.set_input_paused(false);
    h.poll();
    let faults: Vec<_> = std::iter::from_fn(|| h.driver.notices.take())
        .filter_map(|notice| match notice {
            InputNotice::Fault { which, code, .. } => Some((which, code)),
            _ => None,
        })
        .collect();
    assert_eq!(faults.len(), 2);
    assert!(
        faults.iter().all(|(_, code)| *code == StreamCode::Cancelled),
        "{faults:?}"
    );
}

#[test]
fn cancelled_writer_post_before_close_event_preserves_cause() {
    let mut h = Harness::new();
    let (read, _motion) = h.outbound(1);
    let (write, _incoming_motion) = h.inbound(1);
    opening(&write, &remote(), true);
    h.attach();
    let encoded = encode(&h.driver.adapter.opening_state(true)[0]).unwrap();
    // Model the network processing the peer's STOP_SENDING while the ordinary
    // input task has not yet delivered StreamClosed. A native post sees Closed
    // first; its fault notice must not turn that same cancellation into failure.
    read.cancel();
    assert!(!h.driver.post_bytes(peer(), &encoded, false));
    let faults: Vec<_> = std::iter::from_fn(|| h.driver.notices.take())
        .filter_map(|notice| match notice {
            InputNotice::Fault { which, code, .. } => Some((which, code)),
            _ => None,
        })
        .collect();
    assert_eq!(faults, vec![(Which::Outbound, StreamCode::Cancelled)]);
}
fn wait(mut predicate: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(3);
    while !predicate() {
        assert!(Instant::now() < end, "input owner did not reach the requested state");
        std::thread::sleep(Duration::from_millis(1));
    }
}
#[test]
fn paused_capture_registers_only_resume_hotkey_and_resume_starts_fresh_session() {
    pause_resume_injection(false);
}

#[test]
fn paused_capture_with_delayed_initial_injection_counts_only_one_resumed_session() {
    pause_resume_injection(true);
}

fn pause_resume_injection(delay_initial_injection: bool) {
    let fake = Arc::new(oink_test_support::input::FakeInput::new());
    let startup = delay_initial_injection.then(|| fake.block_next_injection_start());
    let inject = super::super::super::super::inject::start_with_backend(fake.clone(), Box::new(|_| {})).unwrap();
    if let Some((entered, _)) = &startup {
        entered.recv_timeout(Duration::from_secs(3)).unwrap();
    }
    let layout = oink_display::layout::Layout::default();
    let snapshots = InputSnapshots {
        mode_switch_in_callback: true,
        capture_reports_key_repeat: false,
        local_input_transform: oink_foundation::input_transform::InputTransform::initial(std::env::consts::OS),
        layout: Arc::new(Snapshot::new(RouterLayout {
            application_focus_enabled: false,
            group: oink_foundation::GroupId([0; 16]),
            owners: Arc::default(),
            version: fixture_version(),
            topology: oink_routing::TopologyLayout::from_layout(&layout, &Default::default()),
            layout: Arc::new(layout),
            devices: Default::default(),
        })),
        settings: Arc::new(Snapshot::default()),
        environment: Arc::new(Snapshot::default()),
    };
    let (main, receive_main) = crate::threads::main_channel();
    let (input, _notices) = InputThread::start(fixture_boot(DeviceId([1; 32])), snapshots, inject, main).unwrap();
    // Drop the release channel before the worker-owning handles on unwind too.
    let initial_injection_start = startup;
    // Startup waits for the initial owner pause command before requesting capture.
    assert_eq!(fake.capture_active(), Some(false));
    input.send(InputCommand::Paused(false));
    wait(|| fake.capture_active() == Some(true));
    let (write, read) = reliable_pair(LinkId(1), tag(1), QueueSpec::reliable(StreamKind::Input).1);
    input.send(InputCommand::PeerAdmission(fixture_admission(peer(), LinkId(1))));
    input.send(InputCommand::Inbound {
        peer: peer(),
        input: read,
        motion: None,
        peer_os: Os::Windows,
    });
    let (out, _remote_reader) = reliable_pair(LinkId(1), tag(1), QueueSpec::reliable(StreamKind::Input).0);
    let (motion, _remote_motion) = datagram_pair(LinkId(1), tag(1), QueueSpec::datagram(StreamKind::Input));
    input.send(InputCommand::PeerAdmission(fixture_admission(peer(), LinkId(1))));
    input.send(InputCommand::Outbound {
        peer: peer(),
        input: out,
        motion: Some(motion),
    });
    opening(&write, &remote(), true);
    wait(|| input.status().peers.iter().any(|p| p.attached));
    let old_epoch = fake.input_safety().epoch;
    let old_session = fake.capture_sessions();
    // Capture/admission is deliberately allowed to progress while the native
    // injection worker has not started. Release before asking it to stop.
    drop(initial_injection_start);
    input.send(InputCommand::Hotkeys(vec![oink_routing::HotkeyBinding {
        action: "toggle_sharing".into(),
        chord: oink_routing::Chord::parse(&["Ctrl", "Alt", "S"]).unwrap(),
    }]));
    input.send(InputCommand::KeyboardDisabled(true));
    input.send(InputCommand::Paused(true));
    wait(|| fake.capture_active() == Some(false) && fake.paused_hotkey().is_some());
    wait(|| input.status().injection_detached);
    assert!(!fake.injection_active());
    // Peer attachment/capture readiness does not fence native injection startup.
    // Count the old worker only after pause has finished it, so a delayed first
    // start cannot be mistaken for an extra session created by resume.
    let old_inject = fake.injection_sessions();
    let paused = fake.input_safety();
    assert!(paused.paused);
    assert_eq!(paused.epoch, old_epoch + 1);
    assert_eq!(fake.paused_hotkey().unwrap().key, oink_input::KeyCode::keyboard(22));
    assert_eq!(
        fake.feed(&CapturedEvent {
            event: oink_input::InputEvent::Key {
                code: oink_input::KeyCode::ESCAPE,
                pressed: true
            },
            physical: true,
            source: oink_input::SourceKind::Keyboard,
            time_us: 0,
            cursor: None,
        }),
        Disposition::Pass,
        "no paused keyboard guard or long-Escape interception"
    );
    assert!(
        write.post(encode(&remote().opening_state(true)[1]).unwrap()).is_err(),
        "old input stream must be closed"
    );
    fake.fire_resume_hotkey();
    wait(|| {
        receive_main.try_iter().any(|command| {
            matches!(command,
        MainCommand::ResumeSharing { epoch } if epoch == paused.epoch)
        })
    });
    // The ordinary owner consumes this notification; this fixture supplies its
    // resulting command without invoking an installed app or a native hotkey.
    input.send(InputCommand::Paused(false));
    wait(|| fake.capture_active() == Some(true) && fake.paused_hotkey().is_none());
    assert_eq!(fake.input_safety().epoch, paused.epoch + 1);
    assert_eq!(fake.capture_sessions(), old_session + 1);
    wait(|| fake.injection_active());
    assert_eq!(fake.injection_sessions(), old_inject + 1);
    assert!(!input.status().peers[0].attached, "resume cannot reuse the old pair");
    input.stop();
    assert_eq!(fake.capture_active(), Some(false));
}

#[test]
fn handoff_to_unavailable_target_is_rejected_without_replacing_the_pair() {
    use oink_foundation::input::KeyCode;
    let fake = Arc::new(oink_test_support::input::FakeInput::new());
    let mut h = Harness::with_backend(fake.clone());
    let (mut replies, _out_motion) = h.outbound(1);
    let (inbound, _in_motion) = h.inbound(1);
    opening(&inbound, &remote(), true);
    h.attach();
    drain(&mut replies);
    let instance = h.driver.adapter.routing_session();
    h.driver.command(InputCommand::Controllable(false));
    for message in [
        Message::Control(ControlMsg::FocusEnter {
            epoch: 200,
            display: "screen".into(),
            pos: oink_foundation::Point::new(5., 5.),
            scope: Some(input_scope(&h.driver)),
            pointer_source: Some(peer()),
        }),
        Message::Input(InputMsg::Key {
            code: KeyCode::keyboard(4),
            pressed: true,
        }),
    ] {
        inbound.post(encode(&message).unwrap()).unwrap();
    }
    h.poll();
    assert!(h.driver.adapter.attached(peer()));
    assert_eq!(h.driver.adapter.routing_session(), instance);
    assert_eq!(h.driver.adapter.focus().holder, h.driver.local);
    assert!(drain(&mut replies).iter().any(|message| matches!(
        message,
        Message::Control(ControlMsg::FocusState { holder, .. }) if *holder == h.driver.local
    )));
    // Stop drains the actual injection queue before inspecting its platform effects.
    drop(h);
    assert!(
        !fake
            .take_injected()
            .iter()
            .any(|event| matches!(event, InjectEvent::Key { pressed: true, .. }))
    );
}

#[test]
fn physical_input_driving_a_peer_declares_local_activity_with_group_wake_disabled() {
    use oink_foundation::input::{InputEvent, KeyCode, SourceKind};
    let fake = Arc::new(oink_test_support::input::FakeInput::new());
    let mut h = Harness::with_backend(fake.clone());
    let (mut reader, _out_motion) = h.outbound(1);
    let (writer, _in_motion) = h.inbound(1);
    let remote = remote();
    opening(&writer, &remote, true);
    h.attach();
    let display = oink_foundation::display::DisplayInfo {
        key: "screen".into(),
        name: "screen".into(),
        bounds: oink_foundation::Rect::new(0., 0., 1000., 1000.),
        pixel_width: 1000,
        pixel_height: 1000,
        reference_extent: Some(oink_foundation::Size::new(300., 300.)),
        rotation_deg: 0,
        is_primary: true,
        is_builtin: false,
        is_virtual: false,
    };
    h.driver.command(InputCommand::LocalDisplays(vec![display.clone()]));
    let layout = Arc::new(oink_display::layout::Layout::auto(&[
        (h.driver.local, vec![display.clone()]),
        (peer(), vec![display]),
    ]));
    h.driver.snapshots.layout.publish(RouterLayout {
        application_focus_enabled: false,
        group: oink_foundation::GroupId([0; 16]),
        owners: Arc::default(),
        version: fixture_version(),
        topology: oink_routing::TopologyLayout::from_layout(&layout, &Default::default()),
        layout,
        devices: BTreeSet::from([h.driver.local, peer()]),
    });
    let mut settings = h.driver.adapter.config().settings.clone();
    settings.wake_displays_on_input = false;
    h.driver.snapshots.settings.publish(settings);
    h.driver.refresh();
    h.driver.adapter.set_cursor(oink_foundation::Point::new(995., 500.));
    for _ in 0..20 {
        h.driver.event(&CapturedEvent {
            physical: false,
            event: InputEvent::PointerDelta { dx: 100., dy: 0. },
            source: SourceKind::Mouse,
            time_us: 0,
            cursor: Some(oink_foundation::Point::new(999., 500.)),
        });
        h.settle_clip_check();
        if h.driver.adapter.focus().holder == peer() {
            break;
        }
    }
    assert_eq!(h.driver.adapter.focus().holder, peer());
    drain(&mut reader);
    while h._main.try_recv().is_ok() {}
    let event = CapturedEvent {
        physical: true,
        event: InputEvent::Key {
            code: KeyCode::keyboard(4),
            pressed: true,
        },
        source: SourceKind::Keyboard,
        time_us: 0,
        cursor: None,
    };
    let _capture = oink_input::source_time::CaptureScope::new(CaptureTime(123));
    h.driver.event(&event);
    assert!(drain(&mut reader).iter().any(|message| matches!(
        message,
        Message::TimedInput {
            time: CaptureTime(123),
            input: InputMsg::Key { pressed: true, .. }
        }
    )));
    assert_eq!(
        fake.activity_declaration_count(),
        0,
        "the input owner performs no OS activity call"
    );
    let mut local = 0;
    for command in h._main.try_iter() {
        match command {
            MainCommand::Activity { from } => {
                assert!(from.is_none());
                crate::threads::declare_activity(&*fake, from, false).unwrap();
                local += 1;
            }
            MainCommand::ResetUserIdle => panic!("group wake was disabled"),
            _ => {}
        }
    }
    assert_eq!(local, 1);
    assert_eq!(fake.activity_declaration_count(), 1);
    assert!(!crate::threads::declare_activity(&*fake, Some(peer()), false).unwrap());
    assert_eq!(
        fake.activity_declaration_count(),
        1,
        "remote wake still obeys the setting"
    );
}

#[test]
fn overlay_escape_dismisses_on_release_and_hold_still_emergency_pauses() {
    use oink_foundation::input::{InputEvent, KeyCode, SourceKind};
    let escape = |pressed| CapturedEvent {
        event: InputEvent::Key {
            code: KeyCode::ESCAPE,
            pressed,
        },
        physical: true,
        source: SourceKind::Keyboard,
        time_us: 0,
        cursor: None,
    };
    let mut h = Harness::new();
    let (mut read, _motion) = h.outbound(1);
    let (write, _incoming_motion) = h.inbound(1);
    opening(&write, &remote(), true);
    h.attach();
    drain(&mut read);
    h.driver.command(InputCommand::Paused(false));
    h.driver.command(InputCommand::Overlay(Some(42)));
    assert_eq!(h.driver.event(&escape(true)), Disposition::Swallow);
    assert_eq!(h.driver.event(&escape(false)), Disposition::Swallow);
    assert!(
        h._main
            .try_iter()
            .any(|command| matches!(command, MainCommand::DismissOverlay(42)))
    );
    assert!(!h.driver.adapter.fully_paused());
    assert!(
        !drain(&mut read)
            .iter()
            .any(|message| matches!(message, Message::Input(InputMsg::Key { .. })))
    );
    assert_eq!(h.driver.event(&escape(true)), Disposition::Swallow);
    let (_, held_at) = h.driver.overlay_escape.unwrap();
    let deadline = held_at + Duration::from_millis(u64::from(h.driver.settings.emergency_hold_ms));
    h.driver.poll(deadline);
    assert!(h.driver.adapter.fully_paused());
    assert!(
        !h.driver.capture_requested(),
        "pause removes every input interception path"
    );
    assert!(std::iter::from_fn(|| h.driver.notices.take()).any(|notice| notice == InputNotice::EmergencyPause));
}

#[test]
fn system_sleep_withdraws_capture_and_wake_preserves_explicit_pause() {
    let mut h = Harness::new();
    h.driver.command(InputCommand::Paused(false));
    assert!(h.driver.capture_requested());
    let mut environment = *h.driver.snapshots.environment.load();
    environment.system_sleeping = true;
    h.driver.snapshots.environment.publish(environment);
    h.poll();
    assert!(!h.driver.capture_requested());
    assert!(h.driver.adapter.fully_paused());
    environment.system_sleeping = false;
    h.driver.snapshots.environment.publish(environment);
    h.poll();
    assert!(h.driver.capture_requested());
    assert!(!h.driver.adapter.fully_paused());
    h.driver.command(InputCommand::Paused(true));
    environment.system_sleeping = true;
    h.driver.snapshots.environment.publish(environment);
    h.poll();
    environment.system_sleeping = false;
    h.driver.snapshots.environment.publish(environment);
    h.poll();
    assert!(!h.driver.capture_requested());
    assert!(h.driver.adapter.fully_paused());
}

#[test]
fn input_backlog_older_than_five_seconds_discards_every_stream_and_pauses_locally() {
    let mut h = Harness::new();
    let (_read, _motion) = h.outbound(1);
    let (write, _incoming) = h.inbound(1);
    opening(&write, &remote(), true);
    h.attach();
    h.driver.command(InputCommand::Paused(false));
    let now = Instant::now();
    h.driver.poll(now + Duration::from_secs(5) + Duration::from_millis(1));
    assert!(h.driver.paused, "expired capture-to-send work must pause this device");
    assert!(
        h.driver
            .pairs
            .values()
            .all(|pair| pair.outbound.is_none() && pair.inbound.is_none())
    );
}

#[test]
fn backlog_threshold_is_strict_and_inflight_age_survives_an_empty_queue() {
    let mut h = Harness::new();
    h.driver.command(InputCommand::Paused(false));
    // A retained timing guard represents a native call already removed from
    // the queue, which the routing owner must still time independently.
    let timing = h.driver.inject.timing();
    let pending = timing.pending(Instant::now());
    let at = timing.oldest_pending().unwrap();
    h.driver.check_backlog(at + Duration::from_secs(5));
    assert!(!h.driver.paused);
    h.driver
        .check_backlog(at + Duration::from_secs(5) + Duration::from_nanos(1));
    assert!(h.driver.paused);
    assert_eq!(h.driver.backlog_sequence, 1);
    assert_eq!(h.driver.local_emergency_sequence, 0);
    h.driver.check_backlog(at + Duration::from_secs(6));
    assert_eq!(h.driver.backlog_sequence, 1);
    drop(pending);
}

#[test]
fn ordinary_input_backlog_is_guarded_and_old_work_cannot_age_a_resumed_session() {
    let fake = Arc::new(oink_test_support::input::FakeInput::new());
    let mut h = Harness::with_backend(fake.clone());
    h.driver.command(InputCommand::Paused(false));
    let old = h.driver.inject.ordinary_input(fake.input_safety()).unwrap();
    let at = h.driver.inject.oldest_pending().unwrap();
    h.driver
        .check_backlog(at + Duration::from_secs(5) + Duration::from_millis(1));
    assert!(h.driver.paused);
    assert_eq!(h.driver.backlog_sequence, 1);
    assert!(!fake.input_safety().accepts(old.safety));
    h.driver.command(InputCommand::Paused(false));
    assert!(
        h.driver.inject.oldest_pending().is_none(),
        "invalidated ordinary commands have no live input age"
    );
    h.driver.check_backlog(at + Duration::from_secs(10));
    assert!(!h.driver.paused);
    drop(old);
}

#[test]
fn session_idle_refresh_rechecks_pause_sleep_wake_setting_and_input_safety() {
    let fake = Arc::new(oink_test_support::input::FakeInput::new());
    let mut h = Harness::with_backend(fake.clone());
    h.driver.command(InputCommand::Paused(false));
    assert!(h.driver.idle_refresh_allowed());
    let mut settings = (*h.driver.snapshots.settings.load()).clone();
    settings.wake_displays_on_input = false;
    h.driver.snapshots.settings.publish(settings.clone());
    assert!(!h.driver.idle_refresh_allowed(), "setting takes effect before polling");
    settings.wake_displays_on_input = true;
    h.driver.snapshots.settings.publish(settings);
    assert!(h.driver.idle_refresh_allowed());
    h.driver.command(InputCommand::Paused(true));
    assert!(!h.driver.idle_refresh_allowed());
    h.driver.command(InputCommand::Paused(false));
    wait(|| {
        h.poll();
        h.driver.idle_refresh_allowed()
    });
    Arc::make_mut(&mut h.driver.environment).system_sleeping = true;
    assert!(!h.driver.idle_refresh_allowed());
    Arc::make_mut(&mut h.driver.environment).system_sleeping = false;
    fake.set_input_paused(true);
    assert!(
        !h.driver.idle_refresh_allowed(),
        "native safety revocation wins before the next owner poll"
    );
}
