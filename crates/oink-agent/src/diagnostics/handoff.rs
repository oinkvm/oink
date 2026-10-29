//! Fixed-size native-owner evidence. Identity/screen hashing happens on the ordinary drain.
use super::*;
use oink_foundation::{Point, Rect, input::CaptureMode};
use oink_routing::{
    Output, ScreenPos,
    router::{DecisionKind, DecisionReason, RoutingDiagnostics},
};
use oink_routing::{Packet as Message, control::ControlMsg};
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum HandoffAction {
    FocusEnterSent,
    FocusEnterReceived,
    FocusStateSent,
    FocusStateReceived,
    FocusResumeSent,
    FocusResumeReceived,
    RoutingDecision,
    PairAttached,
    PairDetached,
    ModeRequested,
    ModeStarted,
    ModeCompleted,
    RestoreStarted,
    RestoreCompleted,
    MotionFlush,
    TrailActive,
    TrailDepart,
    TrailResume,
    TrailSent,
    TrailReceived,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub enum FlushReason {
    Due,
    OrderingBarrier,
    TargetChanged,
    FrequencyChanged,
    AttachmentBarrier,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HandoffEvent {
    pub action: HandoffAction,
    pub peer: Option<String>,
    pub holder: Option<String>,
    pub epoch: Option<u64>,
    pub router_instance: Option<String>,
    pub screen: Option<String>,
    pub at: Option<Point>,
    pub clip: Option<Rect>,
    pub mode: Option<CaptureMode>,
    pub sequence: Option<u64>,
    pub success: Option<bool>,
    pub decision: Option<DecisionKind>,
    pub reason: Option<DecisionReason>,
    pub count: u64,
    pub topology_sequence: u64,
    pub sender: Option<String>,
    pub recipient: Option<String>,
    pub origin: Option<String>,
    pub next_target: Option<String>,
    pub flush_reason: Option<FlushReason>,
    pub first_sequence: Option<u64>,
    pub message_identity: Option<String>,
    pub scope: Vec<(String, String)>,
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Screen {
    bytes: [u8; 256],
    len: u16,
    device: DeviceId,
}
impl Screen {
    pub(super) fn alias(&self, recorder: &Recorder) -> String {
        recorder.anonymizer().screen(
            self.device,
            std::str::from_utf8(&self.bytes[..self.len as usize]).unwrap_or(""),
        )
    }
    pub(super) fn new(device: DeviceId, text: &str) -> Option<Self> {
        if text.len() > 256 {
            return None;
        }
        let mut bytes = [0; 256];
        bytes[..text.len()].copy_from_slice(text.as_bytes());
        Some(Self {
            bytes,
            len: text.len() as u16,
            device,
        })
    }
}
#[derive(Clone, Copy)]
pub(super) struct Raw {
    pub action: HandoffAction,
    pub peer: Option<DeviceId>,
    pub holder: Option<DeviceId>,
    pub epoch: Option<u64>,
    pub instance: Option<[u8; 16]>,
    pub(super) screen: Option<Screen>,
    pub at: Option<Point>,
    pub clip: Option<Rect>,
    pub mode: Option<CaptureMode>,
    pub sequence: Option<u64>,
    pub success: Option<bool>,
    pub decision: Option<DecisionKind>,
    pub reason: Option<DecisionReason>,
    pub count: u64,
    pub topology_sequence: u64,
    pub sender: Option<DeviceId>,
    pub recipient: Option<DeviceId>,
    pub origin: Option<DeviceId>,
    pub next_target: Option<DeviceId>,
    pub flush_reason: Option<FlushReason>,
    pub first_sequence: Option<u64>,
    pub scope: [Option<(DeviceId, [u8; 16])>; 16],
}
impl Raw {
    pub fn new(action: HandoffAction) -> Self {
        Self {
            action,
            peer: None,
            holder: None,
            epoch: None,
            instance: None,
            screen: None,
            at: None,
            clip: None,
            mode: None,
            sequence: None,
            success: None,
            decision: None,
            reason: None,
            count: 0,
            topology_sequence: 0,
            sender: None,
            recipient: None,
            origin: None,
            next_target: None,
            flush_reason: None,
            first_sequence: None,
            scope: [None; 16],
        }
    }
    pub(super) fn export(self, recorder: &Recorder) -> Event {
        let anon = recorder.anonymizer();
        let message_identity = self
            .sender
            .zip(self.recipient)
            .zip(self.epoch)
            .map(|((sender, recipient), epoch)| {
                let mut bytes = Vec::with_capacity(128);
                bytes.extend(sender.0);
                bytes.extend(recipient.0);
                bytes.extend(epoch.to_le_bytes());
                let kind = match self.action {
                    HandoffAction::FocusEnterSent | HandoffAction::FocusEnterReceived => 0,
                    HandoffAction::FocusStateSent | HandoffAction::FocusStateReceived => 1,
                    HandoffAction::FocusResumeSent | HandoffAction::FocusResumeReceived => 2,
                    _ => 3,
                };
                bytes.push(kind);
                if let Some(holder) = self.holder {
                    bytes.extend(holder.0);
                }
                if let Some(instance) = self.instance {
                    bytes.extend(instance);
                }
                for (device, instance) in self.scope.iter().flatten() {
                    bytes.extend(device.0);
                    bytes.extend(instance);
                }
                anon.control_identity(&bytes)
            });
        Event::Handoff(HandoffEvent {
            action: self.action,
            peer: self.peer.map(|d| anon.device(d)),
            holder: self.holder.map(|d| anon.device(d)),
            epoch: self.epoch,
            router_instance: self.instance.map(|id| anon.router_instance(&id)),
            screen: self
                .screen
                .map(|s| anon.screen(s.device, std::str::from_utf8(&s.bytes[..s.len as usize]).unwrap_or(""))),
            at: self.at,
            clip: self.clip,
            mode: self.mode,
            sequence: self.sequence,
            success: self.success,
            decision: self.decision,
            reason: self.reason,
            count: self.count,
            topology_sequence: self.topology_sequence,
            sender: self.sender.map(|d| anon.device(d)),
            recipient: self.recipient.map(|d| anon.device(d)),
            origin: self.origin.map(|d| anon.device(d)),
            next_target: self.next_target.map(|d| anon.device(d)),
            flush_reason: self.flush_reason,
            first_sequence: self.first_sequence,
            message_identity,
            scope: self
                .scope
                .iter()
                .flatten()
                .map(|(device, instance)| (anon.device(*device), anon.router_instance(instance)))
                .collect(),
        })
    }
}
fn scope_fingerprint(scope: &oink_routing::control::FocusScope) -> u64 {
    scope
        .members
        .iter()
        .flat_map(|(d, i)| d.0.iter().chain(i.iter()))
        .fold(0xcbf29ce484222325, |hash, b| {
            (hash ^ u64::from(*b)).wrapping_mul(0x100000001b3)
        })
}
impl input_trace::InputTrace {
    pub fn control(&self, local: DeviceId, peer: DeviceId, sent: bool, message: &Message) {
        if !self.enabled() {
            return;
        }
        let Message::Control(control) = message else { return };
        let action = match control {
            ControlMsg::FocusEnter { .. } => {
                if sent {
                    HandoffAction::FocusEnterSent
                } else {
                    HandoffAction::FocusEnterReceived
                }
            }
            ControlMsg::FocusState {
                lock_state: _,
                epoch,
                holder,
                session,
                scope,
            } => {
                if !self.focus_changed(
                    peer,
                    sent,
                    (*epoch, *holder, *session, scope.as_ref().map_or(0, scope_fingerprint)),
                ) {
                    return;
                }
                if sent {
                    HandoffAction::FocusStateSent
                } else {
                    HandoffAction::FocusStateReceived
                }
            }
            ControlMsg::FocusResume { .. } => {
                if sent {
                    HandoffAction::FocusResumeSent
                } else {
                    HandoffAction::FocusResumeReceived
                }
            }
            ControlMsg::CursorTrail { .. } | ControlMsg::CursorTrailHandoff { .. } => {
                if sent {
                    HandoffAction::TrailSent
                } else {
                    HandoffAction::TrailReceived
                }
            }
            _ => return,
        };
        let mut raw = Raw::new(action);
        let scope = match control {
            ControlMsg::FocusEnter { scope, .. } | ControlMsg::FocusState { scope, .. } => scope.as_ref(),
            ControlMsg::FocusResume { scope, .. } => Some(scope),
            _ => None,
        };
        if let Some(scope) = scope {
            for (out, member) in raw.scope.iter_mut().zip(&scope.members) {
                *out = Some(*member);
            }
        }

        raw.peer = Some(peer);
        raw.sender = Some(if sent { local } else { peer });
        raw.recipient = Some(if sent { peer } else { local });
        match control {
            ControlMsg::FocusEnter {
                epoch,
                display,
                pos,
                scope,
                pointer_source,
                ..
            } => {
                raw.origin = *pointer_source;
                raw.epoch = Some(*epoch);
                raw.holder = Some(if sent { peer } else { local });
                raw.screen = Screen::new(if sent { peer } else { local }, display);
                raw.at = Some(*pos);
                raw.instance = scope.as_ref().and_then(|s| {
                    s.members
                        .iter()
                        .find(|(id, _)| *id == if sent { local } else { peer })
                        .map(|(_, instance)| *instance)
                });
                raw.count = scope.as_ref().map_or(0, |s| s.members.len()) as u64;
            }
            ControlMsg::FocusState {
                lock_state: _,
                epoch,
                holder,
                session,
                scope,
            } => {
                raw.epoch = Some(*epoch);
                raw.holder = Some(*holder);
                raw.instance = *session;
                raw.count = scope.as_ref().map_or(0, |s| s.members.len()) as u64;
            }
            ControlMsg::FocusResume { epoch, holder, scope } => {
                raw.epoch = Some(*epoch);
                raw.holder = Some(*holder);
                raw.count = scope.members.len() as u64;
            }
            ControlMsg::CursorTrail { active } => raw.success = Some(*active),
            ControlMsg::CursorTrailHandoff { epoch, sources, state } => {
                raw.epoch = Some(*epoch);
                raw.count = sources.len() as u64;
                raw.at = Some(state.heading);
                self.evidence(input_trace::Evidence::Trail(RawTrail::new(
                    if sent {
                        TrailStage::Converted
                    } else {
                        TrailStage::Received
                    },
                    self.topology_sequence.load(Ordering::Relaxed),
                    *epoch,
                    None,
                    Point::new(0., 0.),
                    Point::new(1., 1.),
                    state,
                )));
            }
            _ => {}
        }
        self.handoff(raw);
    }
    pub fn pair(&self, peer: DeviceId, attached: bool) {
        if !self.enabled() {
            return;
        }
        let mut raw = Raw::new(if attached {
            HandoffAction::PairAttached
        } else {
            HandoffAction::PairDetached
        });
        raw.peer = Some(peer);
        self.handoff(raw);
    }
    pub fn mode(
        &self,
        mode: Option<CaptureMode>,
        restore: Option<Point>,
        sequence: u64,
        started: bool,
        success: Option<bool>,
    ) {
        if !self.enabled() {
            return;
        }
        let mut raw = Raw::new(match (restore.is_some(), started) {
            (false, true) => HandoffAction::ModeStarted,
            (false, false) => HandoffAction::ModeCompleted,
            (true, true) => HandoffAction::RestoreStarted,
            (true, false) => HandoffAction::RestoreCompleted,
        });
        raw.mode = mode;
        raw.at = restore;
        raw.sequence = Some(sequence);
        raw.success = success;
        self.handoff(raw);
    }
    #[cfg(all(test, debug_assertions))]
    pub fn flush(&self, peer: DeviceId, sequence: u64, dx: f64, dy: f64) {
        self.flush_detail(
            peer,
            None,
            None,
            sequence,
            sequence,
            1,
            Point::new(dx, dy),
            FlushReason::Due,
        );
    }
    #[allow(clippy::too_many_arguments)]
    pub fn flush_detail(
        &self,
        peer: DeviceId,
        origin: Option<DeviceId>,
        next: Option<DeviceId>,
        first: u64,
        sequence: u64,
        count: u64,
        delta: Point,
        reason: FlushReason,
    ) {
        if !self.enabled() {
            return;
        }
        let mut raw = Raw::new(HandoffAction::MotionFlush);
        raw.peer = Some(peer);
        raw.sequence = Some(sequence);
        raw.at = Some(delta);
        raw.origin = origin;
        raw.next_target = next;
        raw.first_sequence = Some(first);
        raw.count = count;
        raw.flush_reason = Some(reason);
        self.handoff(raw);
    }
    pub fn output(&self, output: &Output, epoch: u64) {
        if !self.enabled() {
            return;
        }
        let mut raw = match output {
            Output::CursorTrail { active } => {
                let mut r = Raw::new(HandoffAction::TrailActive);
                r.success = Some(*active);
                r
            }
            Output::CursorTrailDepart { exit } => {
                let mut r = Raw::new(HandoffAction::TrailDepart);
                r.at = Some(*exit);
                r
            }
            Output::CursorTrailResume(state) | Output::CursorTrailResumeClipped { state, .. } => {
                let mut r = Raw::new(HandoffAction::TrailResume);
                r.count = state.points.len() as u64;
                r.at = Some(state.heading);
                self.evidence(input_trace::Evidence::Trail(RawTrail::new(
                    TrailStage::Resume,
                    self.topology_sequence.load(Ordering::Relaxed),
                    epoch,
                    None,
                    Point::new(0., 0.),
                    Point::new(1., 1.),
                    state,
                )));
                r
            }
            _ => return,
        };
        raw.epoch = Some(epoch);
        if let Output::CursorTrailResumeClipped { clip, .. } = output {
            raw.clip = Some(*clip);
        }
        self.handoff(raw);
    }
}
impl RoutingDiagnostics for input_trace::InputTrace {
    fn application_focus(&self, record: oink_routing::application_focus::Diagnostic) {
        self.evidence(input_trace::Evidence::ApplicationFocus(record));
    }
    fn source_decision(&self, record: oink_routing::router::SourceDecision) {
        self.evidence(input_trace::Evidence::SourceRouting(record));
    }
    fn volume(&self, record: oink_routing::router::VolumeDecision) {
        self.evidence(input_trace::Evidence::Volume(record));
    }
    fn enabled(&self) -> bool {
        self.enabled()
    }
    fn log(&self, line: u32, message: &'static str, epoch: u64) {
        if self.enabled() {
            self.evidence(input_trace::Evidence::Log { line, message, epoch });
        }
    }
    #[allow(clippy::too_many_arguments)]
    fn state(
        &self,
        phase: &'static str,
        local: DeviceId,
        epoch: u64,
        revision: u64,
        pointer: Option<&ScreenPos>,
        exit: Option<&ScreenPos>,
        reference: Option<&str>,
        displays: &[oink_foundation::display::DisplayInfo],
        layout: &dyn oink_routing::layout::LayoutQuery,
    ) {
        if !self.enabled() {
            return;
        }
        let position = |device, key: &str, at| RawPosition {
            screen: Screen::new(device, key),
            at,
            native_bounds: displays
                .iter()
                .find(|d| device == local && d.key == key)
                .map(|d| d.bounds),
            compiled_bounds: layout.gap_topology().and_then(|t| {
                t.screens()
                    .iter()
                    .find(|s| s.id.device == device && s.id.display == key)
                    .map(|s| s.local)
            }),
        };
        self.evidence(input_trace::Evidence::RouterState(RawRouterState {
            phase,
            epoch,
            revision,
            topology_sequence: self.topology_sequence.load(Ordering::Relaxed),
            pointer: pointer.map(|p| position(p.screen.device, &p.screen.display, Some(p.local))),
            exit: exit.map(|p| position(p.screen.device, &p.screen.display, Some(p.local))),
            reference: reference.map(|key| position(local, key, None)),
        }));
        let mut raw = RawDisplays::new(local, displays, self.topology_sequence.load(Ordering::Relaxed));
        raw.phase = "router_snapshot";
        raw.revision = Some(revision);
        self.evidence(input_trace::Evidence::Displays(raw));
    }
    #[allow(clippy::too_many_arguments)]
    fn crossing(
        &self,
        layout: Arc<dyn oink_routing::layout::LayoutQuery>,
        epoch: u64,
        from: &ScreenPos,
        delta: Point,
        local: bool,
        gap: bool,
        to: &ScreenPos,
        departure: oink_routing::router::DepartureEvidence<'_>,
    ) {
        self.crossing_with_zone(layout, epoch, from, delta, local, gap, false, to, departure);
    }
    #[allow(clippy::too_many_arguments)]
    fn crossing_with_zone(
        &self,
        layout: Arc<dyn oink_routing::layout::LayoutQuery>,
        epoch: u64,
        from: &ScreenPos,
        delta: Point,
        local: bool,
        gap: bool,
        trigger_zone: bool,
        to: &ScreenPos,
        departure: oink_routing::router::DepartureEvidence<'_>,
    ) {
        if !self.enabled() {
            return;
        }
        self.evidence(input_trace::Evidence::Crossing(RawCrossing {
            layout,
            topology_sequence: self.topology_sequence.load(Ordering::Relaxed),
            epoch,
            pointer_source: if local {
                Some(from.screen.device)
            } else {
                departure.pointer_source
            },
            from_screen: Screen::new(from.screen.device, &from.screen.display),
            from: from.local,
            to_screen: Screen::new(to.screen.device, &to.screen.display),
            to: to.local,
            delta,
            local,
            gap,
            trigger_zone,
            departure_bounds: departure.bounds,
            departure_at: departure.point.map(|p| p.local),
            display_revision: departure.revision,
        }));
    }
    fn trail(
        &self,
        stage: oink_routing::router::TrailStage,
        epoch: u64,
        screen: &oink_routing::ScreenId,
        anchor: Point,
        scale: Point,
        state: &oink_routing::control::TrailContinuation,
    ) {
        if !self.enabled() {
            return;
        }
        let stage = match stage {
            oink_routing::router::TrailStage::Source => TrailStage::Source,
            oink_routing::router::TrailStage::Converted => TrailStage::Converted,
        };
        self.evidence(input_trace::Evidence::Trail(RawTrail::new(
            stage,
            self.topology_sequence.load(Ordering::Relaxed),
            epoch,
            Screen::new(screen.device, &screen.display),
            anchor,
            scale,
            state,
        )));
    }
    fn decision(
        &self,
        kind: DecisionKind,
        reason: DecisionReason,
        epoch: u64,
        holder: DeviceId,
        target: Option<&ScreenPos>,
    ) {
        if !self.enabled() {
            return;
        }
        let mut raw = Raw::new(HandoffAction::RoutingDecision);
        raw.decision = Some(kind);
        raw.reason = Some(reason);
        raw.epoch = Some(epoch);
        raw.holder = Some(holder);
        if let Some(target) = target {
            raw.peer = Some(target.screen.device);
            raw.screen = Screen::new(target.screen.device, &target.screen.display);
            raw.at = Some(target.local);
        }
        if self.decision_changed(&raw) {
            self.handoff(raw);
        }
    }
    fn held_buttons(&self, record: oink_routing::router::ButtonRefusal) {
        if self.enabled() {
            self.evidence(input_trace::Evidence::ButtonRefusal(super::buttons::Raw::capture(
                record,
                oink_input::button_diagnostics::sample,
            )));
        }
    }
}

impl input_trace::InputTrace {
    pub(crate) fn topology(
        &self,
        layout: &oink_routing::TopologyLayout,
        group: GroupId,
        source_layout: Arc<oink_display::layout::Layout>,
        owners: Arc<std::collections::BTreeMap<DeviceId, oink_display::layout::OwnerLayoutPreferences>>,
        version: oink_protocol::v1::dto::ConfigVersion,
        input_online: Option<std::collections::BTreeSet<DeviceId>>,
    ) {
        // Topology changes are low-frequency lifecycle evidence, independent of
        // input tracing. Keep native work bounded; projection happens on the drain.
        self.topology_change(layout, group, source_layout, owners, version, input_online);
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CrossingEvent {
    pub topology_sequence: u64,
    pub epoch: u64,
    pub from_screen: Option<String>,
    pub from: Point,
    pub target_screen: Option<String>,
    pub final_position: Point,
    pub delta: Point,
    pub delta_is_local: bool,
    pub gap_travel: bool,
    pub trigger_zone: bool,
    pub first_exit: Option<Point>,
    pub first_entry: Option<Point>,
    pub first_target_screen: Option<String>,
    pub side: Option<oink_display::layout::Side>,
    pub gap_distance: Option<f64>,
    pub remaining_reference_motion: Option<Point>,
    pub departure_clamp: Option<DepartureClamp>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DepartureClamp {
    pub requested: Point,
    pub applied: Point,
    pub bounds: Rect,
    pub display_revision: u64,
}
pub(super) struct RawCrossing {
    pub pointer_source: Option<DeviceId>,
    pub layout: Arc<dyn oink_routing::layout::LayoutQuery>,
    pub topology_sequence: u64,
    pub epoch: u64,
    pub from_screen: Option<Screen>,
    pub from: Point,
    pub to_screen: Option<Screen>,
    pub to: Point,
    pub delta: Point,
    pub local: bool,
    pub gap: bool,
    pub trigger_zone: bool,
    pub departure_bounds: Option<Rect>,
    pub departure_at: Option<Point>,
    pub display_revision: u64,
}
impl RawCrossing {
    pub(super) fn export(self, recorder: &Recorder) -> Event {
        let anon = recorder.anonymizer();
        let name = |s: Screen| std::str::from_utf8(&s.bytes[..s.len as usize]).unwrap_or("").to_owned();
        let first = self.from_screen.and_then(|screen| {
            let from = ScreenPos::new(oink_routing::ScreenId::new(screen.device, name(screen)), self.from);
            let k = if self.local {
                self.layout.reference_per_native_unit(&from.screen)?
            } else {
                1.
            };
            let delta = (self.delta.x * k, self.delta.y * k);
            if let Some(source) = self.pointer_source {
                self.layout.first_portal_from_source(&from, delta, source)
            } else {
                self.layout.first_portal(&from, delta)
            }
        });
        Event::Crossing(CrossingEvent {
            topology_sequence: self.topology_sequence,
            epoch: self.epoch,
            from_screen: self.from_screen.map(|s| anon.screen(s.device, &name(s))),
            from: self.from,
            target_screen: self.to_screen.map(|s| anon.screen(s.device, &name(s))),
            final_position: self.to,
            delta: self.delta,
            delta_is_local: self.local,
            gap_travel: self.gap,
            trigger_zone: self.trigger_zone,
            first_exit: first.as_ref().map(|p| p.from.local),
            first_entry: first.as_ref().map(|p| p.to.local),
            first_target_screen: first
                .as_ref()
                .map(|p| anon.screen(p.to.screen.device, &p.to.screen.display)),
            side: first.as_ref().map(|p| p.side),
            gap_distance: first.as_ref().map(|p| p.gap_distance),
            remaining_reference_motion: first.map(|p| Point::new(p.remaining.0, p.remaining.1)),
            departure_clamp: if !self.local && !self.gap {
                self.from_screen.and_then(|screen| {
                    let from = ScreenPos::new(oink_routing::ScreenId::new(screen.device, name(screen)), self.from);
                    let plane = self.layout.to_plane(&from)?;
                    let requested = self
                        .layout
                        .from_plane(&from.screen, Point::new(plane.x + self.delta.x, plane.y + self.delta.y))?;
                    Some(DepartureClamp {
                        requested,
                        applied: self.departure_at?,
                        bounds: self.departure_bounds?,
                        display_revision: self.display_revision,
                    })
                })
            } else {
                None
            },
        })
    }
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub enum TrailStage {
    Source,
    Converted,
    Received,
    Resume,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TrailEvent {
    pub stage: TrailStage,
    pub topology_sequence: u64,
    pub epoch: u64,
    pub screen: Option<String>,
    pub anchor: Point,
    pub scale: Point,
    pub shown_secs: f64,
    pub size_scale: f64,
    pub native_units_per_reference: f64,
    pub emit_carry: f64,
    pub heading: Point,
    pub points: Vec<oink_routing::control::TrailPoint>,
}
#[derive(Clone, Copy)]
struct TrailPoint {
    age: f64,
    point: Point,
}
pub(super) struct RawTrail {
    stage: TrailStage,
    topology_sequence: u64,
    epoch: u64,
    screen: Option<Screen>,
    anchor: Point,
    scale: Point,
    shown_secs: f64,
    size_scale: f64,
    native_units_per_reference: f64,
    emit_carry: f64,
    heading: Point,
    points: [TrailPoint; 64],
    length: usize,
}
impl RawTrail {
    fn new(
        stage: TrailStage,
        topology_sequence: u64,
        epoch: u64,
        screen: Option<Screen>,
        anchor: Point,
        scale: Point,
        state: &oink_routing::control::TrailContinuation,
    ) -> Self {
        let mut points = [TrailPoint {
            age: 0.,
            point: Point::new(0., 0.),
        }; 64];
        let length = state.points.len().min(64);
        for (output, input) in points.iter_mut().zip(&state.points) {
            output.age = input.age_secs;
            output.point = input.pos;
        }
        Self {
            stage,
            topology_sequence,
            epoch,
            screen,
            anchor,
            scale,
            shown_secs: state.shown_secs,
            size_scale: state.size_scale,
            native_units_per_reference: state.native_units_per_reference,
            emit_carry: state.emit_carry,
            heading: state.heading,
            points,
            length,
        }
    }
    pub(super) fn export(self, recorder: &Recorder) -> Event {
        Event::Trail(TrailEvent {
            stage: self.stage,
            topology_sequence: self.topology_sequence,
            epoch: self.epoch,
            screen: self.screen.map(|s| {
                recorder
                    .anonymizer()
                    .screen(s.device, std::str::from_utf8(&s.bytes[..s.len as usize]).unwrap_or(""))
            }),
            anchor: self.anchor,
            scale: self.scale,
            shown_secs: self.shown_secs,
            size_scale: self.size_scale,
            native_units_per_reference: self.native_units_per_reference,
            emit_carry: self.emit_carry,
            heading: self.heading,
            points: self.points[..self.length]
                .iter()
                .map(|p| oink_routing::control::TrailPoint {
                    age_secs: p.age,
                    pos: p.point,
                })
                .collect(),
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DisplayBounds {
    pub screen: String,
    pub bounds: Rect,
    pub pixel_width: u32,
    pub pixel_height: u32,
    pub pixels_per_unit: Point,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DisplaysEvent {
    pub phase: String,
    pub revision: Option<u64>,
    pub device: String,
    pub topology_sequence: u64,
    pub screens: Vec<DisplayBounds>,
    pub truncated: usize,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PositionEvidence {
    pub screen: Option<String>,
    pub at: Option<Point>,
    pub native_bounds: Option<Rect>,
    pub compiled_bounds: Option<Rect>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RouterStateEvent {
    pub phase: String,
    pub epoch: u64,
    pub display_revision: u64,
    pub topology_sequence: u64,
    pub pointer: Option<PositionEvidence>,
    pub exit: Option<PositionEvidence>,
    pub reference: Option<PositionEvidence>,
}
#[derive(Clone, Copy)]
struct RawPosition {
    screen: Option<Screen>,
    at: Option<Point>,
    native_bounds: Option<Rect>,
    compiled_bounds: Option<Rect>,
}
pub(super) struct RawRouterState {
    phase: &'static str,
    epoch: u64,
    revision: u64,
    topology_sequence: u64,
    pointer: Option<RawPosition>,
    exit: Option<RawPosition>,
    reference: Option<RawPosition>,
}
impl RawRouterState {
    pub(super) fn export(self, recorder: &Recorder) -> Event {
        let anon = recorder.anonymizer();
        let position = |p: RawPosition| PositionEvidence {
            screen: p
                .screen
                .map(|s| anon.screen(s.device, std::str::from_utf8(&s.bytes[..s.len as usize]).unwrap_or(""))),
            at: p.at,
            native_bounds: p.native_bounds,
            compiled_bounds: p.compiled_bounds,
        };
        Event::RouterState(RouterStateEvent {
            phase: self.phase.into(),
            epoch: self.epoch,
            display_revision: self.revision,
            topology_sequence: self.topology_sequence,
            pointer: self.pointer.map(position),
            exit: self.exit.map(position),
            reference: self.reference.map(position),
        })
    }
}
pub(super) struct RawDisplays {
    device: DeviceId,
    topology_sequence: u64,
    screens: [Option<Screen>; 16],
    bounds: [Rect; 16],
    pixels: [(u32, u32); 16],
    phase: &'static str,
    revision: Option<u64>,
    length: usize,
    truncated: usize,
}
impl RawDisplays {
    fn new(device: DeviceId, displays: &[oink_foundation::display::DisplayInfo], topology_sequence: u64) -> Self {
        let length = displays.len().min(16);
        let mut raw = Self {
            device,
            topology_sequence,
            screens: [None; 16],
            bounds: [Rect::new(0., 0., 0., 0.); 16],
            pixels: [(0, 0); 16],
            phase: "input_applied",
            revision: None,
            length,
            truncated: displays.len() - length,
        };
        for (i, d) in displays.iter().take(16).enumerate() {
            raw.screens[i] = Screen::new(device, &d.key);
            raw.bounds[i] = d.bounds;
            raw.pixels[i] = (d.pixel_width, d.pixel_height);
        }
        raw
    }
    pub(super) fn export(self, recorder: &Recorder) -> Event {
        let anon = recorder.anonymizer();
        Event::Displays(DisplaysEvent {
            phase: self.phase.into(),
            revision: self.revision,
            device: anon.device(self.device),
            topology_sequence: self.topology_sequence,
            screens: self.screens[..self.length]
                .iter()
                .zip(&self.bounds)
                .zip(&self.pixels)
                .filter_map(|((screen, bounds), (pixel_width, pixel_height))| {
                    screen.map(|s| DisplayBounds {
                        screen: anon.screen(s.device, std::str::from_utf8(&s.bytes[..s.len as usize]).unwrap_or("")),
                        bounds: *bounds,
                        pixel_width: *pixel_width,
                        pixel_height: *pixel_height,
                        pixels_per_unit: Point::new(
                            *pixel_width as f64 / bounds.w.max(1.),
                            *pixel_height as f64 / bounds.h.max(1.),
                        ),
                    })
                })
                .collect(),
            truncated: self.truncated,
        })
    }
}
impl input_trace::InputTrace {
    pub fn displays(&self, device: DeviceId, displays: &[oink_foundation::display::DisplayInfo]) {
        if !self.enabled() {
            return;
        }
        let fingerprint = displays.iter().fold(0xcbf29ce484222325, |hash, d| {
            d.key
                .as_bytes()
                .iter()
                .copied()
                .chain(
                    [d.bounds.x, d.bounds.y, d.bounds.w, d.bounds.h]
                        .into_iter()
                        .flat_map(|n| n.to_bits().to_le_bytes()),
                )
                .chain(d.pixel_width.to_le_bytes())
                .chain(d.pixel_height.to_le_bytes())
                .fold(hash, |hash, b| (hash ^ u64::from(b)).wrapping_mul(0x100000001b3))
        });
        if !self.displays_changed(device, fingerprint) {
            return;
        }
        let raw = RawDisplays::new(device, displays, self.topology_sequence.load(Ordering::Relaxed));
        self.evidence(input_trace::Evidence::Displays(raw));
    }
    pub fn mode_requested(&self, mode: Option<CaptureMode>, at: Option<Point>, sequence: u64) {
        if !self.enabled() {
            return;
        }
        let mut raw = Raw::new(HandoffAction::ModeRequested);
        raw.mode = mode;
        raw.at = at;
        raw.sequence = Some(sequence);
        self.handoff(raw);
    }
}

impl Recorder {
    pub(crate) fn display_enumerated(&self, displays: &[oink_foundation::display::DisplayInfo], revision: u64) {
        if !self.enabled() {
            return;
        }
        let anon = self.anonymizer();
        self.record(Event::Displays(DisplaysEvent {
            phase: "native_enumerated".into(),
            revision: Some(revision),
            device: anon.device(self.inner.me),
            topology_sequence: 0,
            truncated: 0,
            screens: displays
                .iter()
                .map(|d| DisplayBounds {
                    screen: anon.screen(self.inner.me, &d.key),
                    bounds: d.bounds,
                    pixel_width: d.pixel_width,
                    pixel_height: d.pixel_height,
                    pixels_per_unit: Point::new(
                        d.pixel_width as f64 / d.bounds.w.max(1.),
                        d.pixel_height as f64 / d.bounds.h.max(1.),
                    ),
                })
                .collect(),
        }));
    }
}
