//! Bounded in-memory input evidence. Native owners only copy numeric event fields;
//! the ordinary consumer anonymizes, correlates presses, formats and writes them.
use super::*;
use oink_foundation::input::{CapturedEvent, InjectEvent};
use oink_routing::{Packet as Message, input::InputMsg};

// Not serializable: raw key codes may exist only in this transient bounded queue.
enum Record {
    Capture(
        CapturedEvent,
        oink_input::diagnostics::CaptureMetadata,
        Option<oink_foundation::input::Disposition>,
        Option<oink_routing::router::ContactsPath>,
    ),
    Injection(InjectEvent, Option<DeviceId>, bool),
    Remote(DeviceId, InputMsg),
    Held(HeldRecord),
    Contacts(oink_input::contact_trace::Observation),
}
#[derive(Clone, Copy)]
struct HeldRecord {
    peer: DeviceId,
    source: DeviceId,
    sent: bool,
    physical: bool,
    keys: usize,
    buttons: usize,
    modifiers: u8,
    codes: [Option<oink_input::KeyCode>; 16],
    observations: u64,
}
struct HeldSample {
    counts: (usize, usize, u8),
    at: std::time::Instant,
    observations: u64,
}
type FocusKey = (u64, DeviceId, Option<[u8; 16]>, u64);
struct Dedup {
    focus: std::collections::HashMap<(DeviceId, bool), FocusKey>,
    held: std::collections::HashMap<(DeviceId, DeviceId, bool), HeldSample>,
    decision: Option<super::handoff::Raw>,
    displays: std::collections::HashMap<DeviceId, u64>,
    topology: Option<super::layout::RouterCompilation>,
    topology_owners: Option<Arc<std::collections::BTreeMap<DeviceId, oink_display::layout::OwnerLayoutPreferences>>>,
}
#[allow(clippy::large_enum_variant)] // fixed bounded storage; producers cannot allocate boxes
pub(super) enum Evidence {
    Timing(oink_foundation::handoff_timing::Event),
    Volume(oink_routing::router::VolumeDecision),
    SourceRouting(oink_routing::router::SourceDecision),
    Dnd(super::dnd::Raw),
    Log {
        line: u32,
        message: &'static str,
        epoch: u64,
    },
    Handoff(super::handoff::Raw),
    Topology(super::layout::RouterCompilation),
    Crossing(super::handoff::RawCrossing),
    Trail(super::handoff::RawTrail),
    Displays(super::handoff::RawDisplays),
    RouterState(super::handoff::RawRouterState),
    ButtonRefusal(super::buttons::Raw),
}
struct StampedEvidence {
    stamp: clock::Stamp,
    record: Evidence,
}
struct Stamped {
    stamp: clock::Stamp,
    record: Record,
}
pub(crate) struct InputTrace {
    enabled: AtomicBool,
    queue: ArrayQueue<Stamped>,
    clock: Arc<clock::Clock>,
    evidence: ArrayQueue<StampedEvidence>,
    pub(super) topology_sequence: AtomicU64,
    pub(crate) config: std::sync::OnceLock<crate::config_module::ConfigModule>,
    dedup: ArrayQueue<Dedup>,
    pub(super) last_dnd_state: std::sync::atomic::AtomicU8,
    dropped: AtomicU64,
    wake: std::sync::OnceLock<Arc<dyn Fn() + Send + Sync>>,
    pending: AtomicBool,
}
impl oink_foundation::handoff_timing::Observer for InputTrace {
    fn enabled(&self) -> bool {
        self.enabled()
    }
    fn record(&self, event: oink_foundation::handoff_timing::Event) {
        self.evidence(Evidence::Timing(event));
    }
}
fn timing_event(record: oink_foundation::handoff_timing::Event) -> Event {
    Event::Log {
        module: "handoff_timing".into(),
        line: 0,
        level: 2,
        fields: vec![
            ("stage".into(), record.stage as u16 as f64),
            ("epoch".into(), record.token.epoch as f64),
            ("sequence".into(), record.token.sequence as f64),
            ("link".into(), record.link as f64),
            ("stream".into(), record.stream as f64),
            ("elapsed_us".into(), record.elapsed_us as f64),
            ("code".into(), f64::from(record.code)),
            ("sent_packets".into(), record.sent_packets as f64),
            ("lost_packets".into(), record.lost_packets as f64),
            ("rtt_us".into(), record.rtt_us as f64),
            ("count".into(), record.count as f64),
        ],
        labels: vec![],
    }
}
// Compare only typed geometry. Display names never affect a topology sequence.
fn screen_geometry(s: &oink_display::layout::ScreenPlacement) -> impl PartialEq + '_ {
    (&s.id, s.local, s.rect, s.size_source, s.primary, s.builtin)
}
fn effective_geometry(s: &oink_display::layout::EffectiveScreen) -> impl PartialEq + '_ {
    (screen_geometry(&s.placement), s.enabled, s.gain, s.speed_mode)
}
impl InputTrace {
    #[cfg(all(test, debug_assertions))]
    pub fn new(enabled: bool) -> Arc<Self> {
        Self::with_clock(enabled, clock::Clock::system())
    }
    pub fn for_recorder(recorder: &Recorder) -> Arc<Self> {
        Self::with_clock(recorder.enabled(), recorder.inner.clock.clone())
    }
    fn with_clock(enabled: bool, clock: Arc<clock::Clock>) -> Arc<Self> {
        let dedup = ArrayQueue::new(1);
        let _ = dedup.push(Dedup {
            focus: std::collections::HashMap::with_capacity(64),
            held: std::collections::HashMap::with_capacity(64),
            decision: None,
            displays: std::collections::HashMap::with_capacity(16),
            topology: None,
            topology_owners: None,
        });
        Arc::new(Self {
            enabled: AtomicBool::new(enabled),
            queue: ArrayQueue::new(8192),
            clock,
            evidence: ArrayQueue::new(512),
            topology_sequence: AtomicU64::new(0),
            config: Default::default(),
            dedup,
            last_dnd_state: std::sync::atomic::AtomicU8::new(u8::MAX),
            dropped: AtomicU64::new(0),
            wake: std::sync::OnceLock::new(),
            pending: AtomicBool::new(false),
        })
    }
    pub fn set_wake(&self, wake: Arc<dyn Fn() + Send + Sync>) {
        let _ = self.wake.set(wake);
    }
    fn push(&self, record: Record) {
        if !self.enabled.load(Ordering::Relaxed) {
            return;
        }
        if self
            .queue
            .push(Stamped {
                stamp: self.clock.now(),
                record,
            })
            .is_err()
        {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        if !self.pending.swap(true, Ordering::AcqRel)
            && let Some(wake) = self.wake.get()
        {
            wake();
        }
    }
    #[cfg(all(test, debug_assertions))]
    pub fn captured(&self, event: &CapturedEvent) {
        if matches!(event.event, oink_input::InputEvent::TouchpadSample(_)) {
            return;
        }
        if !self.enabled() {
            return;
        }
        self.push(Record::Capture(
            event.clone(),
            oink_input::diagnostics::capture_metadata(),
            None,
            None,
        ));
    }
    pub fn injected(&self, event: &InjectEvent, peer: Option<DeviceId>, success: bool) {
        if matches!(event, InjectEvent::TouchpadContacts(_)) {
            return;
        }
        if !self.enabled() {
            return;
        }
        self.push(Record::Injection(event.clone(), peer, success));
    }
    pub fn remote(&self, mut peer: DeviceId, message: &Message) {
        if !self.enabled() {
            return;
        }
        let previous_hop = peer;
        let (Message::Input(message) | Message::TimedInput { input: message, .. }) = message else {
            return;
        };
        let mut message = message;
        let mut depth = 0;
        while let InputMsg::Forwarded { origin, msg, .. } = message {
            if depth == 8 {
                return;
            }
            depth += 1;
            peer = origin.device_id;
            message = msg;
        }
        if matches!(message, InputMsg::TouchpadContacts(_)) {
            return;
        }
        if let InputMsg::HeldSync { keys, buttons } = message {
            self.held_sync((previous_hop, peer, false, false), keys, buttons);
        } else {
            self.push(Record::Remote(peer, message.clone()));
        }
    }
    pub fn sent_held_sync(&self, local: DeviceId, peer: DeviceId, message: &Message) {
        let (Message::Input(input) | Message::TimedInput { input, .. }) = message else {
            return;
        };
        let mut input = input;
        let mut source = local;
        while let InputMsg::Forwarded { origin, msg, .. } = input {
            source = origin.device_id;
            input = msg;
        }
        if let InputMsg::HeldSync { keys, buttons } = input {
            self.held_sync((peer, source, true, source == local), keys, buttons);
        }
    }
    fn held_sync(
        &self,
        (peer, source, sent, physical): (DeviceId, DeviceId, bool, bool),
        keys: &[oink_input::KeyCode],
        buttons: &[oink_input::MouseButton],
    ) {
        if !self.enabled() {
            return;
        }
        let modifiers = keys
            .iter()
            .filter(|key| key.is_modifier())
            .fold(0u8, |mask, key| mask | (1 << (key.usage() - 0xe0)));
        let counts = (keys.len(), buttons.len(), modifiers);
        let Some(mut cache) = self.dedup.pop() else {
            return;
        };
        let key = (peer, source, sent);
        let capacity = cache.held.len() < 64;
        let old = cache.held.get_mut(&key);
        let now = std::time::Instant::now();
        let (record, observations) = match old {
            Some(old) => {
                old.observations += 1;
                let record = old.counts != counts || (counts != (0, 0, 0) && now.duration_since(old.at).as_secs() >= 1);
                let observations = old.observations;
                if record {
                    old.counts = counts;
                    old.at = now;
                    old.observations = 0;
                }
                (record, observations)
            }
            None if counts != (0, 0, 0) && capacity => {
                cache.held.insert(
                    key,
                    HeldSample {
                        counts,
                        at: now,
                        observations: 0,
                    },
                );
                (true, 1)
            }
            _ => (false, 0), // preserve idle-owner dedup: no empty-snapshot wakeups
        };
        let _ = self.dedup.push(cache);
        if record {
            let mut codes = [None; 16];
            for (slot, code) in codes.iter_mut().zip(keys) {
                *slot = Some(*code);
            }
            self.push(Record::Held(HeldRecord {
                peer,
                source,
                sent,
                physical,
                keys: keys.len(),
                buttons: buttons.len(),
                modifiers,
                codes,
                observations,
            }));
        }
    }

    pub fn drain(&self, recorder: &Recorder) {
        self.pending.store(false, Ordering::Release);
        let enabled = recorder.enabled();
        if enabled
            && !self.enabled.swap(enabled, Ordering::Relaxed)
            && let Some(mut cache) = self.dedup.pop()
        {
            cache.focus.clear();
            cache.held.clear();
            cache.decision = None;
            self.last_dnd_state.store(u8::MAX, Ordering::Relaxed);
            cache.displays.clear();
            let _ = self.dedup.push(cache);
        }
        self.enabled.store(enabled, Ordering::Relaxed);
        let dropped = self.dropped.swap(0, Ordering::Relaxed);
        if dropped != 0 {
            recorder.inner.press_generation.fetch_add(1, Ordering::Relaxed);
            recorder.inner.dropped.fetch_add(dropped, Ordering::Relaxed);
        }
        for _ in 0..8192 {
            let Some(stamped) = self.queue.pop() else { break };
            recorder.with_capture_stamp(stamped.stamp, || match stamped.record {
                Record::Capture(event, metadata, outcome, path) => {
                    recorder.captured_outcome(&event, None, metadata, outcome, path)
                }
                Record::Contacts(observation) => recorder.record(super::contacts::event(observation)),
                Record::Injection(event, peer, success) => recorder.injected(&event, peer, success),
                Record::Remote(peer, message) => recorder.remote_message(peer, &Message::Input(message)),
                Record::Held(held) => recorder.record(Event::HeldSync {
                    peer: recorder.anonymizer().device(held.peer),
                    source: recorder.anonymizer().device(held.source),
                    sent: held.sent,
                    keys: held.keys,
                    buttons: held.buttons,
                    modifiers: held.modifiers,
                    press_ids: recorder.held_press_ids(&held.codes, held.physical, held.source),
                    observations: held.observations,
                }),
            });
        }
        for _ in 0..512 {
            let Some(stamped) = self.evidence.pop() else { break };
            if let Evidence::Topology(compiled) = stamped.record {
                if let Some(config) = self.config.get() {
                    config.router_compiled(compiled);
                }
                continue;
            }
            recorder.with_capture_stamp(stamped.stamp, || {
                recorder.record(match stamped.record {
                    Evidence::Timing(record) => timing_event(record),
                    Evidence::SourceRouting(record) => Event::SourceRouting(record),
                    Evidence::Volume(record) => Event::VolumeRouting(super::volume::export(recorder, record)),
                    Evidence::Dnd(raw) => raw.export(recorder),
                    Evidence::Log { line, message, epoch } => Event::Log {
                        module: "oink_routing::router".into(),
                        line,
                        level: 2,
                        fields: vec![("epoch".into(), epoch as f64)],
                        labels: vec![("message".into(), message.into())],
                    },
                    Evidence::Handoff(raw) => raw.export(recorder),
                    Evidence::Topology(_) => unreachable!(),
                    Evidence::Crossing(raw) => raw.export(recorder),
                    Evidence::Trail(raw) => raw.export(recorder),
                    Evidence::Displays(raw) => raw.export(recorder),
                    Evidence::RouterState(raw) => raw.export(recorder),
                    Evidence::ButtonRefusal(raw) => raw.export(recorder),
                })
            });
        }
        if !self.queue.is_empty() || !self.evidence.is_empty() {
            self.notify();
        }
    }
    pub(super) fn topology_change(
        &self,
        layout: &oink_routing::TopologyLayout,
        group: GroupId,
        source_layout: Arc<oink_display::layout::Layout>,
        owners: Arc<std::collections::BTreeMap<DeviceId, oink_display::layout::OwnerLayoutPreferences>>,
        version: oink_protocol::v1::dto::ConfigVersion,
        input_online: Option<std::collections::BTreeSet<DeviceId>>,
    ) {
        let Some(mut cache) = self.dedup.pop() else {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let unchanged = cache.topology.as_ref().is_some_and(|old| {
            let previous = old.topology.topology();
            let current = layout.topology();
            old.group == group
                && old.version == version
                && cache.topology_owners.as_ref() == Some(&owners)
                && previous.settings() == current.settings()
                && old.input_online == input_online
                && old
                    .source_layout
                    .screens
                    .iter()
                    .map(screen_geometry)
                    .eq(source_layout.screens.iter().map(screen_geometry))
                && previous
                    .screens()
                    .iter()
                    .map(effective_geometry)
                    .eq(current.screens().iter().map(effective_geometry))
                && current.screens().iter().all(|s| {
                    previous.is_active(&s.id) == current.is_active(&s.id)
                        && old.topology.input_online(s.id.device) == layout.input_online(s.id.device)
                })
        });
        if unchanged {
            let _ = self.dedup.push(cache);
            return;
        }
        let sequence = self.topology_sequence.fetch_add(1, Ordering::Relaxed) + 1;
        let compiled = super::layout::RouterCompilation {
            group,
            version,
            sequence,
            stage: super::layout::CompilationStage::Router,
            topology: layout.clone(),
            source_layout,
            input_online,
        };
        cache.topology = Some(compiled.clone());
        cache.topology_owners = Some(owners);
        let _ = self.dedup.push(cache);
        self.topology_evidence(compiled);
    }
    pub(super) fn topology_evidence(&self, compiled: super::layout::RouterCompilation) {
        if self
            .evidence
            .push(StampedEvidence {
                stamp: self.clock.now(),
                record: Evidence::Topology(compiled),
            })
            .is_err()
        {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        if !self.pending.swap(true, Ordering::AcqRel)
            && let Some(wake) = self.wake.get()
        {
            wake();
        }
    }
    #[cfg(all(test, debug_assertions))]
    pub(super) fn disable_for_test(&self) {
        self.enabled.store(false, Ordering::Relaxed);
    }
    pub(super) fn evidence(&self, record: Evidence) {
        if !self.enabled() {
            return;
        }
        if self
            .evidence
            .push(StampedEvidence {
                stamp: self.clock.now(),
                record,
            })
            .is_err()
        {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        self.notify();
    }
    fn notify(&self) {
        if !self.pending.swap(true, Ordering::AcqRel)
            && let Some(wake) = self.wake.get()
        {
            wake();
        }
    }
    pub(crate) fn begin_capture(&self) -> Option<(clock::Stamp, oink_input::diagnostics::CaptureMetadata)> {
        self.enabled()
            .then(|| (self.clock.now(), oink_input::diagnostics::capture_metadata()))
    }
    pub(crate) fn finish_capture(
        &self,
        event: &CapturedEvent,
        captured: Option<(clock::Stamp, oink_input::diagnostics::CaptureMetadata)>,
        outcome: oink_foundation::input::Disposition,
    ) {
        self.finish_capture_path(event, captured, outcome, None);
    }
    pub(crate) fn finish_capture_path(
        &self,
        event: &CapturedEvent,
        captured: Option<(clock::Stamp, oink_input::diagnostics::CaptureMetadata)>,
        outcome: oink_foundation::input::Disposition,
        path: Option<oink_routing::router::ContactsPath>,
    ) {
        let Some((stamp, metadata)) = captured else { return };
        if !self.enabled() {
            return;
        }
        if self
            .queue
            .push(Stamped {
                stamp,
                record: if let oink_input::InputEvent::TouchpadSample(sample) = &event.event {
                    Record::Contacts(oink_input::contact_trace::Observation::capture(sample))
                } else {
                    Record::Capture(event.clone(), metadata, Some(outcome), path)
                },
            })
            .is_err()
        {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        self.notify();
    }
    pub(crate) fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }
    pub(super) fn handoff(&self, raw: super::handoff::Raw) {
        let mut raw = raw;
        raw.topology_sequence = self.topology_sequence.load(Ordering::Relaxed);
        self.evidence(Evidence::Handoff(raw));
    }
    pub(super) fn focus_changed(&self, peer: DeviceId, sent: bool, key: FocusKey) -> bool {
        let Some(mut cache) = self.dedup.pop() else {
            return false;
        };
        let old = cache.focus.get(&(peer, sent));
        let changed = old != Some(&key);
        if changed && (old.is_some() || cache.focus.len() < 64) {
            cache.focus.insert((peer, sent), key);
        }
        let _ = self.dedup.push(cache);
        changed
    }
    pub(super) fn displays_changed(&self, device: DeviceId, value: u64) -> bool {
        let Some(mut cache) = self.dedup.pop() else {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        };
        let changed = cache.displays.get(&device) != Some(&value);
        if changed && (cache.displays.contains_key(&device) || cache.displays.len() < 16) {
            cache.displays.insert(device, value);
        }
        let _ = self.dedup.push(cache);
        changed
    }
    pub(super) fn decision_changed(&self, raw: &super::handoff::Raw) -> bool {
        let Some(mut cache) = self.dedup.pop() else {
            return false;
        };
        let changed = cache.decision.as_ref().is_none_or(|old| {
            old.decision != raw.decision
                || old.reason != raw.reason
                || old.peer != raw.peer
                || old.epoch != raw.epoch
                || old.screen != raw.screen
        });
        if changed {
            cache.decision = Some(*raw);
        }
        let _ = self.dedup.push(cache);
        changed
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    #[test]
    fn crossing_timing_is_numeric_projectable_and_obeys_recorder_switch() {
        use super::*;
        use oink_foundation::handoff_timing::{Event as TimingEvent, Observer, Stage, Token};
        let trace = InputTrace::new(true);
        let event = TimingEvent::new(
            Stage::MotionPacing,
            Token {
                epoch: 37,
                sequence: 42,
            },
        );
        Observer::record(&*trace, event);
        let row = trace.evidence.pop().unwrap();
        let Evidence::Timing(record) = row.record else {
            panic!("numeric timing record missing")
        };
        assert_eq!(record, event);
        let projected = timing_event(record);
        assert!(projected.network(false).is_ok());
        assert!(projected.network(true).is_ok());
        let Event::Log { fields, labels, .. } = projected else {
            unreachable!()
        };
        assert!(labels.is_empty());
        assert_eq!(fields.len(), 11);
        assert!(fields.iter().all(|(_, value)| value.is_finite()));
        trace.disable_for_test();
        Observer::record(&*trace, event);
        assert!(trace.evidence.is_empty());
    }
    #[test]
    fn source_only_policy_changes_are_not_deduplicated_with_empty_targets() {
        use oink_display::layout::Layout;
        let trace = super::InputTrace::new(false);
        let group = super::GroupId([2; 16]);
        let source = std::sync::Arc::new(Layout::default());
        let peer = super::DeviceId([3; 32]);
        for shared in [false, true] {
            let owners = std::sync::Arc::new(
                [(
                    peer,
                    oink_display::layout::OwnerLayoutPreferences {
                        screen_shared: shared,
                        ..Default::default()
                    },
                )]
                .into(),
            );
            let topology = oink_routing::TopologyLayout::from_effective(&source.effective(&owners), |_| true);
            trace.topology(
                &topology,
                group,
                source.clone(),
                owners,
                super::super::tests::wire_version(0),
                None,
            );
        }
        assert_eq!(
            trace.evidence.len(),
            2,
            "headless owner policy changed without target geometry"
        );
    }
    #[test]
    fn display_names_do_not_advance_topology_sequences() {
        use oink_display::layout::{Layout, ScreenPlacement};
        let trace = super::InputTrace::new(false);
        let mut layout = Layout::default();
        layout.screens.push(ScreenPlacement::from_display(
            super::DeviceId([1; 32]),
            &crate::config_module::runtime_tests::display("PRIVATE"),
            oink_foundation::Point::new(0., 0.),
        ));
        for name in ["PRIVATE-ONE", "PRIVATE-TWO"] {
            layout.screens[0].name = name.into();
            trace.topology(
                &oink_routing::TopologyLayout::from_layout(&layout, &Default::default()),
                super::GroupId([2; 16]),
                std::sync::Arc::new(layout.clone()),
                Default::default(),
                super::super::tests::wire_version(0),
                None,
            );
        }
        assert_eq!(trace.evidence.len(), 1);
        assert_eq!(trace.topology_sequence.load(std::sync::atomic::Ordering::Relaxed), 1);
    }
    use super::*;
    use oink_foundation::input::{InputEvent, KeyCode, SourceKind};
    use std::io::Read;

    #[test]
    fn held_sync_records_direction_modifiers_press_ids_and_liveness_without_idle_wakes() {
        let dir = tempfile::tempdir().unwrap();
        let local = DeviceId([1; 32]);
        let peer = DeviceId([2; 32]);
        let recorder = Recorder::start(dir.path().into(), local, Some(GroupId([2; 16])), true).unwrap();
        recorder.wait_recovery();
        let trace = InputTrace::new(true);
        let code = KeyCode::LEFT_GUI;
        trace.captured(&CapturedEvent {
            event: InputEvent::Key { code, pressed: true },
            physical: true,
            source: SourceKind::Keyboard,
            time_us: 1,
            cursor: None,
        });
        let held = |keys| Message::Input(InputMsg::HeldSync { keys, buttons: vec![] });
        trace.sent_held_sync(local, peer, &held(vec![code]));
        trace.remote(peer, &Message::Input(InputMsg::Key { code, pressed: true }));
        trace.remote(peer, &held(vec![code]));
        trace.drain(&recorder);
        for _ in 0..4 {
            trace.sent_held_sync(local, peer, &held(vec![code]));
        }
        assert!(trace.queue.is_empty());
        let mut cache = trace.dedup.pop().unwrap();
        cache.held.get_mut(&(peer, local, true)).unwrap().at -= std::time::Duration::from_secs(1);
        trace.dedup.push(cache).ok().unwrap();
        trace.sent_held_sync(local, peer, &held(vec![code]));
        trace.sent_held_sync(local, peer, &held(vec![KeyCode::LEFT_CTRL])); // same count, different modifier
        trace.sent_held_sync(local, peer, &held(vec![]));
        trace.drain(&recorder);
        for _ in 0..100 {
            trace.sent_held_sync(local, peer, &held(vec![]));
        }
        assert!(trace.queue.is_empty());
        let bundle = recorder.save(Reason::Manual).unwrap();
        let mut zip = zip::ZipArchive::new(std::fs::File::open(bundle.path).unwrap()).unwrap();
        let mut text = String::new();
        zip.by_name("merged.ndjson").unwrap().read_to_string(&mut text).unwrap();
        let held_rows: Vec<serde_json::Value> = text
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .filter_map(|row| row["event"].get("HeldSync").cloned())
            .collect();
        assert_eq!(held_rows.len(), 5);
        assert!(held_rows.iter().any(|row| row["sent"] == true
            && row["modifiers"] == 8
            && row["press_ids"].as_array().unwrap().len() == 1));
        assert!(held_rows.iter().any(|row| row["sent"] == false
            && row["modifiers"] == 8
            && row["press_ids"].as_array().unwrap().len() == 1));
        assert!(held_rows.iter().any(|row| row["observations"] == 5));
        assert!(held_rows.iter().any(|row| row["modifiers"] == 1));
        assert!(held_rows.iter().any(|row| row["keys"] == 0));
        for forbidden in ["458979", "key_code", "scan_code", &peer.to_hex()] {
            assert!(!text.contains(forbidden));
        }
        recorder.shutdown();
    }

    #[test]
    fn queued_capture_remote_and_injection_are_anonymized_before_export() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = Recorder::start(dir.path().into(), DeviceId([1; 32]), Some(GroupId([2; 16])), true).unwrap();
        recorder.wait_recovery();
        let trace = InputTrace::new(true);
        let origin = DeviceId([4; 32]);
        let code = KeyCode::keyboard(0x17);
        trace.captured(&CapturedEvent {
            event: InputEvent::Key { code, pressed: true },
            physical: true,
            source: SourceKind::Keyboard,
            time_us: 10,
            cursor: None,
        });
        trace.remote(
            DeviceId([3; 32]),
            &Message::Input(InputMsg::Forwarded {
                origin: super::super::tests::boot(origin),
                hops: 1,
                msg: Box::new(InputMsg::Key { code, pressed: true }),
            }),
        );
        trace.injected(&InjectEvent::Key { code, pressed: true }, Some(origin), true);
        trace.drain(&recorder);
        let bundle = recorder.save(Reason::Manual).unwrap();
        let mut zip = zip::ZipArchive::new(std::fs::File::open(bundle.path).unwrap()).unwrap();
        let mut text = String::new();
        zip.by_name("merged.ndjson").unwrap().read_to_string(&mut text).unwrap();
        for forbidden in ["458775", "key_code", "usage", "scan_code", &origin.to_hex()] {
            assert!(!text.contains(forbidden), "raw input identifier in exported trace");
        }
        let rows = text
            .lines()
            .map(|row| serde_json::from_str::<serde_json::Value>(row).unwrap())
            .collect::<Vec<_>>();
        let input = rows
            .iter()
            .filter_map(|row| row["event"].get("Input"))
            .collect::<Vec<_>>();
        assert_eq!(input.len(), 3);
        assert_eq!(
            input
                .iter()
                .filter(|event| event["peer"] == recorder.anonymizer().device(origin))
                .count(),
            2
        );
        assert!(input.iter().any(|event| event["success"] == true));
        recorder.shutdown();
    }

    #[test]
    fn queue_is_bounded_and_disabled_drain_discards_buffered_input() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = Recorder::start(dir.path().into(), DeviceId([1; 32]), None, false).unwrap();
        let trace = InputTrace::new(true);
        for _ in 0..8193 {
            trace.injected(
                &InjectEvent::MoveTo {
                    pos: oink_foundation::Point::new(1., 2.),
                },
                None,
                true,
            );
        }
        assert_eq!(trace.queue.len(), 8192);
        assert_eq!(trace.dropped.load(Ordering::Relaxed), 1);
        trace.drain(&recorder);
        assert!(trace.queue.is_empty());
        assert_eq!(recorder.inner.dropped.load(Ordering::Relaxed), 1);
        assert!(!trace.enabled.load(Ordering::Relaxed));
        recorder.shutdown();
    }
}

#[cfg(all(test, debug_assertions))]
mod handoff_tests {
    use super::*;
    use oink_foundation::{
        Point, Rect,
        input::{CaptureMode, InputEvent, SourceKind},
    };
    use oink_routing::control::{ControlMsg, FocusScope};
    use oink_routing::{
        Output, ScreenId, ScreenPos,
        router::{DecisionKind, DecisionReason, RoutingDiagnostics},
    };
    use std::sync::atomic::AtomicI64;
    #[test]
    fn entry_keeps_the_compiled_local_offset_and_filtered_topology() {
        use oink_display::layout::{Layout, ScreenPlacement};
        use oink_foundation::{Size, display::DisplayInfo};
        let dir = tempfile::tempdir().unwrap();
        let me = DeviceId([1; 32]);
        let peer = DeviceId([3; 32]);
        let recorder = Recorder::start(dir.path().into(), me, Some(GroupId([2; 16])), true).unwrap();
        recorder.wait_recovery();
        let trace = InputTrace::for_recorder(&recorder);
        let screens = [me, peer]
            .into_iter()
            .enumerate()
            .map(|(i, device)| {
                ScreenPlacement::from_display(
                    device,
                    &DisplayInfo {
                        key: "PRIVATE SCREEN".into(),
                        name: "PRIVATE DISPLAY".into(),
                        bounds: Rect::new(237., 11., 800., 600.),
                        pixel_width: 800,
                        pixel_height: 600,
                        reference_extent: Some(Size::new(300. / 686., 200. / 686.)),
                        rotation_deg: 0,
                        is_primary: true,
                        is_builtin: false,
                        is_virtual: false,
                    },
                    Point::new(i as f64 * 300., 0.),
                )
            })
            .collect();
        let layout = oink_routing::TopologyLayout::from_layout(
            &Layout {
                screens,
                ..Layout::default()
            },
            &Default::default(),
        )
        .with_online([me].into_iter().collect());
        let native = oink_foundation::display::DisplayInfo {
            key: "PRIVATE SCREEN".into(),
            name: "PRIVATE DISPLAY".into(),
            bounds: Rect::new(237., 11., 749., 600.),
            pixel_width: 1498,
            pixel_height: 1200,
            reference_extent: None,
            rotation_deg: 0,
            is_primary: true,
            is_builtin: false,
            is_virtual: false,
        };
        let at = ScreenPos::new(ScreenId::new(me, "PRIVATE SCREEN"), Point::new(985., 12.));
        trace.topology(
            &layout,
            GroupId([2; 16]),
            Arc::new(oink_display::layout::Layout::default()),
            Default::default(),
            super::super::tests::wire_version(0),
            Some([me].into_iter().collect()),
        );
        trace.state(
            "entry_before_clamp",
            me,
            9,
            3,
            Some(&at),
            Some(&at),
            Some("PRIVATE SCREEN"),
            std::slice::from_ref(&native),
            &layout,
        );
        recorder.display_enumerated(std::slice::from_ref(&native), 3);
        trace.crossing(
            Arc::new(layout.clone()),
            10,
            &at,
            Point::new(30., 0.),
            false,
            false,
            &ScreenPos::new(ScreenId::new(peer, "PRIVATE SCREEN"), Point::new(240., 12.)),
            oink_routing::router::DepartureEvidence {
                revision: 3,
                bounds: Some(native.bounds),
                point: Some(&at),
            },
        );
        trace.drain(&recorder);
        trace.control(
            me,
            peer,
            false,
            &Message::Control(ControlMsg::FocusEnter {
                epoch: 9,
                display: "PRIVATE SCREEN".into(),
                pos: Point::new(238., 12.),
                scope: None,
                pointer_source: Some(peer),
            }),
        );
        trace.drain(&recorder);
        let bundle = recorder.save(Reason::Manual).unwrap();
        let rows = super::super::tests::timeline(&bundle.path, "merged.ndjson");
        let topology = super::super::layout::RouterCompilation {
            group: GroupId([2; 16]),
            version: super::super::tests::wire_version(0),
            sequence: 1,
            stage: super::super::layout::CompilationStage::Router,
            topology: layout.clone(),
            source_layout: Arc::new(oink_display::layout::Layout::default()),
            input_online: Some([me].into_iter().collect()),
        }
        .project(&recorder.anonymizer())
        .unwrap();
        assert!(topology.screens.iter().all(|s| s.local.x == 237. && s.local.y == 11.));
        assert_eq!(topology.screens.iter().filter(|s| s.active).count(), 1);
        let state = rows.iter().find_map(|r| r["event"].get("RouterState")).unwrap();
        assert_eq!(state["display_revision"], 3);
        for position in ["pointer", "exit", "reference"] {
            assert_eq!(state[position]["native_bounds"]["w"], 749.0);
            assert_eq!(state[position]["compiled_bounds"]["w"], 800.0);
        }
        let displays = rows
            .iter()
            .find_map(|r| r["event"].get("Displays").filter(|d| d["phase"] == "native_enumerated"))
            .unwrap();
        assert_eq!(displays["revision"], 3);
        assert_eq!(displays["screens"][0]["pixel_width"], 1498);
        assert_eq!(displays["screens"][0]["pixels_per_unit"]["x"], 2.0);
        let crossing = rows.iter().find_map(|r| r["event"].get("Crossing")).unwrap();
        let clamp = &crossing["departure_clamp"];
        assert!((clamp["requested"]["x"].as_f64().unwrap() - 1037.0).abs() < 0.00001);
        assert_eq!(clamp["applied"]["x"], 985.0);
        assert_eq!(clamp["bounds"]["w"], 749.0);
        assert_eq!(clamp["display_revision"], 3);
        assert!(!serde_json::to_string(&rows).unwrap().contains("PRIVATE"));
        recorder.shutdown();
    }
    struct Source(AtomicI64);
    impl clock::Source for Source {
        fn read(&self) -> (u64, i64) {
            (
                self.0.load(Ordering::Relaxed) as u64,
                utc_us() - 1000 + self.0.load(Ordering::Relaxed),
            )
        }
    }
    #[test]
    fn handoff_records_are_private_capture_stamped_and_repeated_idle_state_does_not_wake() {
        let dir = tempfile::tempdir().unwrap();
        let source = Arc::new(Source(AtomicI64::new(0)));
        let recorder = Recorder::start_with_clock(
            dir.path().into(),
            DeviceId([1; 32]),
            Some(GroupId([2; 16])),
            true,
            clock::Clock::new(source.clone()),
        )
        .unwrap();
        recorder.wait_recovery();
        let trace = InputTrace::for_recorder(&recorder);
        let wakes = Arc::new(AtomicU64::new(0));
        let observed = wakes.clone();
        trace.set_wake(Arc::new(move || {
            observed.fetch_add(1, Ordering::Relaxed);
        }));
        let me = DeviceId([1; 32]);
        let peer = DeviceId([3; 32]);
        let scope = FocusScope {
            members: vec![(me, [4; 16]), (peer, [5; 16])],
        };
        let state = Message::Control(ControlMsg::FocusState {
            lock_state: None,
            epoch: 7,
            holder: peer,
            session: Some([5; 16]),
            scope: Some(scope.clone()),
        });
        trace.control(me, peer, false, &state);
        trace.drain(&recorder);
        let old_wakes = wakes.load(Ordering::Relaxed);
        for _ in 0..100 {
            trace.control(me, peer, false, &state);
            trace.remote(
                peer,
                &Message::Input(InputMsg::HeldSync {
                    keys: vec![],
                    buttons: vec![],
                }),
            );
        }
        assert!(trace.queue.is_empty());
        assert!(trace.evidence.is_empty());
        assert_eq!(old_wakes, wakes.load(Ordering::Relaxed));
        source.0.store(10, Ordering::Relaxed);
        trace.control(
            me,
            peer,
            true,
            &Message::Control(ControlMsg::FocusEnter {
                epoch: 8,
                display: "PRIVATE SCREEN".into(),
                pos: Point::new(1., 2.),
                scope: Some(scope),
                pointer_source: Some(me),
            }),
        );
        trace.log(123, "focus handoff", 8);
        trace.pair(peer, true);
        trace.decision(
            DecisionKind::Handoff,
            DecisionReason::EdgeCrossed,
            8,
            me,
            Some(&ScreenPos::new(
                ScreenId::new(peer, "PRIVATE SCREEN"),
                Point::new(1., 2.),
            )),
        );
        trace.mode_requested(
            Some(CaptureMode::Remote {
                park: Point::new(3., 4.),
            }),
            None,
            1,
        );
        trace.mode(
            Some(CaptureMode::Remote {
                park: Point::new(3., 4.),
            }),
            None,
            1,
            true,
            None,
        );
        trace.mode(
            Some(CaptureMode::Remote {
                park: Point::new(3., 4.),
            }),
            None,
            1,
            false,
            Some(true),
        );
        trace.output(
            &Output::CursorTrailResumeClipped {
                state: oink_routing::control::TrailContinuation {
                    shown_secs: 0.,
                    size_scale: 1.,
                    native_units_per_reference: 1.,
                    emit_carry: 0.,
                    heading: Point::new(0., 0.),
                    points: vec![],
                },
                clip: Rect::new(0., 0., 10., 10.),
            },
            8,
        );
        trace.flush(peer, 13, 1., 2.);
        source.0.store(20, Ordering::Relaxed);
        trace.captured(&CapturedEvent {
            event: InputEvent::PointerDelta { dx: 1., dy: 2. },
            physical: true,
            source: SourceKind::Mouse,
            time_us: 999,
            cursor: Some(Point::new(1., 2.)),
        });
        source.0.store(30, Ordering::Relaxed);
        trace.injected(
            &InjectEvent::MoveTo {
                pos: Point::new(1., 2.),
            },
            Some(peer),
            true,
        );
        source.0.store(100, Ordering::Relaxed);
        trace.drain(&recorder);
        let bundle = recorder.save(Reason::Manual).unwrap();
        let rows = super::super::tests::timeline(&bundle.path, "merged.ndjson");
        let handoffs: Vec<_> = rows.iter().filter(|r| r["event"].get("Handoff").is_some()).collect();
        assert_eq!(handoffs.len(), 9);
        assert!(
            handoffs
                .iter()
                .any(|r| r["event"]["Handoff"]["screen"] == recorder.anonymizer().screen(peer, "PRIVATE SCREEN"))
        );
        assert!(handoffs.iter().skip(1).all(|r| r["mono_us"] == 10));
        assert!(
            rows.iter()
                .any(|r| r["event"].get("Input").is_some() && r["mono_us"] == 20)
        );
        assert!(
            rows.iter()
                .any(|r| r["event"].get("Input").is_some() && r["mono_us"] == 30)
        );
        assert!(
            rows.iter()
                .any(|r| r["event"]["Log"]["module"] == "oink_routing::router" && r["mono_us"] == 10)
        );
        let json = serde_json::to_string(&rows).unwrap();
        for private in [
            "PRIVATE SCREEN",
            &peer.to_hex(),
            "key_code",
            "clipboard",
            "window_title",
        ] {
            assert!(!json.contains(private), "leak {private}");
        }
        recorder.set_enabled(false).unwrap();
        trace.drain(&recorder);
        let old_wakes = wakes.load(Ordering::Relaxed);
        trace.control(me, peer, true, &state);
        trace.pair(peer, false);
        trace.flush(peer, 14, 1., 2.);
        assert!(trace.queue.is_empty());
        assert_eq!(old_wakes, wakes.load(Ordering::Relaxed));
        recorder.shutdown();
    }
}

#[cfg(all(test, debug_assertions))]
mod contact_privacy_tests {
    use super::*;
    #[test]
    fn contact_recorder_keeps_each_capture_id_set_without_geometry() {
        use oink_foundation::contacts::{Contact, Sample, Surface};
        let dir = tempfile::tempdir().unwrap();
        let recorder = Recorder::start(dir.path().into(), DeviceId([1; 32]), None, true).unwrap();
        recorder.wait_recovery();
        let trace = InputTrace::for_recorder(&recorder);
        for (time_us, ids) in [(1000, &[17, u32::MAX][..]), (9000, &[u32::MAX][..]), (17000, &[][..])] {
            let mut sample = Sample {
                device: 1,
                time_us,
                surface: Surface {
                    width: 15600,
                    height: 9600,
                },
                len: ids.len() as u8,
                ..Default::default()
            };
            for (contact, id) in sample.contacts.iter_mut().zip(ids) {
                *contact = Contact {
                    id: *id,
                    x: 12345,
                    y: 8765,
                };
            }
            let event = CapturedEvent {
                physical: false,
                event: oink_input::InputEvent::TouchpadSample(sample),
                source: oink_input::SourceKind::Touchpad,
                time_us,
                cursor: None,
            };
            trace.finish_capture(&event, trace.begin_capture(), oink_input::Disposition::Swallow);
        }
        trace.drain(&recorder);
        let bundle = recorder.save(Reason::Manual).unwrap();
        let rows = super::super::tests::timeline(&bundle.path, "merged.ndjson");
        let contacts: Vec<_> = rows
            .iter()
            .filter_map(|r| {
                let log = r["event"].get("Log")?;
                (log["module"] == "touchpad_contacts").then_some(log)
            })
            .collect();
        assert_eq!(
            contacts.len(),
            3,
            "every contact frame, including full lift, needs numeric evidence"
        );
        let fields = |index: usize| -> std::collections::BTreeMap<String, f64> {
            serde_json::from_value::<Vec<(String, f64)>>(contacts[index]["fields"].clone())
                .unwrap()
                .into_iter()
                .collect()
        };
        assert_eq!(fields(0)["count"], 2.);
        assert_eq!(fields(0)["id0"], 17.);
        assert_eq!(fields(0)["id1"], f64::from(u32::MAX));
        assert_eq!(fields(1)["count"], 1.);
        assert_eq!(fields(1)["id0"], f64::from(u32::MAX));
        assert_eq!(fields(2)["count"], 0.);
        assert_eq!(fields(2)["time_us"], 17000.);
        let json = serde_json::to_string(&contacts).unwrap();
        assert!(
            !json.contains("12345") && !json.contains("8765"),
            "contact coordinates must stay absent"
        );
        recorder.shutdown();
    }
}
