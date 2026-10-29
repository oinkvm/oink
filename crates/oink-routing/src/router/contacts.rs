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
    /// Negotiated independently so older peers retain their reference-unit wire contract.
    pub fn set_peer_scroll_pixels(&mut self, peer: DeviceId, supported: bool) {
        if let Some(p) = self.peers.get_mut(&peer) {
            p.scroll_pixels = supported;
        }
    }
    pub fn set_peer_touchpad_options(&mut self, peer: DeviceId, supported: bool) {
        if let Some(p) = self.peers.get_mut(&peer) {
            p.touchpad_options = supported;
        }
    }
    pub(super) fn touchpad_options_enabled(&self, origin: DeviceId) -> bool {
        self.cfg.os == Os::Windows
            && self
                .peers
                .get(&origin)
                .is_some_and(|p| p.os == Os::MacOS && p.touchpad_options)
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
        if matches!(ev.event, InputEvent::TouchpadBoundary) {
            // Native mode commit can follow routing's focus transition. An old
            // queued zero in between cannot admit a partial touch in this epoch.
            self.source_contacts_boundary(true);
            self.contacts_target = target;
            return Some(if target.is_some() {
                Disposition::Swallow
            } else {
                Disposition::Pass
            });
        }
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
        if frame.phase == Phase::Begin {
            self.stop_synthetic_tail_on_touch(origin);
        }
        if frame.phase == Phase::Begin && self.pending_contacts.origin == Some(origin) {
            self.pending_contacts.clear();
        }
        let allow = self.filter_multifinger(origin, MultiEvent::Contacts(frame.phase));
        let scroll_only = self.touchpad_options_enabled(origin) && !self.cfg.settings.native_multifinger;
        let mut cancel = false;
        if let Some(state) = self.multifinger.get_mut(&origin) {
            if frame.phase == Phase::Begin {
                state.active_contacts = 0;
            }
            for change in frame.changes() {
                match change.phase {
                    Phase::Begin => state.active_contacts += 1,
                    Phase::End | Phase::Cancel => state.active_contacts = state.active_contacts.saturating_sub(1),
                    Phase::Move => {}
                }
            }
            // Never submit a third finger to Windows in scroll-only mode. A
            // lift back to two cannot resume a partially injected touch.
            if scroll_only && state.active_contacts >= 3 && state.path == MultiPath::NativeScroll {
                state.path = MultiPath::MappedGestures;
                cancel = true;
            }
        }
        if cancel && self.contacts_received.is_some_and(|(owner, _)| owner == origin) {
            self.cancel_received_contacts();
        }
        if self
            .multifinger
            .get(&origin)
            .is_some_and(|s| s.path == MultiPath::Pending && !s.retired)
            || self.pending_contacts.origin == Some(origin)
        {
            let pending = &mut self.pending_contacts;
            if pending.origin.is_some_and(|p| p != origin) {
                return;
            }
            pending.origin = Some(origin);
            if frame.phase.ends() || pending.len == pending.frames.len() {
                pending.clear();
                if let Some(state) = self.multifinger.get_mut(&origin) {
                    state.path = MultiPath::Suppressed;
                }
                return;
            }
            for change in frame.changes() {
                match change.phase {
                    Phase::Begin => pending.active += 1,
                    Phase::End | Phase::Cancel => pending.active = pending.active.saturating_sub(1),
                    Phase::Move => {}
                }
            }
            pending.frames[pending.len] = frame;
            pending.len += 1;
            if pending.active >= 3 {
                let path = if scroll_only {
                    MultiPath::MappedGestures
                } else {
                    MultiPath::Native
                };
                self.multifinger.get_mut(&origin).expect("pending sequence").path = path;
                self.resolve_pending_contacts(origin, path);
            }
            return;
        }
        if !allow || cancel {
            return;
        }
        self.inject_received_contact(origin, frame);
    }
    fn inject_received_contact(&mut self, origin: DeviceId, frame: Frame) {
        if self.touchpad_contacts.is_none() {
            // Runtime capability changes retire the old sequence. New sequences
            // select the system gesture path while injection is unavailable.
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

/// Selection belongs to the current physical touch. System scroll fingers and
/// momentum keep their own selections when a subsequent touch starts.
#[derive(Debug)]
pub(super) struct MultiFinger {
    path: MultiPath,
    pub(super) retired: bool,
    contacts: bool,
    active_contacts: usize,
    gesture: bool,
    prelude: bool,
    scroll: Option<ScrollPaths>,
}
/// Evidence-based target scroll selection: no native side effect before system
/// Scroll/Gesture evidence (or three active contacts) selects a whole sequence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MultiPath {
    Native,
    /// Physical contacts admitted only after system two-finger scroll evidence.
    NativeScroll,
    /// Non-scroll semantic mapping after a scroll-only touch is classified.
    MappedGestures,
    Gestures,
    Pending,
    Scroll,
    Suppressed,
}
#[derive(Clone, Copy, Debug)]
struct Selection {
    path: MultiPath,
    synthetic_scroll: bool,
    retired: bool,
}
impl Selection {
    fn scroll(self) -> bool {
        !self.retired && matches!(self.path, MultiPath::Gestures | MultiPath::Scroll)
    }
}

/// Ended may have no momentum, or MomentumBegan may arrive after a new contact
/// Begin. Keep that optional tail without reserving the next physical touch.
/// These are the independent phases of the ordered OS scroll stream, not timers.
#[derive(Debug, Default)]
struct ScrollPaths {
    fingers: Option<Selection>,
    pending: Option<Selection>,
    momentum: Option<Selection>,
}
impl ScrollPaths {
    fn owner(&self, phase: ScrollPhase) -> Option<Selection> {
        match phase {
            ScrollPhase::MomentumBegan => self.pending,
            ScrollPhase::Momentum | ScrollPhase::MomentumEnded => self.momentum,
            _ => self.fingers.or(self.pending),
        }
    }
    fn phase(&mut self, phase: ScrollPhase, current: Selection) -> Option<Selection> {
        match phase {
            ScrollPhase::Began => {
                self.fingers = Some(current);
                self.fingers
            }
            ScrollPhase::Ended => {
                let owner = self.fingers.take();
                // A retired idle route cannot start another momentum sequence.
                self.pending = owner.filter(|s| !s.retired);
                owner
            }
            ScrollPhase::Cancelled => self.fingers.take().or_else(|| self.pending.take()),
            ScrollPhase::MomentumBegan => {
                self.momentum = self.pending.take();
                self.momentum
            }
            ScrollPhase::MomentumEnded => self.momentum.take(),
            ScrollPhase::Momentum => self.momentum,
            ScrollPhase::Changed => self.fingers,
            ScrollPhase::None => Some(current),
        }
    }
    fn stop_synthetic_tail(&mut self, prelude: bool) -> bool {
        // A new scroll can precede its raw Begin. Its Began already stopped the
        // old pan; this late contact frame must not cancel the new finger scroll.
        // Without that prelude, fingers can still name the OLD scroll whose
        // Ended trails the physical lift/new Begin in the ordered source stream.
        if prelude && self.fingers.is_some() {
            return false;
        }
        let mut cancel = false;
        for owner in self
            .fingers
            .iter_mut()
            .chain(&mut self.pending)
            .chain(&mut self.momentum)
        {
            if owner.scroll() && owner.synthetic_scroll {
                owner.retired = true;
                cancel = true;
            }
        }
        cancel
    }
    fn empty(&self) -> bool {
        self.fingers.is_none() && self.pending.is_none() && self.momentum.is_none()
    }
    fn retire(&mut self) -> bool {
        self.pending = None;
        let mut cancel = false;
        for owner in self.fingers.iter_mut().chain(&mut self.momentum) {
            cancel |= owner.scroll() && owner.synthetic_scroll;
            owner.retired = true;
        }
        cancel
    }
}
const CLASSIFY_FRAMES: usize = 256;
pub(super) struct PendingContacts {
    pub(super) origin: Option<DeviceId>,
    frames: Box<[Frame; CLASSIFY_FRAMES]>,
    len: usize,
    active: usize,
}
impl Default for PendingContacts {
    fn default() -> Self {
        Self {
            origin: None,
            frames: Box::new([Frame::default(); CLASSIFY_FRAMES]),
            len: 0,
            active: 0,
        }
    }
}
impl PendingContacts {
    pub(super) fn clear(&mut self) {
        self.origin = None;
        self.len = 0;
        self.active = 0;
    }
}
#[derive(Clone, Copy)]
pub(super) enum MultiEvent {
    Contacts(Phase),
    Gesture(Gesture),
    Scroll(ScrollPhase),
}
impl Router {
    fn resolve_pending_contacts(&mut self, origin: DeviceId, path: MultiPath) {
        if self.pending_contacts.origin != Some(origin) {
            return;
        }
        let len = self.pending_contacts.len;
        self.pending_contacts.clear();
        if matches!(path, MultiPath::Native | MultiPath::NativeScroll) {
            for i in 0..len {
                self.inject_received_contact(origin, self.pending_contacts.frames[i]);
            }
        }
    }
    pub(super) fn filter_multifinger(&mut self, origin: DeviceId, event: MultiEvent) -> bool {
        let starts = match event {
            MultiEvent::Contacts(p) => p == Phase::Begin,
            MultiEvent::Gesture(g) => g.starts(),
            MultiEvent::Scroll(p) => matches!(p, ScrollPhase::Began | ScrollPhase::None),
        };
        let options = self.touchpad_options_enabled(origin);
        let scroll_only = options && !self.cfg.settings.native_multifinger;
        let native_scroll_only = scroll_only && !self.cfg.settings.mac_scroll_feel;
        // Pinch/rotation may follow scrolling in the same physical touch. End
        // the native pan before the semantic mapper runs; do not replay contacts.
        if native_scroll_only
            && matches!(event, MultiEvent::Gesture(g) if g.starts())
            && self
                .multifinger
                .get(&origin)
                .is_some_and(|s| s.path == MultiPath::NativeScroll && !s.retired)
        {
            self.multifinger.get_mut(&origin).expect("native scroll").path = MultiPath::MappedGestures;
            if self.contacts_received.is_some_and(|(owner, _)| owner == origin) {
                self.cancel_received_contacts();
            }
        }
        if starts {
            let native = self.cfg.os == Os::Windows
                && (if options {
                    self.cfg.settings.touchpad_injection && (!scroll_only || native_scroll_only)
                } else {
                    self.accept_contacts
                })
                && self.touchpad_contacts.is_some();
            let classify = native
                && (self.cfg.settings.mac_scroll_feel || native_scroll_only)
                && self.peers.get(&origin).is_some_and(|p| p.os == Os::MacOS);
            let path = if !native {
                MultiPath::Gestures
            } else if classify {
                match event {
                    MultiEvent::Contacts(_) => MultiPath::Pending,
                    MultiEvent::Scroll(_) => {
                        if native_scroll_only {
                            MultiPath::NativeScroll
                        } else {
                            MultiPath::Scroll
                        }
                    }
                    MultiEvent::Gesture(_) => {
                        if scroll_only {
                            MultiPath::MappedGestures
                        } else {
                            MultiPath::Native
                        }
                    }
                }
            } else {
                MultiPath::Native
            };
            let state = self.multifinger.entry(origin).or_insert(MultiFinger {
                path,
                retired: false,
                contacts: false,
                active_contacts: 0,
                gesture: false,
                prelude: false,
                scroll: None,
            });
            if matches!(event, MultiEvent::Contacts(Phase::Begin)) {
                // Scroll/Gesture can precede its contact Begin in the source's
                // ordered stream. That is evidence for this touch, whereas an
                // earlier touch's retained momentum is never such evidence.
                if !state.prelude {
                    state.path = path;
                    state.retired = false;
                }
                state.prelude = false;
            } else if !state.contacts {
                state.path = path;
                state.retired = false;
                state.prelude = true;
            }
        }
        let Some(state) = self.multifinger.get_mut(&origin) else {
            return false;
        };
        if state.path == MultiPath::Pending && !state.retired && starts {
            state.path = match event {
                MultiEvent::Scroll(_) => {
                    if native_scroll_only {
                        MultiPath::NativeScroll
                    } else {
                        MultiPath::Scroll
                    }
                }
                MultiEvent::Gesture(_) => {
                    if scroll_only {
                        MultiPath::MappedGestures
                    } else {
                        MultiPath::Native
                    }
                }
                _ => MultiPath::Pending,
            };
        }
        let path = state.path;
        let live = match event {
            MultiEvent::Contacts(p) => {
                let live = starts || state.contacts;
                state.contacts = !p.ends() && live;
                if p.ends() {
                    state.prelude = false;
                }
                live
            }
            MultiEvent::Gesture(g) => {
                let live = starts || state.gesture;
                state.gesture = !g.ends() && live;
                live
            }
            MultiEvent::Scroll(p) => {
                let current = Selection {
                    path,
                    synthetic_scroll: !options || self.cfg.settings.touchpad_injection,
                    retired: state.retired,
                };
                let owner = state.scroll.get_or_insert_with(ScrollPaths::default).phase(p, current);
                if state.scroll.as_ref().is_some_and(ScrollPaths::empty) {
                    state.scroll = None;
                }
                // The old momentum owns its decision even while a new touch is
                // Pending/Native. Its end cannot resolve the new pending frames.
                let allow = owner.is_some_and(Selection::scroll);
                if !state.contacts && !state.gesture && state.scroll.is_none() {
                    self.multifinger.remove(&origin);
                }
                if starts && path != MultiPath::Pending {
                    self.resolve_pending_contacts(origin, path);
                }
                return allow;
            }
        };
        let selected = match path {
            MultiPath::Native | MultiPath::NativeScroll => matches!(event, MultiEvent::Contacts(_)),
            MultiPath::MappedGestures => matches!(event, MultiEvent::Gesture(_)),
            MultiPath::Gestures => !matches!(event, MultiEvent::Contacts(_)),
            MultiPath::Scroll => matches!(event, MultiEvent::Scroll(_)),
            MultiPath::Pending | MultiPath::Suppressed => false,
        };
        let allow = live && !state.retired && selected;
        if !state.contacts && !state.gesture && state.scroll.is_none() {
            self.multifinger.remove(&origin);
        }
        if path != MultiPath::Pending {
            self.resolve_pending_contacts(origin, path);
        }
        allow
    }
    pub(super) fn retired_scroll(&self, origin: DeviceId, phase: ScrollPhase) -> bool {
        // New finger starts pass the ordinary focus/scope gates. Only an old
        // retired tail bypasses those gates to finish its own bookkeeping.
        if matches!(phase, ScrollPhase::Began | ScrollPhase::None) {
            return false;
        }
        self.multifinger.get(&origin).is_some_and(|state| {
            state
                .scroll
                .as_ref()
                .and_then(|s| s.owner(phase))
                .map_or(state.retired, |owner| owner.retired)
        })
    }
    fn stop_synthetic_tail_on_touch(&mut self, origin: DeviceId) {
        if self.cfg.os != Os::Windows
            || self.touchpad_options_enabled(origin)
                && !(self.cfg.settings.touchpad_injection && self.cfg.settings.mac_scroll_feel)
        {
            return;
        }
        let cancel = self.multifinger.get_mut(&origin).is_some_and(|state| {
            state
                .scroll
                .as_mut()
                .is_some_and(|scroll| scroll.stop_synthetic_tail(state.prelude))
        });
        if cancel {
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
    }

    pub(super) fn retire_received_multifinger(&mut self) {
        self.cancel_received_contacts();
        self.pending_contacts.clear();
        let mut cancel_scroll = false;
        for state in self.multifinger.values_mut() {
            cancel_scroll |= state.scroll.as_mut().is_some_and(ScrollPaths::retire);
            if state.scroll.as_ref().is_some_and(ScrollPaths::empty) {
                state.scroll = None;
            }
            state.retired = true;
            state.prelude = false;
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
    fn native_entry_barrier_retires_a_late_zero_before_admitting_new_contacts() {
        let peer = DeviceId([2; 32]);
        let mut r = router();
        r.test_connect(peer, 0);
        r.holder = peer;
        r.claim = peer;
        // A queued local zero arrived after routing chose the peer, but before
        // the native mode commit. It must not admit the navigation finger.
        r.on_local_event(&captured(sample(1000, 0, 0)), 1000);
        r.out.clear();
        let mut boundary = captured(sample(2000, 0, 0));
        boundary.event = InputEvent::TouchpadBoundary;
        r.on_local_event(&boundary, 2000);
        assert_eq!(r.contacts.mode(), crate::contacts::Mode::AwaitLift);
        r.on_local_event(&captured(sample(3000, 3, 1000)), 3000);
        assert!(!r.out.iter().any(|o| matches!(
            o,
            Output::Send {
                msg: Message::Input(InputMsg::TouchpadContacts(_)),
                ..
            }
        )));
        r.on_local_event(&captured(sample(4000, 0, 0)), 4000);
        r.on_local_event(&captured(sample(5000, 3, 1000)), 5000);
        assert!(r.out.iter().any(|o| matches!(o, Output::Send { msg: Message::Input(InputMsg::TouchpadContacts(f)), .. } if f.phase == Phase::Begin)));
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
    fn target_uses_gestures_when_injector_unavailable_and_contacts_when_available() {
        let origin = DeviceId([2; 32]);
        for capacity in [None, Some(5)] {
            let mut r = router();
            r.set_touchpad_contacts(capacity);
            assert!(r.accept_contacts, "stored preference remains enabled");
            r.out.clear();
            r.receive_contacts(origin, Frame::between(None, &sample(0, 2, 1000)));
            for phase in [GesturePhase::Began, GesturePhase::Ended] {
                let gesture = Gesture::Magnify { phase, amount: 0.1 };
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
                    .filter(|o| matches!(o, Output::Inject(InjectEvent::TouchpadContacts(_))))
                    .count(),
                usize::from(capacity.is_some())
            );
            assert_eq!(
                r.out
                    .iter()
                    .filter(|o| matches!(o, Output::Inject(InjectEvent::Gesture(_))))
                    .count(),
                if capacity.is_none() { 2 } else { 0 }
            );
            assert!(
                r.out.iter().all(|o| matches!(o, Output::Inject(_))),
                "no unavailable fault or stop"
            );
        }
    }
    fn mac_scroll_router(enabled: bool, capacity: Option<u8>) -> (Router, DeviceId) {
        let origin = DeviceId([2; 32]);
        let mut r = router();
        r.test_connect(origin, 0);
        r.peers.get_mut(&origin).unwrap().os = Os::MacOS;
        r.set_touchpad_contacts(capacity);
        let mut settings = r.cfg.settings.clone();
        settings.mac_scroll_feel = enabled;
        r.set_settings(settings);
        r.out.clear();
        (r, origin)
    }
    #[test]
    fn continuous_fling_physical_touch_cancels_only_synthetic_tail() {
        for (parent, mac) in [(true, true), (true, false), (false, false)] {
            let (mut r, origin) = options_router(parent, true, mac);
            r.set_accept_contacts(false);
            r.focus_scope = Some(r.new_focus_scope());
            let scroll = |phase| InputMsg::Scroll {
                source: SourceKind::Touchpad,
                dx: 0.,
                dy: 1.,
                unit: ScrollAmount::Reference,
                phase,
            };
            let old = sample(0, 2, 1000);
            r.receive_contacts(origin, Frame::between(None, &old));
            r.on_input(origin, origin, 0, scroll(ScrollPhase::Began), 0);
            r.receive_contacts(origin, Frame::between(Some(&old), &sample(8000, 0, 0)));
            r.on_input(origin, origin, 0, scroll(ScrollPhase::Ended), 8000);
            r.on_input(origin, origin, 0, scroll(ScrollPhase::MomentumBegan), 9000);
            r.out.clear();
            r.receive_contacts(origin, Frame::between(None, &sample(10000, 1, 1000)));
            let cancelled = r
                .out
                .iter()
                .filter(|o| {
                    matches!(o,
                Output::Inject(InjectEvent::TouchpadScroll { wheel, .. }) if wheel.phase == ScrollPhase::Cancelled)
                })
                .count();
            assert_eq!(cancelled, usize::from(mac));
            r.out.clear();
            r.on_input(origin, origin, 0, scroll(ScrollPhase::Momentum), 11000);
            assert!(
                !r.out
                    .iter()
                    .any(|o| matches!(o, Output::Inject(InjectEvent::TouchpadScroll { .. })))
            );
            if !parent {
                assert!(
                    r.out
                        .iter()
                        .any(|o| matches!(o, Output::Inject(InjectEvent::Scroll { .. }))),
                    "legacy wheel tail remains unchanged"
                );
            }
        }
    }
    #[test]
    fn parent_off_uses_plain_wheel_instead_of_synthetic_ptp() {
        let (mut r, origin) = mac_scroll_router(true, Some(5));
        r.set_peer_touchpad_options(origin, true);
        let mut value = serde_json::to_value(&r.cfg.settings).unwrap();
        value["touchpad_injection"] = false.into();
        r.set_settings(serde_json::from_value(value).unwrap());
        r.set_accept_contacts(false);
        r.inject_scroll(
            origin,
            SourceKind::Touchpad,
            1.,
            2.,
            ScrollAmount::Reference,
            ScrollPhase::Began,
        );
        assert!(matches!(r.out.last(), Some(Output::Inject(InjectEvent::Scroll { .. }))));
    }
    fn options_router(parent: bool, native: bool, mac: bool) -> (Router, DeviceId) {
        let (mut r, origin) = mac_scroll_router(mac, Some(5));
        r.set_peer_touchpad_options(origin, true);
        let mut settings = r.cfg.settings.clone();
        settings.touchpad_injection = parent;
        settings.native_multifinger = native;
        r.set_settings(settings);
        r.out.clear();
        (r, origin)
    }
    #[test]
    fn touchpad_options_matrix_selects_one_scroll_injection_path() {
        for parent in [false, true] {
            for native in [false, true] {
                for mac in [false, true] {
                    let (mut r, origin) = options_router(parent, native, mac);
                    let first = sample(0, 2, 1000);
                    r.receive_contacts(origin, Frame::between(None, &first));
                    for phase in [ScrollPhase::Began, ScrollPhase::Changed, ScrollPhase::Ended] {
                        if r.filter_multifinger(origin, MultiEvent::Scroll(phase)) {
                            r.inject_scroll(origin, SourceKind::Touchpad, 1., 2., ScrollAmount::Reference, phase);
                        }
                    }
                    r.receive_contacts(origin, Frame::between(Some(&first), &sample(8000, 0, 0)));
                    let raw = r
                        .out
                        .iter()
                        .any(|o| matches!(o, Output::Inject(InjectEvent::TouchpadContacts(_))));
                    let synthetic = r
                        .out
                        .iter()
                        .any(|o| matches!(o, Output::Inject(InjectEvent::TouchpadScroll { .. })));
                    let wheel = r
                        .out
                        .iter()
                        .any(|o| matches!(o, Output::Inject(InjectEvent::Scroll { .. })));
                    assert_eq!(
                        (raw, synthetic, wheel),
                        (parent && !mac, parent && mac, !parent),
                        "parent={parent} native={native} mac={mac}"
                    );
                    if raw {
                        let frames: Vec<_> = r
                            .out
                            .iter()
                            .filter_map(|o| match o {
                                Output::Inject(InjectEvent::TouchpadContacts(f)) => Some(*f),
                                _ => None,
                            })
                            .collect();
                        assert_eq!(
                            frames,
                            [
                                Frame::between(None, &first),
                                Frame::between(Some(&first), &sample(8000, 0, 0))
                            ],
                            "native scroll preserves physical geometry/timestamps, bypassing inverse gain and the Mac curve"
                        );
                    }
                }
            }
        }
    }
    #[test]
    fn touchpad_options_matrix_maps_non_scroll_gestures_only_when_requested() {
        for parent in [false, true] {
            for native in [false, true] {
                for mac in [false, true] {
                    for fingers in [2, 3, 4] {
                        let (mut r, origin) = options_router(parent, native, mac);
                        r.receive_contacts(origin, Frame::between(None, &sample(0, fingers, 1000)));
                        let gesture = Gesture::Magnify {
                            phase: GesturePhase::Began,
                            amount: 0.1,
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
                        let raw = r
                            .out
                            .iter()
                            .any(|o| matches!(o, Output::Inject(InjectEvent::TouchpadContacts(_))));
                        let mapped = r
                            .out
                            .iter()
                            .any(|o| matches!(o, Output::Inject(InjectEvent::Gesture(_))));
                        assert_eq!(
                            (raw, mapped),
                            (parent && native, !parent || !native),
                            "parent={parent} native={native} mac={mac} fingers={fingers}"
                        );
                    }
                }
            }
        }
    }
    #[test]
    fn scroll_only_cancels_before_third_contact_or_pinch_without_replaying_tails() {
        for pinch in [false, true] {
            let (mut r, origin) = options_router(true, false, false);
            let first = sample(0, 2, 1000);
            r.receive_contacts(origin, Frame::between(None, &first));
            assert!(r.out.is_empty());
            assert!(!r.filter_multifinger(origin, MultiEvent::Scroll(ScrollPhase::Began)));
            assert_eq!(r.out.len(), 1);
            r.out.clear();
            if pinch {
                assert!(r.filter_multifinger(origin, magnify(GesturePhase::Began)));
            } else {
                r.receive_contacts(origin, Frame::between(Some(&first), &sample(8000, 3, 1200)));
                assert!(r.filter_multifinger(
                    origin,
                    MultiEvent::Gesture(Gesture::DockSwipe {
                        axis: oink_foundation::input::SwipeAxis::Vertical,
                        phase: GesturePhase::Began,
                        progress: 0.1,
                        velocity: 0.,
                    })
                ));
            }
            assert_eq!(cancels(&r), 1);
            assert!(
                !r.out
                    .iter()
                    .any(|o| matches!(o, Output::Inject(InjectEvent::TouchpadContacts(f)) if f.phase != Phase::Cancel))
            );
            r.out.clear();
            r.receive_contacts(
                origin,
                Frame::between(Some(&sample(8000, 3, 1200)), &sample(16000, 2, 1300)),
            );
            assert!(!r.filter_multifinger(origin, MultiEvent::Scroll(ScrollPhase::Changed)));
            assert!(r.out.is_empty());
        }
    }
    #[test]
    fn scroll_only_never_uses_the_mac_curve_after_a_mapped_gesture_or_injector_loss() {
        for fingers in [2, 3, 4] {
            let (mut r, origin) = options_router(true, false, false);
            r.receive_contacts(origin, Frame::between(None, &sample(0, fingers, 1000)));
            assert!(r.filter_multifinger(origin, magnify(GesturePhase::Began)));
            assert!(!r.filter_multifinger(origin, MultiEvent::Scroll(ScrollPhase::Began)));
            assert!(r.out.is_empty());
        }
        for parent in [false, true] {
            for mac in [false, true] {
                let (mut r, origin) = options_router(parent, false, mac);
                r.set_touchpad_contacts(None);
                assert!(r.filter_multifinger(origin, MultiEvent::Scroll(ScrollPhase::Began)));
                r.inject_scroll(
                    origin,
                    SourceKind::Touchpad,
                    1.,
                    2.,
                    ScrollAmount::Reference,
                    ScrollPhase::Began,
                );
                assert_eq!(
                    r.out
                        .iter()
                        .any(|o| matches!(o, Output::Inject(InjectEvent::TouchpadScroll { .. }))),
                    parent && mac
                );
            }
        }
    }
    #[test]
    fn legacy_mac_source_keeps_existing_contacts_and_scroll_paths() {
        for legacy in [false, true] {
            for mac in [false, true] {
                let (mut r, origin) = mac_scroll_router(mac, Some(5));
                let mut settings = r.cfg.settings.clone();
                settings.touchpad_injection = false;
                settings.native_multifinger = false;
                r.set_settings(settings);
                r.set_accept_contacts(legacy);
                r.out.clear();
                r.receive_contacts(origin, Frame::between(None, &sample(0, 2, 1000)));
                let allow = r.filter_multifinger(origin, MultiEvent::Scroll(ScrollPhase::Began));
                assert_eq!(allow, !legacy || mac);
                if allow {
                    r.inject_scroll(
                        origin,
                        SourceKind::Touchpad,
                        1.,
                        2.,
                        ScrollAmount::Reference,
                        ScrollPhase::Began,
                    );
                }
                assert!(
                    !r.out
                        .iter()
                        .any(|o| matches!(o, Output::Inject(InjectEvent::Scroll { .. })))
                );
            }
        }
    }
    #[test]
    fn parent_off_retirement_never_creates_synthetic_native_scroll() {
        let (mut r, origin) = options_router(false, true, true);
        assert!(r.filter_multifinger(origin, MultiEvent::Scroll(ScrollPhase::Began)));
        r.retire_received_multifinger();
        assert!(r.out.is_empty());
    }
    fn magnify(phase: GesturePhase) -> MultiEvent {
        MultiEvent::Gesture(Gesture::Magnify { phase, amount: 0.1 })
    }
    // Separate physical touches must be classified independently from the
    // optional/overlapping system momentum tail of an earlier touch.
    #[test]
    fn regression_new_three_finger_touch_after_scroll_without_momentum() {
        let (mut r, origin) = mac_scroll_router(true, Some(5));
        let scroll = sample(0, 2, 1000);
        let lift = sample(16_000, 0, 0);
        r.receive_contacts(origin, Frame::between(None, &scroll));
        assert!(r.filter_multifinger(origin, MultiEvent::Scroll(ScrollPhase::Began)));
        r.receive_contacts(origin, Frame::between(Some(&scroll), &lift));
        assert!(r.filter_multifinger(origin, MultiEvent::Scroll(ScrollPhase::Ended)));
        r.out.clear();

        r.receive_contacts(origin, Frame::between(None, &sample(144_000, 3, 1000)));
        assert!(
            r.out.iter().any(|o| matches!(o,
            Output::Inject(InjectEvent::TouchpadContacts(f)) if f.phase == Phase::Begin)),
            "an optional momentum start cannot reserve the next physical touch"
        );
    }

    #[test]
    fn regression_new_touch_before_old_momentum_end_keeps_native_begin() {
        let (mut r, origin) = mac_scroll_router(true, Some(5));
        let scroll = sample(0, 2, 1000);
        let lift = sample(16_000, 0, 0);
        r.receive_contacts(origin, Frame::between(None, &scroll));
        assert!(r.filter_multifinger(origin, MultiEvent::Scroll(ScrollPhase::Began)));
        r.receive_contacts(origin, Frame::between(Some(&scroll), &lift));
        assert!(r.filter_multifinger(origin, MultiEvent::Scroll(ScrollPhase::Ended)));
        assert!(r.filter_multifinger(origin, MultiEvent::Scroll(ScrollPhase::MomentumBegan)));
        r.out.clear();

        // Recorded ordering: the new two-contact Begin precedes the old
        // MomentumEnded, then a third contact joins 16 ms later.
        let next = sample(144_000, 2, 1000);
        r.receive_contacts(origin, Frame::between(None, &next));
        r.filter_multifinger(origin, MultiEvent::Scroll(ScrollPhase::MomentumEnded));
        r.receive_contacts(origin, Frame::between(Some(&next), &sample(160_000, 3, 1200)));
        let frames: Vec<_> = r
            .out
            .iter()
            .filter_map(|o| match o {
                Output::Inject(InjectEvent::TouchpadContacts(f)) => Some(f),
                _ => None,
            })
            .collect();
        assert_eq!(
            frames.len(),
            2,
            "the new touch must retain and replay its Begin independently of the old momentum"
        );
        assert_eq!(frames[0].phase, Phase::Begin);
        assert_eq!(frames[0].time_us, 144_000);
        assert_eq!(frames[1].time_us, 160_000);
    }

    #[test]
    fn new_touch_stops_old_synthetic_tail_and_preserves_new_native_owner() {
        for position in 0..3 {
            let (mut r, origin) = mac_scroll_router(true, Some(5));
            let old = sample(0, 2, 1000);
            r.receive_contacts(origin, Frame::between(None, &old));
            assert!(r.filter_multifinger(origin, MultiEvent::Scroll(ScrollPhase::Began)));
            r.receive_contacts(origin, Frame::between(Some(&old), &sample(8000, 0, 0)));
            assert!(r.filter_multifinger(origin, MultiEvent::Scroll(ScrollPhase::Ended)));
            if position >= 1 {
                assert!(r.filter_multifinger(origin, MultiEvent::Scroll(ScrollPhase::MomentumBegan)));
            }
            if position == 2 {
                assert!(r.filter_multifinger(origin, MultiEvent::Scroll(ScrollPhase::MomentumEnded)));
            }
            r.out.clear();
            let next = sample(16000, 2, 1100);
            r.receive_contacts(origin, Frame::between(None, &next));
            assert_eq!(
                r.out
                    .iter()
                    .filter(|o| matches!(o,
                Output::Inject(InjectEvent::TouchpadScroll { wheel, .. }) if wheel.phase == ScrollPhase::Cancelled))
                    .count(),
                usize::from(position != 2)
            );
            r.out.clear();
            if position == 0 {
                assert!(!r.filter_multifinger(origin, MultiEvent::Scroll(ScrollPhase::MomentumBegan)));
            }
            if position != 2 {
                assert!(!r.filter_multifinger(origin, MultiEvent::Scroll(ScrollPhase::Momentum)));
            }
            r.receive_contacts(origin, Frame::between(Some(&next), &sample(24000, 3, 1300)));
            let frames: Vec<_> = r
                .out
                .iter()
                .filter_map(|o| match o {
                    Output::Inject(InjectEvent::TouchpadContacts(f)) => Some(*f),
                    _ => None,
                })
                .collect();
            assert_eq!(
                frames.iter().map(|f| (f.phase, f.time_us)).collect::<Vec<_>>(),
                [(Phase::Begin, 16000), (Phase::Move, 24000)]
            );
            assert!(!r.filter_multifinger(origin, magnify(GesturePhase::Began)));
            if position != 2 {
                assert!(!r.filter_multifinger(origin, MultiEvent::Scroll(ScrollPhase::MomentumEnded)));
            }
            // Old MomentumEnded cannot retire the newly admitted native touch.
            r.receive_contacts(
                origin,
                Frame::between(Some(&sample(24000, 3, 1300)), &sample(32000, 3, 1400)),
            );
            assert!(
                matches!(r.out.last(), Some(Output::Inject(InjectEvent::TouchpadContacts(f))) if f.time_us == 32000)
            );
            assert!(
                r.out
                    .iter()
                    .all(|o| !matches!(o, Output::Inject(InjectEvent::TouchpadScroll { .. })))
            );
        }
    }

    #[test]
    fn late_old_finger_end_does_not_resolve_new_pending_touch() {
        let (mut r, origin) = mac_scroll_router(true, Some(5));
        let old = sample(0, 2, 1000);
        r.receive_contacts(origin, Frame::between(None, &old));
        assert!(r.filter_multifinger(origin, MultiEvent::Scroll(ScrollPhase::Began)));
        r.receive_contacts(origin, Frame::between(Some(&old), &sample(8000, 0, 0)));
        let next = sample(16000, 2, 1000);
        r.receive_contacts(origin, Frame::between(None, &next));
        assert!(r.out.iter().any(|o| matches!(o,
            Output::Inject(InjectEvent::TouchpadScroll { wheel, .. }) if wheel.phase == ScrollPhase::Cancelled)));
        assert!(!r.filter_multifinger(origin, MultiEvent::Scroll(ScrollPhase::Ended)));
        assert_eq!(r.pending_contacts.len, 1);
        assert!(!r.filter_multifinger(origin, MultiEvent::Scroll(ScrollPhase::MomentumBegan)));
        assert!(!r.filter_multifinger(origin, MultiEvent::Scroll(ScrollPhase::MomentumEnded)));
        assert_eq!(r.pending_contacts.len, 1);
        r.receive_contacts(origin, Frame::between(Some(&next), &sample(24000, 3, 1300)));
        assert_eq!(
            r.out
                .iter()
                .filter(|o| matches!(o, Output::Inject(InjectEvent::TouchpadContacts(_))))
                .count(),
            2
        );
    }

    #[test]
    fn suppressed_old_momentum_stays_suppressed_during_new_scroll() {
        let (mut r, origin) = mac_scroll_router(true, Some(5));
        let old = sample(0, 3, 1000);
        r.receive_contacts(origin, Frame::between(None, &old));
        assert!(!r.filter_multifinger(origin, MultiEvent::Scroll(ScrollPhase::Began)));
        r.receive_contacts(origin, Frame::between(Some(&old), &sample(8000, 0, 0)));
        assert!(!r.filter_multifinger(origin, MultiEvent::Scroll(ScrollPhase::Ended)));
        assert!(!r.filter_multifinger(origin, MultiEvent::Scroll(ScrollPhase::MomentumBegan)));
        let next = sample(16000, 2, 1200);
        r.receive_contacts(origin, Frame::between(None, &next));
        assert!(r.filter_multifinger(origin, MultiEvent::Scroll(ScrollPhase::Began)));
        assert!(!r.filter_multifinger(origin, MultiEvent::Scroll(ScrollPhase::Momentum)));
        assert!(!r.filter_multifinger(origin, MultiEvent::Scroll(ScrollPhase::MomentumEnded)));
        assert!(r.filter_multifinger(origin, MultiEvent::Scroll(ScrollPhase::Changed)));
        assert!(r.filter_multifinger(origin, MultiEvent::Scroll(ScrollPhase::Ended)));
        assert!(r.filter_multifinger(origin, MultiEvent::Scroll(ScrollPhase::MomentumBegan)));
        assert!(r.filter_multifinger(origin, MultiEvent::Scroll(ScrollPhase::MomentumEnded)));
    }

    #[test]
    fn scroll_or_gesture_before_contact_begin_is_current_touch_evidence() {
        for gesture_first in [false, true] {
            let (mut r, origin) = mac_scroll_router(true, Some(5));
            if gesture_first {
                assert!(!r.filter_multifinger(origin, magnify(GesturePhase::Began)));
            } else {
                assert!(r.filter_multifinger(origin, MultiEvent::Scroll(ScrollPhase::Began)));
            }
            r.receive_contacts(origin, Frame::between(None, &sample(0, 2, 1000)));
            assert_eq!(
                r.out
                    .iter()
                    .filter(|o| matches!(o, Output::Inject(InjectEvent::TouchpadContacts(_))))
                    .count(),
                usize::from(gesture_first)
            );
            assert_eq!(
                r.multifinger[&origin].path,
                if gesture_first {
                    MultiPath::Native
                } else {
                    MultiPath::Scroll
                }
            );
        }
    }

    #[test]
    fn retired_old_momentum_drains_before_scope_gates_without_poisoning_new_touch() {
        let (mut r, origin) = mac_scroll_router(true, Some(5));
        r.focus_scope = Some(r.new_focus_scope());
        let scroll = |phase| InputMsg::Scroll {
            source: SourceKind::Touchpad,
            dx: 0.,
            dy: 1.,
            unit: ScrollAmount::Reference,
            phase,
        };
        let old = sample(0, 2, 1000);
        r.receive_contacts(origin, Frame::between(None, &old));
        r.on_input(origin, origin, 0, scroll(ScrollPhase::Began), 0);
        r.receive_contacts(origin, Frame::between(Some(&old), &sample(8000, 0, 0)));
        r.on_input(origin, origin, 0, scroll(ScrollPhase::Ended), 8000);
        r.on_input(origin, origin, 0, scroll(ScrollPhase::MomentumBegan), 9000);
        r.become_source(2, origin);
        r.become_holder(3, None);
        r.claim = r.me();
        r.out.clear();
        let next = sample(16000, 2, 1100);
        r.receive_contacts(origin, Frame::between(None, &next));
        // Focus retirement has already removed scroll_fwd. This tail must still
        // drain its old owner without injecting or erasing new pending frames.
        r.on_input(origin, origin, 0, scroll(ScrollPhase::Momentum), 17000);
        r.on_input(origin, origin, 0, scroll(ScrollPhase::MomentumEnded), 18000);
        assert!(r.out.is_empty());
        assert_eq!(r.pending_contacts.len, 1);
        r.receive_contacts(origin, Frame::between(Some(&next), &sample(24000, 3, 1300)));
        assert_eq!(
            r.out
                .iter()
                .filter(|o| matches!(o, Output::Inject(InjectEvent::TouchpadContacts(_))))
                .count(),
            2
        );
        r.receive_contacts(
            origin,
            Frame::between(Some(&sample(24000, 3, 1300)), &sample(32000, 0, 0)),
        );
        assert!(!r.multifinger.contains_key(&origin));
    }

    #[test]
    fn input_dispatch_cancels_old_tail_before_new_native_replay() {
        let (mut r, origin) = mac_scroll_router(true, Some(5));
        r.focus_scope = Some(r.new_focus_scope());
        let scroll = |phase| InputMsg::Scroll {
            source: SourceKind::Touchpad,
            dx: 0.,
            dy: 1.,
            unit: ScrollAmount::Reference,
            phase,
        };
        let old = sample(0, 2, 1000);
        r.receive_contacts(origin, Frame::between(None, &old));
        r.on_input(origin, origin, 0, scroll(ScrollPhase::Began), 0);
        r.receive_contacts(origin, Frame::between(Some(&old), &sample(8000, 0, 0)));
        r.on_input(origin, origin, 0, scroll(ScrollPhase::Ended), 8000);
        r.out.clear();
        let next = sample(16000, 2, 1100);
        r.receive_contacts(origin, Frame::between(None, &next));
        r.on_input(origin, origin, 0, scroll(ScrollPhase::MomentumBegan), 17000);
        r.on_input(origin, origin, 0, scroll(ScrollPhase::Momentum), 18000);
        r.receive_contacts(origin, Frame::between(Some(&next), &sample(24000, 3, 1300)));
        r.receive_contacts(
            origin,
            Frame::between(Some(&sample(24000, 3, 1300)), &sample(32000, 0, 0)),
        );
        r.on_input(origin, origin, 0, scroll(ScrollPhase::Momentum), 33000);
        r.on_input(origin, origin, 0, scroll(ScrollPhase::MomentumEnded), 34000);
        let order: Vec<_> = r
            .out
            .iter()
            .filter_map(|o| match o {
                Output::Inject(InjectEvent::TouchpadContacts(f)) => Some((Some(f.phase), None, f.time_us)),
                Output::Inject(InjectEvent::TouchpadScroll { wheel, .. }) => Some((None, Some(wheel.phase), 0)),
                _ => None,
            })
            .collect();
        assert_eq!(
            order,
            [
                (None, Some(ScrollPhase::Cancelled), 0),
                (Some(Phase::Begin), None, 16000),
                (Some(Phase::Move), None, 24000),
                (Some(Phase::End), None, 32000),
            ]
        );
        assert!(!r.multifinger.contains_key(&origin));
    }

    #[test]
    fn optional_old_momentum_cannot_restart_after_new_native_touch_ends() {
        let (mut r, origin) = mac_scroll_router(true, Some(5));
        let old = sample(0, 2, 1000);
        r.receive_contacts(origin, Frame::between(None, &old));
        assert!(r.filter_multifinger(origin, MultiEvent::Scroll(ScrollPhase::Began)));
        r.receive_contacts(origin, Frame::between(Some(&old), &sample(8000, 0, 0)));
        assert!(r.filter_multifinger(origin, MultiEvent::Scroll(ScrollPhase::Ended)));
        let next = sample(16000, 3, 1100);
        r.receive_contacts(origin, Frame::between(None, &next));
        r.receive_contacts(origin, Frame::between(Some(&next), &sample(24000, 0, 0)));
        assert!(!r.filter_multifinger(origin, MultiEvent::Scroll(ScrollPhase::MomentumBegan)));
        assert!(!r.filter_multifinger(origin, MultiEvent::Scroll(ScrollPhase::MomentumEnded)));
        assert!(!r.multifinger.contains_key(&origin));
    }

    #[test]
    fn new_begin_never_replays_an_older_pending_touch() {
        let (mut r, origin) = mac_scroll_router(true, Some(5));
        r.receive_contacts(origin, Frame::between(None, &sample(0, 2, 1000)));
        r.receive_contacts(origin, Frame::between(None, &sample(16000, 3, 1300)));
        let frames: Vec<_> = r
            .out
            .iter()
            .filter_map(|o| match o {
                Output::Inject(InjectEvent::TouchpadContacts(f)) => Some(*f),
                _ => None,
            })
            .collect();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].phase, Phase::Begin);
        assert_eq!(frames[0].time_us, 16000);
        assert!(r.pending_contacts.origin.is_none());
    }

    #[test]
    fn mac_scroll_feel_selects_exactly_one_owner_with_option_off_on_and_unavailable() {
        for enabled in [false, true] {
            for capacity in [None, Some(5)] {
                let (mut r, origin) = mac_scroll_router(enabled, capacity);
                r.receive_contacts(origin, Frame::between(None, &sample(0, 2, 1000)));
                let native = capacity.is_some() && !enabled;
                assert_eq!(
                    r.out
                        .iter()
                        .any(|o| matches!(o, Output::Inject(InjectEvent::TouchpadContacts(_)))),
                    native
                );
                assert_eq!(
                    r.filter_multifinger(origin, MultiEvent::Scroll(ScrollPhase::Began)),
                    !native
                );
                r.receive_contacts(
                    origin,
                    Frame::between(Some(&sample(0, 2, 1000)), &sample(8000, 3, 1200)),
                );
                assert_eq!(
                    r.filter_multifinger(origin, magnify(GesturePhase::Began)),
                    capacity.is_none()
                );
                assert_eq!(
                    r.filter_multifinger(origin, MultiEvent::Scroll(ScrollPhase::Ended)),
                    !native
                );
                assert_eq!(
                    r.filter_multifinger(origin, MultiEvent::Scroll(ScrollPhase::MomentumBegan)),
                    !native
                );
                if enabled && capacity.is_some() {
                    assert!(
                        r.out.is_empty(),
                        "scroll→pinch and scroll 2→3 cannot inject raw contacts"
                    );
                }
            }
        }
    }
    #[test]
    fn mac_scroll_feel_replays_native_pinch_and_three_finger_frames_in_source_order() {
        for start_fingers in [2, 3] {
            let (mut r, origin) = mac_scroll_router(true, Some(5));
            r.receive_contacts(origin, Frame::between(None, &sample(0, start_fingers, 1000)));
            if start_fingers == 2 {
                assert!(r.out.is_empty());
                r.receive_contacts(
                    origin,
                    Frame::between(Some(&sample(0, 2, 1000)), &sample(8000, 2, 1200)),
                );
                assert!(r.out.is_empty());
            }
            assert!(!r.filter_multifinger(origin, magnify(GesturePhase::Began)));
            let frames: Vec<_> = r
                .out
                .iter()
                .filter_map(|o| match o {
                    Output::Inject(InjectEvent::TouchpadContacts(f)) => Some(*f),
                    _ => None,
                })
                .collect();
            assert_eq!(frames.len(), if start_fingers == 2 { 2 } else { 1 });
            assert_eq!(frames[0].time_us, 0);
            assert_eq!(frames[0].phase, Phase::Begin);
            if start_fingers == 2 {
                assert_eq!(frames[1].time_us, 8000);
            }
            assert!(!r.filter_multifinger(origin, MultiEvent::Scroll(ScrollPhase::Began)));
        }
    }
    #[test]
    fn mac_scroll_feel_pending_count_changes_lifts_overflow_and_boundaries_are_safe() {
        for boundary in 0..6 {
            let (mut r, origin) = mac_scroll_router(true, Some(5));
            r.receive_contacts(origin, Frame::between(None, &sample(0, 2, 1000)));
            assert!(r.out.is_empty());
            match boundary {
                0 => {
                    r.receive_contacts(
                        origin,
                        Frame::between(Some(&sample(0, 2, 1000)), &sample(8000, 3, 1200)),
                    );
                    assert_eq!(
                        r.out
                            .iter()
                            .filter(|o| matches!(o, Output::Inject(InjectEvent::TouchpadContacts(_))))
                            .count(),
                        2
                    );
                    assert!(!r.filter_multifinger(origin, MultiEvent::Scroll(ScrollPhase::Began)));
                }
                1 => {
                    r.receive_contacts(origin, Frame::between(Some(&sample(0, 2, 1000)), &sample(8000, 0, 0)));
                    assert!(r.out.is_empty());
                    assert!(r.multifinger.is_empty());
                }
                2 => {
                    for i in 1..=CLASSIFY_FRAMES {
                        r.receive_contacts(
                            origin,
                            Frame::between(Some(&sample(0, 2, 1000)), &sample(i as u64 * 8000, 2, 1200)),
                        );
                    }
                    assert!(r.out.is_empty());
                    assert!(!r.filter_multifinger(origin, MultiEvent::Scroll(ScrollPhase::Began)));
                    assert_eq!(r.multifinger[&origin].path, MultiPath::Suppressed);
                }
                3 | 4 => {
                    if boundary == 3 {
                        let mut settings = r.cfg.settings.clone();
                        settings.mac_scroll_feel = false;
                        r.set_settings(settings);
                    } else {
                        r.set_paused(true);
                    }
                    assert!(!r.out.iter().any(|o| matches!(o, Output::Inject(_))));
                    assert!(!r.filter_multifinger(origin, magnify(GesturePhase::Began)));
                }
                _ => {
                    r.release_origin(origin);
                    assert!(r.pending_contacts.origin.is_none());
                }
            }
        }
    }
    #[test]
    fn mac_scroll_feel_policy_switch_cancels_chosen_owner_and_never_injects_tails() {
        for native in [false, true] {
            let (mut r, origin) = mac_scroll_router(true, Some(5));
            r.receive_contacts(origin, Frame::between(None, &sample(0, 2, 1000)));
            if native {
                r.filter_multifinger(origin, magnify(GesturePhase::Began));
            } else {
                assert!(r.filter_multifinger(origin, MultiEvent::Scroll(ScrollPhase::Began)));
            }
            r.out.clear();
            let mut settings = r.cfg.settings.clone();
            settings.mac_scroll_feel = false;
            r.set_settings(settings);
            assert_eq!(cancels(&r), usize::from(native));
            if !native {
                assert!(r.out.iter().any(|o| matches!(o, Output::Inject(InjectEvent::TouchpadScroll { wheel, .. }) if wheel.phase == ScrollPhase::Cancelled)));
            }
            r.out.clear();
            assert!(!r.filter_multifinger(origin, MultiEvent::Contacts(Phase::End)));
            assert!(!r.filter_multifinger(origin, magnify(GesturePhase::Ended)));
            assert!(!r.filter_multifinger(origin, MultiEvent::Scroll(ScrollPhase::Ended)));
            assert!(!r.filter_multifinger(origin, MultiEvent::Scroll(ScrollPhase::MomentumBegan)));
            assert!(r.out.is_empty());
        }
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
