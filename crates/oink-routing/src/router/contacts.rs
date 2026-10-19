//! Contacts never migrate across a focus boundary. Receiver ownership is one
//! source/device pair; a second source cannot splice its fingers into that device.
use super::*;
use oink_foundation::contacts::{Frame, Phase};

/// Captured gesture routing evidence, recorded once per gesture phase boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ContactsPath {
    NativeContacts,
    CaptureUnavailable,
    AwaitLift,
    Local,
}

impl Router {
    /// Owner preference on the receiving Windows device. It never gates capture
    /// or the advertised native injector capability.
    pub fn set_accept_contacts(&mut self, enabled: bool) {
        if self.accept_contacts != enabled {
            self.retire_received_multifinger();
            self.accept_contacts = enabled;
        }
    }
    pub fn contacts_path(&self) -> ContactsPath {
        if !matches!(self.mouse_target(), Target::Remote(_)) {
            return ContactsPath::Local;
        }
        if self.contacts.mode() == crate::contacts::Mode::AwaitLift {
            return ContactsPath::AwaitLift;
        }
        if !self.contacts_observed || !self.contacts_healthy {
            return ContactsPath::CaptureUnavailable;
        }
        ContactsPath::NativeContacts
    }
    pub fn set_touchpad_contacts(&mut self, capacity: Option<u8>) {
        let capacity = capacity.filter(|n| (2..=5).contains(n));
        if self.touchpad_contacts != capacity {
            self.retire_received_multifinger();
            self.touchpad_contacts = capacity;
        }
    }
    pub fn set_peer_touchpad_contacts(&mut self, peer: DeviceId, capacity: Option<u8>) {
        if let Some(p) = self.peers.get_mut(&peer) {
            p.touchpad_contacts = capacity.filter(|n| (2..=5).contains(n));
        }
    }
    fn contacts_destination(&self) -> Option<DeviceId> {
        if self.cfg.settings.forward_gestures && !self.paused && self.capture_available {
            match self.mouse_target() {
                Target::Remote(d) => Some(d),
                _ => None,
            }
        } else {
            None
        }
    }
    pub(super) fn contacts_route(&mut self, ev: &CapturedEvent) -> Option<Disposition> {
        let target = self.contacts_destination();
        if self.contacts_target != target {
            if self.contacts_observed {
                self.source_contacts_boundary(true);
            } else {
                self.cancel_source_contacts();
            }
            self.contacts_target = target;
        }
        let old = self.contacts.mode();
        let before = self.contacts.raw_frames;
        if let InputEvent::TouchpadSample(sample) = ev.event {
            if !sample.valid() {
                self.contacts_observed = true;
                self.contacts_healthy = false;
            } else if !self.contacts_observed || !self.contacts_healthy && sample.len == 0 {
                // MultitouchSupport keeps reporting after a corrupt/overflowed
                // frame. A validated full lift starts a fresh touch; keeping the
                // fault latched for the process lifetime suppresses every future
                // native gesture (observed on macOS 26.6.2).
                self.contacts_observed = true;
                self.contacts_healthy = true;
            }
            if let Some(to) = target.filter(|_| self.contacts_healthy) {
                let out = &mut self.out;
                self.contacts.sample(sample, |frame| {
                    out.push(Output::Send {
                        to,
                        msg: Message::Input(InputMsg::TouchpadContacts(frame)),
                        delivery: Delivery::Reliable,
                    })
                });
            } else {
                if !sample.valid() {
                    // A raw capture fault cancels only contacts. The OS gesture
                    // stream must still deliver its own End/Cancel to the target.
                    self.cancel_source_contacts();
                }
                self.contacts.ignore_sample(sample);
            }
            self.contacts_diagnostic(old, before);
            return Some(if target.is_some() {
                Disposition::Swallow
            } else {
                Disposition::Pass
            });
        }
        // Native samples and system gestures each retain their own lifecycle.
        // In particular, a scroll recognizer must not suppress native contacts,
        // and a native gesture must not suppress its system-event counterpart.
        None
    }
    fn contacts_diagnostic(&mut self, old: crate::contacts::Mode, before: u64) {
        let mode = self.contacts.mode();
        if mode != old {
            self.out.push(Output::Status(StatusEvent::TouchpadContacts {
                mode,
                frames: self.contacts.raw_frames,
                gestures: self.contacts.gestures,
                cancelled: self.contacts.cancelled,
                ended: mode == crate::contacts::Mode::Ready && self.contacts.raw_frames != before,
            }));
        }
    }
    pub(super) fn cancel_source_contacts(&mut self) {
        self.source_contacts_boundary(false);
    }
    fn source_contacts_boundary(&mut self, preserve_scroll: bool) {
        let old = self.contacts.mode();
        let before = self.contacts.raw_frames;
        let target = self.contacts_target.take();
        let out = &mut self.out;
        let emit = |frame| {
            if let Some(to) = target {
                out.push(Output::Send {
                    to,
                    msg: Message::Input(InputMsg::TouchpadContacts(frame)),
                    delivery: Delivery::Reliable,
                });
            }
        };
        if preserve_scroll {
            self.contacts.reconfigure(emit);
        } else {
            self.contacts.reset(emit);
        }
        self.contacts_diagnostic(old, before);
    }
    pub(super) fn cancel_received_contacts(&mut self) {
        if let Some((_, device)) = self.contacts_received.take() {
            self.out
                .push(Output::Inject(InjectEvent::TouchpadContacts(Frame::cancel(device))));
        }
    }
    pub(super) fn receive_contacts(&mut self, origin: DeviceId, frame: Frame) {
        // No forwarding: focus handoff cancels the old target locally and at source.
        if self.holder != self.me() || self.claim != self.me() {
            // Focus retirement already cancelled native injection. Still consume
            // the source's End/Cancel: it is addressed to this old receiver and
            // may arrive after the focus announcement. Leaving its contact bit
            // retired would discard the next touch when focus returns here.
            if frame.phase.ends() {
                self.filter_multifinger(origin, MultiEvent::Contacts(frame.phase));
            }
            return;
        }
        if !self.filter_multifinger(origin, MultiEvent::Contacts(frame.phase)) {
            return;
        }
        if self.touchpad_contacts.is_none() {
            // Windows with native contacts selected must report the unavailable
            // injector; system gestures cannot silently become a fallback.
            return;
        }
        if frame.phase == Phase::Begin {
            if self.contacts_received.is_some() || self.paused {
                return;
            }
            self.contacts_received = Some((origin, frame.device));
        }
        if self.contacts_received != Some((origin, frame.device)) {
            return;
        }
        if frame.phase.ends() {
            self.contacts_received = None;
        }
        self.out.push(Output::Inject(InjectEvent::TouchpadContacts(frame)));
    }
}

/// A target keeps the selected path until both independent lifecycles finish.
/// Retired segments still consume their ends but never inject a surviving tail.
#[derive(Debug)]
pub(super) struct MultiFinger {
    native: bool,
    pub(super) retired: bool,
    contacts: bool,
    gesture: bool,
    scroll: Option<ScrollRoute<()>>,
}
#[derive(Clone, Copy)]
pub(super) enum MultiEvent {
    Contacts(Phase),
    Gesture(Gesture),
    Scroll(ScrollPhase),
}
impl Router {
    pub(super) fn filter_multifinger(&mut self, origin: DeviceId, event: MultiEvent) -> bool {
        let starts = match event {
            MultiEvent::Contacts(p) => p == Phase::Begin,
            MultiEvent::Gesture(g) => g.starts(),
            MultiEvent::Scroll(p) => matches!(p, ScrollPhase::Began | ScrollPhase::None),
        };
        if starts {
            self.multifinger.entry(origin).or_insert(MultiFinger {
                native: self.cfg.os == Os::Windows && self.accept_contacts,
                retired: false,
                contacts: false,
                gesture: false,
                scroll: None,
            });
        }
        let Some(state) = self.multifinger.get_mut(&origin) else {
            return false;
        };
        let live = match event {
            MultiEvent::Contacts(p) => {
                let live = starts || state.contacts;
                state.contacts = !p.ends() && live;
                live
            }
            MultiEvent::Gesture(g) => {
                let live = starts || state.gesture;
                state.gesture = !g.ends() && live;
                live
            }
            MultiEvent::Scroll(p) => {
                if p == ScrollPhase::Began && state.scroll.is_none() {
                    state.scroll = Some(ScrollRoute::new(()));
                }
                let live = starts || state.scroll.is_some();
                if state
                    .scroll
                    .as_mut()
                    .is_some_and(|r| r.phase(p) || state.retired && !r.fingers && !r.momentum)
                {
                    state.scroll = None;
                }
                live
            }
        };
        let native = matches!(event, MultiEvent::Contacts(_));
        let allow = live && !state.retired && native == state.native;
        if !state.contacts && !state.gesture && state.scroll.is_none() {
            self.multifinger.remove(&origin);
        }
        allow
    }
    pub(super) fn retire_received_multifinger(&mut self) {
        self.cancel_received_contacts();
        let mut cancel_scroll = false;
        for state in self.multifinger.values_mut() {
            // macOS may finish a short scroll with Ended and no momentum at all
            // (observed from macOS 26.6.2 to 14.7.8). While admitted, keep that
            // idle route for a possible MomentumBegan. After retirement, that
            // tail must be dropped anyway: waiting for a MomentumEnded that may
            // never arrive would poison every later gesture from this source.
            // Active fingers/momentum still retain their retired tail guards;
            // the Scroll branch above also clears them when they become idle.
            if state.scroll.is_some_and(|r| !r.fingers && !r.momentum) {
                state.scroll = None;
            }
            cancel_scroll |= !state.native && !state.retired && state.scroll.is_some();
            state.retired = true;
        }
        self.multifinger
            .retain(|_, state| state.contacts || state.gesture || state.scroll.is_some());
        if cancel_scroll {
            self.out.push(Output::Inject(InjectEvent::TouchpadScroll {
                dx: 0.,
                dy: 0.,
                pixels_per_reference: 1.,
                wheel: oink_foundation::input::ScrollInjection {
                    dx: 0.,
                    dy: 0.,
                    unit: ScrollUnit::Pixel,
                    phase: ScrollPhase::Cancelled,
                },
            }));
        }
        for (_, gesture) in self.gesture_active.drain() {
            let cancel = match gesture {
                Gesture::DockSwipe { axis, progress, .. } => Gesture::DockSwipe {
                    axis,
                    progress,
                    velocity: 0.,
                    phase: GesturePhase::Cancelled,
                },
                Gesture::Magnify { .. } => Gesture::Magnify {
                    phase: GesturePhase::Cancelled,
                    amount: 0.,
                },
                Gesture::Rotate { .. } => Gesture::Rotate {
                    phase: GesturePhase::Cancelled,
                    degrees: 0.,
                },
                Gesture::SmartMagnify => continue,
            };
            self.out.push(Output::Inject(InjectEvent::Gesture(cancel)));
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use oink_foundation::contacts::{Contact, Sample, Surface};
    fn router() -> Router {
        let mut r = Router::new(
            RouterConfig::new(
                oink_foundation::session::Boot {
                    device_id: DeviceId([1; 32]),
                    nonce: [DeviceId([1; 32]).0[0]; 16],
                },
                "test",
                Os::Windows,
            ),
            Arc::new(crate::SimpleLayout::default()),
        );
        r.epoch = 1;
        r.set_touchpad_contacts(Some(5));
        r
    }
    fn sample(time_us: u64, n: u8, x: i32) -> Sample {
        let mut s = Sample {
            device: 1,
            time_us,
            surface: Surface {
                width: 15_600,
                height: 9_600,
            },
            len: n,
            ..Default::default()
        };
        for (i, p) in s.contacts.iter_mut().enumerate().take(n as usize) {
            *p = Contact {
                id: i as u32 + 1,
                x: x + i as i32 * 1000,
                y: 2000,
            };
        }
        s
    }
    fn captured(s: Sample) -> CapturedEvent {
        CapturedEvent {
            time_us: s.time_us,
            event: InputEvent::TouchpadSample(s),
            source: SourceKind::Touchpad,
            physical: false,
            cursor: None,
        }
    }
    fn cancels(r: &Router) -> usize {
        r.out
            .iter()
            .filter(|o| matches!(o, Output::Inject(InjectEvent::TouchpadContacts(f)) if f.phase == Phase::Cancel))
            .count()
    }
    #[test]
    fn capture_fault_cancels_current_touch_and_recovers_only_after_full_lift() {
        for after_begin in [false, true] {
            let peer = DeviceId([2; 32]);
            let mut r = router();
            r.test_connect(peer, 0);
            r.holder = peer;
            r.claim = peer;
            if after_begin {
                r.on_local_event(&captured(sample(0, 0, 0)), 0);
                r.on_local_event(&captured(sample(8000, 3, 1000)), 8000);
            }
            r.on_local_event(&captured(sample(16000, 255, 0)), 16000);
            r.out.clear();
            r.on_local_event(&captured(sample(24000, 3, 1000)), 24000);
            assert!(!r.contacts_healthy);
            assert!(!r.out.iter().any(|o| matches!(
                o,
                Output::Send {
                    msg: Message::Input(InputMsg::TouchpadContacts(_)),
                    ..
                }
            )));
            r.on_local_event(&captured(sample(32000, 0, 0)), 32000);
            r.on_local_event(&captured(sample(40000, 3, 1200)), 40000);
            assert!(r.contacts_healthy, "a verified full lift ends the failed touch");
            assert!(r.out.iter().any(|o| matches!(o, Output::Send {
                msg: Message::Input(InputMsg::TouchpadContacts(f)), ..
            } if f.phase == Phase::Begin)));
        }
    }
    #[test]
    fn native_capture_fault_does_not_truncate_system_gesture() {
        let peer = DeviceId([2; 32]);
        let mut r = router();
        r.test_connect(peer, 0);
        r.holder = peer;
        r.claim = peer;
        r.on_local_event(&captured(sample(0, 0, 0)), 0);
        r.out.clear();
        for phase in [GesturePhase::Began, GesturePhase::Ended] {
            r.on_local_event(
                &CapturedEvent {
                    event: InputEvent::Gesture(Gesture::Magnify { phase, amount: 0.1 }),
                    time_us: 8000,
                    source: SourceKind::Touchpad,
                    physical: true,
                    cursor: None,
                },
                8000,
            );
            if phase == GesturePhase::Began {
                r.on_local_event(&captured(sample(9000, 255, 0)), 9000);
            }
        }
        assert_eq!(
            r.out
                .iter()
                .filter(|o| matches!(
                    o,
                    Output::Send {
                        msg: Message::Input(InputMsg::Gesture { .. }),
                        ..
                    }
                ))
                .count(),
            2,
            "a native capture fault cannot suppress the independent system gesture tail"
        );
    }

    #[test]
    fn receiver_focus_leave_disconnect_and_pause_emit_cancel_before_reuse() {
        let peer = DeviceId([2; 32]);
        for boundary in 0..3 {
            let mut r = router();
            r.test_connect(peer, 0);
            r.receive_contacts(peer, Frame::between(None, &sample(0, 3, 1000)));
            r.out.clear();
            match boundary {
                0 => r.become_source(2, peer),
                1 => r.detach_input(peer),
                _ => r.set_paused(true),
            }
            assert_eq!(cancels(&r), 1);
            assert!(r.contacts_received.is_none());
            r.out.clear();
            r.release_origin(peer);
            assert_eq!(cancels(&r), 0);
        }
    }
    #[test]
    fn contact_cancel_after_focus_leave_releases_retired_segment_before_next_touch() {
        let origin = DeviceId([2; 32]);
        let mut r = router();
        r.test_connect(origin, 0);
        r.receive_contacts(origin, Frame::between(None, &sample(0, 3, 1000)));
        r.become_source(2, origin);
        r.out.clear();
        // The source sends this cancellation to the old receiver, after that
        // receiver has already learned that focus is elsewhere.
        r.receive_contacts(origin, Frame::cancel(1));
        assert!(r.out.is_empty(), "a retired tail cannot inject on the old target");
        r.become_holder(3, None);
        r.claim = r.me();
        r.out.clear();
        r.receive_contacts(origin, Frame::between(None, &sample(16000, 3, 2000)));
        assert!(
            r.out.iter().any(|out| matches!(out,
                Output::Inject(InjectEvent::TouchpadContacts(frame)) if frame.phase == Phase::Begin
            )),
            "the next complete contact gesture must be accepted after focus returns"
        );
    }

    #[test]
    fn receiver_rejects_second_owner_and_never_forwards_raw_to_next_holder() {
        let mut r = router();
        let a = DeviceId([2; 32]);
        let b = DeviceId([3; 32]);
        r.receive_contacts(a, Frame::between(None, &sample(0, 3, 1000)));
        r.out.clear();
        r.receive_contacts(b, Frame::between(None, &sample(0, 3, 2000)));
        assert!(r.out.is_empty());
        r.become_source(2, b);
        r.out.clear();
        r.receive_contacts(a, Frame::between(Some(&sample(0, 3, 1000)), &sample(8000, 3, 1200)));
        assert!(r.out.is_empty());
    }
    #[test]
    fn source_focus_boundary_cancels_then_waits_for_full_lift() {
        let peer = DeviceId([2; 32]);
        let next = DeviceId([3; 32]);
        let mut r = router();
        for id in [peer, next] {
            r.test_connect(id, 0);
            r.set_peer_touchpad_contacts(id, Some(5));
        }
        r.holder = peer;
        r.claim = peer;
        for s in [sample(0, 0, 0), sample(8000, 3, 1000), sample(16000, 3, 1100)] {
            r.on_local_event(&captured(s), s.time_us);
        }
        r.out.clear();
        r.become_source(2, next);
        assert!(r.out.iter().any(|o| matches!(o,Output::Send {to,msg:Message::Input(InputMsg::TouchpadContacts(f)),..} if *to==peer && f.phase==Phase::Cancel)));
        r.out.clear();
        r.on_local_event(&captured(sample(24000, 1, 1300)), 24000);
        assert!(!r.out.iter().any(|o| matches!(
            o,
            Output::Send {
                msg: Message::Input(InputMsg::TouchpadContacts(_)),
                ..
            }
        )));
    }

    #[test]
    fn source_sends_both_paths_independent_of_receiver_capacity_and_local_preference() {
        for capacity in [None, Some(5)] {
            for accept in [false, true] {
                let mut source = router();
                let peer = DeviceId([2; 32]);
                source.test_connect(peer, 0);
                source.set_peer_touchpad_contacts(peer, capacity);
                source.set_accept_contacts(accept);
                source.holder = peer;
                source.claim = peer;
                source.on_local_event(&captured(sample(0, 0, 0)), 0);
                source.out.clear();
                source.on_local_event(&captured(sample(8000, 2, 1000)), 8000);
                for phase in [GesturePhase::Began, GesturePhase::Changed, GesturePhase::Ended] {
                    source.on_local_event(
                        &CapturedEvent {
                            event: InputEvent::Gesture(Gesture::Magnify { phase, amount: 0.1 }),
                            source: SourceKind::Touchpad,
                            time_us: 9000,
                            cursor: None,
                            physical: true,
                        },
                        9000,
                    );
                }
                source.on_local_event(&captured(sample(16000, 0, 0)), 16000);
                let sent: Vec<_> = source
                    .drain_outputs()
                    .into_iter()
                    .filter_map(|o| match o {
                        Output::Send {
                            msg: Message::Input(m),
                            delivery: Delivery::Reliable,
                            ..
                        } => Some(m),
                        _ => None,
                    })
                    .collect();
                assert_eq!(
                    sent.iter()
                        .filter(|m| matches!(m, InputMsg::TouchpadContacts(_)))
                        .count(),
                    2
                );
                assert_eq!(sent.iter().filter(|m| matches!(m, InputMsg::Gesture { .. })).count(), 3);
            }
        }
    }
    #[test]
    fn target_selects_exactly_one_path_and_keeps_capability_when_disabled() {
        let origin = DeviceId([2; 32]);
        for os in [Os::Windows, Os::MacOS] {
            for accept in [false, true] {
                let mut r = router();
                r.cfg.os = os;
                r.set_accept_contacts(accept);
                let native = os == Os::Windows && accept;
                assert_eq!(r.filter_multifinger(origin, MultiEvent::Contacts(Phase::Begin)), native);
                assert_eq!(
                    r.filter_multifinger(origin, MultiEvent::Scroll(ScrollPhase::Began)),
                    !native
                );
                assert_eq!(
                    r.filter_multifinger(origin, MultiEvent::Scroll(ScrollPhase::Ended)),
                    !native
                );
                assert_eq!(r.filter_multifinger(origin, MultiEvent::Contacts(Phase::End)), native);
                assert_eq!(
                    r.filter_multifinger(origin, MultiEvent::Scroll(ScrollPhase::MomentumBegan)),
                    !native
                );
                assert_eq!(
                    r.filter_multifinger(origin, MultiEvent::Scroll(ScrollPhase::MomentumEnded)),
                    !native
                );
                assert!(r.multifinger.is_empty());
                assert!(matches!(
                    &r.input_opening(true)[0],
                    Message::Control(ControlMsg::InputCapability {
                        touchpad_contacts: Some(5),
                        ..
                    })
                ));
            }
        }
    }
    #[test]
    fn preference_change_cancels_and_retires_both_tails_until_next_touch() {
        let origin = DeviceId([2; 32]);
        let mut r = router();
        r.receive_contacts(origin, Frame::between(None, &sample(0, 2, 1000)));
        assert!(!r.filter_multifinger(origin, MultiEvent::Scroll(ScrollPhase::Began)));
        r.out.clear();
        r.set_accept_contacts(false);
        assert_eq!(cancels(&r), 1);
        assert!(!r.filter_multifinger(origin, MultiEvent::Scroll(ScrollPhase::Changed)));
        assert!(!r.filter_multifinger(origin, MultiEvent::Contacts(Phase::End)));
        assert!(!r.filter_multifinger(origin, MultiEvent::Scroll(ScrollPhase::Ended)));
        assert!(!r.filter_multifinger(origin, MultiEvent::Scroll(ScrollPhase::MomentumBegan)));
        assert!(!r.filter_multifinger(origin, MultiEvent::Scroll(ScrollPhase::MomentumEnded)));
        assert!(r.filter_multifinger(origin, MultiEvent::Scroll(ScrollPhase::Began)));
    }
    #[test]
    fn mac_gesture_after_focus_roundtrip_does_not_wait_for_optional_scroll_momentum() {
        let origin = DeviceId([2; 32]);
        for end_before_focus_leave in [false, true] {
            let mut r = router();
            r.cfg.os = Os::MacOS;
            r.test_connect(origin, 0);
            assert!(r.filter_multifinger(origin, MultiEvent::Scroll(ScrollPhase::Began)));
            if end_before_focus_leave {
                assert!(r.filter_multifinger(origin, MultiEvent::Scroll(ScrollPhase::Ended)));
            }
            r.become_source(2, origin);
            if !end_before_focus_leave {
                assert!(!r.filter_multifinger(origin, MultiEvent::Scroll(ScrollPhase::Ended)));
            }
            r.become_holder(3, None);
            r.out.clear();
            for phase in [GesturePhase::Began, GesturePhase::Changed, GesturePhase::Ended] {
                let gesture = Gesture::DockSwipe {
                    axis: oink_foundation::input::SwipeAxis::Pinch,
                    phase,
                    progress: -0.5,
                    velocity: 0.,
                };
                r.remote_gesture(
                    origin,
                    0,
                    gesture,
                    InputMsg::Gesture {
                        source: SourceKind::Touchpad,
                        gesture,
                    },
                );
            }
            assert_eq!(
                r.out
                    .iter()
                    .filter(|o| matches!(o, Output::Inject(InjectEvent::Gesture(_))))
                    .count(),
                3
            );
            // A late tail from the old scroll must stay discarded.
            assert!(!r.filter_multifinger(origin, MultiEvent::Scroll(ScrollPhase::MomentumBegan)));
            assert!(!r.filter_multifinger(origin, MultiEvent::Scroll(ScrollPhase::MomentumEnded)));
        }
    }

    #[test]
    fn scroll_tail_after_focus_leave_releases_filter_through_input_dispatch() {
        let origin = DeviceId([2; 32]);
        for os in [Os::Windows, Os::MacOS] {
            for phase in [ScrollPhase::Ended, ScrollPhase::Cancelled] {
                let mut r = router();
                r.cfg.os = os;
                r.test_connect(origin, 0);
                r.focus_scope = Some(r.new_focus_scope());
                let scroll = |phase| InputMsg::Scroll {
                    source: SourceKind::Touchpad,
                    dx: 0.,
                    dy: 1.,
                    unit: ScrollAmount::Reference,
                    phase,
                };
                r.on_input(origin, origin, 0, scroll(ScrollPhase::Began), 0);
                assert!(r.multifinger.contains_key(&origin));
                r.become_source(2, origin);
                r.out.clear();
                // Focus retirement clears scroll_fwd before the source's ordered
                // scroll cancellation can reach the old target.
                r.on_input(origin, origin, 0, scroll(phase), 8000);
                assert!(r.out.is_empty(), "retired scroll tails cannot inject or forward");
                r.become_holder(3, None);
                r.claim = r.me();
                r.out.clear();
                if os == Os::Windows {
                    r.on_input(
                        origin,
                        origin,
                        0,
                        InputMsg::TouchpadContacts(Frame::between(None, &sample(16000, 3, 2000))),
                        16000,
                    );
                    assert!(
                        r.out
                            .iter()
                            .any(|o| matches!(o, Output::Inject(InjectEvent::TouchpadContacts(_)))),
                        "new native contact gesture must inject after old scroll retires"
                    );
                } else {
                    r.on_input(origin, origin, 0, scroll(ScrollPhase::Began), 16000);
                    assert!(
                        r.out.iter().any(|o| matches!(
                            o,
                            Output::Inject(InjectEvent::TouchpadScroll { .. } | InjectEvent::Scroll { .. })
                        )),
                        "new system scroll must inject after old scroll retires"
                    );
                }
            }
        }
    }

    #[test]
    fn source_focus_retirement_closes_scroll_at_old_receiver_before_next_contacts() {
        let source_id = DeviceId([1; 32]);
        let target_id = DeviceId([2; 32]);
        for momentum in [false, true] {
            let mut source = router();
            source.cfg.os = Os::MacOS;
            source.test_connect(target_id, 0);
            source.holder = target_id;
            source.claim = target_id;
            let mut target = Router::new(
                RouterConfig::new(
                    oink_foundation::session::Boot {
                        device_id: target_id,
                        nonce: [2; 16],
                    },
                    "test",
                    Os::Windows,
                ),
                Arc::new(crate::SimpleLayout::default()),
            );
            target.epoch = 1;
            target.set_touchpad_contacts(Some(5));
            target.test_connect(source_id, 0);
            target.focus_scope = Some(target.new_focus_scope());
            let deliver = |source: &mut Router, target: &mut Router| {
                for out in source.drain_outputs() {
                    if let Output::Send {
                        to,
                        msg: Message::Input(msg),
                        ..
                    } = out
                    {
                        assert_eq!(to, target_id);
                        target.on_input(source_id, source_id, 0, msg, 0);
                    }
                }
            };
            source.local_scroll(SourceKind::Touchpad, 0., 1., ScrollUnit::Pixel, ScrollPhase::Began);
            if momentum {
                source.local_scroll(SourceKind::Touchpad, 0., 0., ScrollUnit::Pixel, ScrollPhase::Ended);
                source.local_scroll(
                    SourceKind::Touchpad,
                    0.,
                    1.,
                    ScrollUnit::Pixel,
                    ScrollPhase::MomentumBegan,
                );
            }
            deliver(&mut source, &mut target);
            target.become_source(2, source_id);
            source.out.clear();
            source.become_holder(2, None);
            assert!(
                source.out.iter().any(|o| matches!(o, Output::Send {
                to, msg: Message::Input(InputMsg::Scroll { phase: ScrollPhase::Cancelled, .. }), ..
            } if *to == target_id)),
                "source must close its old target before dropping the physical tail"
            );
            deliver(&mut source, &mut target);
            assert!(!target.multifinger.contains_key(&source_id));
            target.become_holder(3, None);
            target.claim = target_id;
            target.out.clear();
            target.on_input(
                source_id,
                source_id,
                0,
                InputMsg::TouchpadContacts(Frame::between(None, &sample(16000, 3, 2000))),
                16000,
            );
            assert!(target.out.iter().any(|o| matches!(o,
                Output::Inject(InjectEvent::TouchpadContacts(f)) if f.phase == Phase::Begin)));
        }
    }

    #[test]
    fn retirement_preserves_active_contact_and_momentum_tail_guards() {
        let origin = DeviceId([2; 32]);
        let mut r = router();
        assert!(r.filter_multifinger(origin, MultiEvent::Contacts(Phase::Begin)));
        assert!(!r.filter_multifinger(origin, MultiEvent::Scroll(ScrollPhase::Began)));
        assert!(!r.filter_multifinger(origin, MultiEvent::Scroll(ScrollPhase::Ended)));
        assert!(!r.filter_multifinger(origin, MultiEvent::Scroll(ScrollPhase::MomentumBegan)));
        r.retire_received_multifinger();
        assert!(!r.filter_multifinger(origin, MultiEvent::Contacts(Phase::Move)));
        assert!(!r.filter_multifinger(origin, MultiEvent::Contacts(Phase::End)));
        assert!(!r.filter_multifinger(origin, MultiEvent::Scroll(ScrollPhase::Momentum)));
        assert!(r.multifinger.contains_key(&origin));
        assert!(!r.filter_multifinger(origin, MultiEvent::Scroll(ScrollPhase::MomentumEnded)));
        assert!(!r.multifinger.contains_key(&origin));
        assert!(r.filter_multifinger(origin, MultiEvent::Contacts(Phase::Begin)));
    }

    #[test]
    fn unavailable_native_injector_never_falls_back_to_gestures() {
        let origin = DeviceId([2; 32]);
        let mut r = router();
        r.set_touchpad_contacts(None);
        r.out.clear();
        r.receive_contacts(origin, Frame::between(None, &sample(0, 2, 1000)));
        assert!(!r.filter_multifinger(
            origin,
            MultiEvent::Gesture(Gesture::Magnify {
                phase: GesturePhase::Began,
                amount: 0.1,
            })
        ));
        assert!(r.out.is_empty());
    }
    #[test]
    fn source_master_cancels_contacts_and_scroll_without_affecting_wheel() {
        let mut r = router();
        let peer = DeviceId([2; 32]);
        r.test_connect(peer, 0);
        r.holder = peer;
        r.claim = peer;
        r.on_local_event(&captured(sample(0, 0, 0)), 0);
        r.on_local_event(&captured(sample(8000, 2, 1000)), 8000);
        r.local_scroll(SourceKind::Touchpad, 1., 2., ScrollUnit::Pixel, ScrollPhase::Began);
        r.out.clear();
        let mut settings = r.config().settings.clone();
        settings.forward_gestures = false;
        r.set_settings(settings);
        assert!(r.out.iter().any(|o| matches!(o, Output::Send {
            msg: Message::Input(InputMsg::TouchpadContacts(f)), ..
        } if f.phase == Phase::Cancel)));
        assert!(r.out.iter().any(|o| matches!(
            o,
            Output::Send {
                msg: Message::Input(InputMsg::Scroll {
                    phase: ScrollPhase::Cancelled,
                    ..
                }),
                ..
            }
        )));
        r.out.clear();
        r.local_scroll(SourceKind::Touchpad, 1., 2., ScrollUnit::Pixel, ScrollPhase::Changed);
        r.on_local_event(&captured(sample(16000, 2, 2000)), 16000);
        assert!(r.out.is_empty());
        r.local_scroll(SourceKind::Mouse, 0., 1., ScrollUnit::Line, ScrollPhase::None);
        assert!(r.out.iter().any(|o| matches!(
            o,
            Output::Send {
                msg: Message::Input(InputMsg::Scroll {
                    source: SourceKind::Mouse,
                    ..
                }),
                ..
            }
        )));
    }
}
