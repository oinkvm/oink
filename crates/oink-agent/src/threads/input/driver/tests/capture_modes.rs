//! Recording native backend: no Windows hooks, windows or cursor operations.
use super::*;
use oink_foundation::{
    Point, Rect, Size,
    display::DisplayInfo,
    input::{CaptureMode, FeelProfile, InputEvent, SourceKind},
};
use oink_input::{EventHandler, InputBackend, InputError, InputPermissions};
use oink_test_support::input::PendingMode;
use std::sync::Mutex;

struct RecordingInput {
    fake: oink_test_support::input::FakeInput,
    release: Mutex<Option<mpsc::Receiver<()>>>,
    entered: mpsc::Sender<()>,
    modes: Mutex<Vec<(PendingMode, std::thread::ThreadId)>>,
}
struct InjectionRelease(mpsc::Sender<()>);
impl InjectionRelease {
    fn send(&self, value: ()) -> Result<(), mpsc::SendError<()>> {
        self.0.send(value)
    }
}
impl Drop for InjectionRelease {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}
impl InputBackend for RecordingInput {
    fn start(&self, handler: EventHandler) -> Result<(), InputError> {
        self.fake.start(handler)
    }
    fn stop(&self) {
        self.fake.stop();
    }
    fn set_mode(&self, mode: CaptureMode) -> Result<(), InputError> {
        self.fake.set_mode(mode)?;
        self.modes
            .lock()
            .unwrap()
            .push((PendingMode::Set(mode), std::thread::current().id()));
        Ok(())
    }
    fn restore_local(&self, at: Point) -> Result<(), InputError> {
        self.fake.restore_local(at)?;
        self.modes
            .lock()
            .unwrap()
            .push((PendingMode::RestoreLocal(at), std::thread::current().id()));
        Ok(())
    }
    fn inject(&self, event: &InjectEvent) -> Result<(), InputError> {
        if let Some(release) = self.release.lock().unwrap().take() {
            self.entered.send(()).unwrap();
            // The sender drops on assertion unwind too. Keep the gate closed
            // until explicit release; elapsed test scheduling is not release.
            let _ = release.recv();
        }
        self.fake.inject(event)
    }
    fn cursor_position(&self) -> Result<Point, InputError> {
        self.fake.cursor_position()
    }
    fn feel_profile(&self) -> FeelProfile {
        self.fake.feel_profile()
    }
    fn permissions(&self) -> InputPermissions {
        self.fake.permissions()
    }
}

#[test]
fn handoff_parks_in_capture_callback_and_older_queued_restore_cannot_uncover_it() {
    handoff(true, false);
}

#[test]
fn disabling_callback_modes_preserves_queued_restore_then_park_order() {
    handoff(false, false);
}

#[test]
fn incoming_shared_mouse_hides_a_mouse_local_receiver_and_own_motion_restores_only_its_cursor() {
    handoff(true, true);
}

fn handoff(immediate: bool, local_mouse: bool) {
    let (release, blocked) = mpsc::channel();
    let (entered, entered_rx) = mpsc::channel();
    let backend = Arc::new(RecordingInput {
        fake: oink_test_support::input::FakeInput::new(),
        release: Mutex::new(Some(blocked)),
        entered,
        modes: Mutex::new(Vec::new()),
    });
    backend.start(Arc::new(|_| Disposition::Pass)).unwrap();
    let mut h = Harness::with_backend(backend.clone());
    // Drop before the harness joins its injection worker, including on panic.
    let release = InjectionRelease(release);
    h.driver.snapshots.mode_switch_in_callback = immediate;
    let local = h.driver.local;
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
    h.driver.command(InputCommand::LocalDisplays(vec![display.clone()]));
    let layout = Arc::new(oink_display::layout::Layout::auto(&[
        (local, vec![display.clone()]),
        (peer(), vec![display]),
    ]));
    h.driver.snapshots.layout.publish(RouterLayout {
        group: oink_foundation::GroupId([0; 16]),
        owners: Arc::default(),
        version: fixture_version(),
        topology: oink_routing::TopologyLayout::from_layout(&layout, &Default::default()),
        layout,
        devices: BTreeSet::from([local, peer()]),
    });
    h.driver.refresh();
    let (mut read, _datagrams) = h.outbound(1);
    let (write, _motion) = h.inbound(1);
    opening(&write, &remote(), true);
    h.attach();
    drain(&mut read);
    if local_mouse {
        let mut settings = (*h.driver.settings).clone();
        settings.mouse_mode = oink_foundation::input::InputMode::Local;
        h.driver.snapshots.settings.publish(settings);
        h.driver.refresh();
        // Incoming motion requires an accepted, scoped group focus record.
        // A fresh instance at epoch zero intentionally rejects remote input.
        h.driver.message(
            peer(),
            Message::Control(ControlMsg::FocusEnter {
                epoch: 1,
                display: "screen".into(),
                pos: Point::new(995., 500.),
                scope: Some(input_scope(&h.driver)),
                pointer_source: Some(peer()),
            }),
        );
    }

    // A slow native injection must not delay the mode change on the hook thread.
    h.driver.inject.push(InjectItem::Inject {
        event: InjectEvent::MoveRelative { dx: 0., dy: 0. },
        from: Some(peer()),
    });
    entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    backend.modes.lock().unwrap().clear();
    // A return from the previous crossing is still queued when the user leaves again.
    h.driver.inject.push(InjectItem::RestoreLocal {
        at: Point::new(990., 500.),
    });
    backend.fake.set_cursor(Point::new(995., 500.));
    h.driver.adapter.router_mut().set_cursor(Point::new(995., 500.));
    for seq in 0..20 {
        if local_mouse {
            write
                .post(encode(&Message::Input(InputMsg::Motion { seq, dx: 30., dy: 0. })).unwrap())
                .unwrap();
            h.poll();
        } else {
            h.driver.event(&CapturedEvent {
                physical: true,
                event: InputEvent::PointerDelta { dx: 100., dy: 0. },
                source: SourceKind::Mouse,
                time_us: 0,
                cursor: Some(Point::new(999., 500.)),
            });
        }
        if h.driver.adapter.router().focus().holder == peer() {
            break;
        }
    }
    assert_eq!(h.driver.adapter.router().focus().holder, peer());
    assert!(
        drain(&mut read)
            .iter()
            .any(|m| matches!(m, Message::Control(ControlMsg::FocusEnter { .. })))
    );
    let mode = backend.fake.mode();
    let changes = backend.modes.lock().unwrap().clone();
    if immediate && !local_mouse {
        assert!(
            matches!(mode, Some(CaptureMode::Remote { .. })),
            "FocusEnter was posted and capture returned, but native mode is {mode:?}"
        );
        assert_eq!(changes.len(), 1);
        assert_eq!(
            changes[0].1,
            std::thread::current().id(),
            "handoff must use the capture owner"
        );
        let Some(CaptureMode::Remote { park }) = mode else {
            unreachable!()
        };
        assert_eq!(backend.cursor_position().unwrap(), park);
    } else {
        assert_eq!(mode, Some(CaptureMode::Local));
        assert!(changes.is_empty(), "callback mode option was ignored");
    }

    release.send(()).unwrap();
    h.driver.inject.push(InjectItem::ReleaseAll { peer: peer() });
    let until = Instant::now() + Duration::from_secs(2);
    while h.driver.inject.take_released().is_none() {
        assert!(Instant::now() < until, "injection barrier did not complete");
        std::thread::yield_now();
    }
    let Some(CaptureMode::Remote { park }) = backend.fake.mode() else {
        panic!("completed handoff is not Remote");
    };
    assert_eq!(backend.cursor_position().unwrap(), park);
    if immediate && !local_mouse {
        assert_eq!(
            backend.fake.mode(),
            mode,
            "older restore must not show the parked cursor"
        );
        assert_eq!(
            *backend.modes.lock().unwrap(),
            changes,
            "stale restore reached the native backend"
        );
    } else {
        let changes = backend.modes.lock().unwrap();
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0].0, PendingMode::RestoreLocal(Point::new(990., 500.)));
        assert_eq!(changes[1].0, PendingMode::Set(CaptureMode::Remote { park }));
        assert_ne!(changes[0].1, std::thread::current().id());
        assert_eq!(changes[0].1, changes[1].1);
    }
    if local_mouse {
        let focus = h.driver.adapter.router().focus();
        drain(&mut read);
        assert_eq!(
            h.driver.event(&CapturedEvent {
                physical: true,
                event: InputEvent::PointerDelta { dx: -5., dy: 2. },
                source: SourceKind::Mouse,
                time_us: 0,
                cursor: None,
            }),
            Disposition::Pass
        );
        assert_eq!(backend.fake.mode(), Some(CaptureMode::Local));
        assert_eq!(h.driver.adapter.router().focus(), focus);
        assert!(!drain(&mut read).iter().any(|m| matches!(
            m,
            Message::Input(_) | Message::TimedInput { .. } | Message::Control(ControlMsg::FocusEnter { .. })
        )));
    }
}

#[test]
fn owner_apply_ack_waits_for_native_work_and_publishes_owner_preference() {
    let (release, blocked) = mpsc::channel();
    let (entered, entered_rx) = mpsc::channel();
    let backend = Arc::new(RecordingInput {
        fake: oink_test_support::input::FakeInput::new(),
        release: Mutex::new(Some(blocked)),
        entered,
        modes: Mutex::new(Vec::new()),
    });
    backend.start(Arc::new(|_| Disposition::Pass)).unwrap();
    let mut h = Harness::with_backend(backend);
    let release = InjectionRelease(release);
    h.driver.inject.push(InjectItem::Inject {
        event: InjectEvent::MoveRelative { dx: 0., dy: 0. },
        from: None,
    });
    entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let (reply, applied) = mpsc::sync_channel(1);
    let mut routing = (*h.driver.settings).clone();
    routing.game_mode_preference = GameMode::On;
    routing.mouse_mode = oink_foundation::input::InputMode::Local;
    h.driver.command(InputCommand::ApplyOwnerPreferences {
        expected_emergency_sequence: 0,
        routing,
        keyboard_disabled: true,
        input_transform: oink_foundation::input_transform::InputTransform::initial("macos"),
        accept_contacts: false,
        reply,
    });
    assert!(matches!(applied.try_recv(), Err(mpsc::TryRecvError::Empty)));
    let status = h.driver.shared.status.load();
    assert!(status.keyboard_disabled);
    assert_eq!(status.mouse_mode, oink_foundation::input::InputMode::Local);
    assert_eq!(
        h.driver.adapter.router().config().settings.game_mode_preference,
        GameMode::On
    );
    release.send(()).unwrap();
    let until = Instant::now() + Duration::from_secs(2);
    loop {
        h.poll();
        if let Ok(result) = applied.try_recv() {
            assert_eq!(
                result.unwrap(),
                super::super::super::OwnerInputApplied {
                    cleanup_pending: false,
                    emergency_sequence: status.emergency_sequence,
                    local_emergency_sequence: status.local_emergency_sequence
                }
            );
            break;
        }
        assert!(Instant::now() < until);
        std::thread::yield_now();
    }
}
