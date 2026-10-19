//! One allow-listed layout projection for changes and per-device export environments.
//! Raw inputs stay in the configuration owner's immutable in-memory snapshots only.
use super::*;
use oink_display::layout::{EffectiveSettings, SizeSource};
use oink_display::layout::{Layout, OwnerLayoutPreferences};
use oink_foundation::{Point, Rect, Size};
use oink_protocol::v1::{self, dto as wire};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Version {
    pub counter: u64,
    pub writer_alias: String,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Scalar {
    Flag(bool),
    Integer(u64),
    Number(f64),
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OwnerScreen {
    pub screen_alias: String,
    pub gain: Option<f64>,
    pub enabled: Option<bool>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OwnerValue {
    pub scalars: BTreeMap<String, Scalar>,
    pub screens: Vec<OwnerScreen>,
    pub omitted: bool,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OwnerPreference {
    pub device_alias: String,
    pub option: u32,
    pub revision: u64,
    pub effective: OwnerValue,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalPause {
    pub paused: bool,
    pub reason: u32,
    pub revision: u64,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SavedScreen {
    pub device: String,
    pub screen: String,
    pub local: Rect,
    pub plane: Rect,
    pub size_source: SizeSource,
    pub gain: f64,
    pub enabled: bool,
    pub primary: bool,
    pub builtin: bool,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Display {
    pub screen: String,
    pub bounds: Rect,
    pub pixel_width: u32,
    pub pixel_height: u32,
    pub reference_extent: Option<Size>,
    pub rotation_deg: u16,
    pub scale: f64,
    pub primary: bool,
    pub builtin: bool,
    pub virtual_display: bool,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Device {
    pub device: String,
    pub saved_member: bool,
    pub applied_member: bool,
    pub online: bool,
    pub input_capable: Option<bool>,
    /// None means this referenced device has not supplied facts, rather than an empty desktop.
    pub facts_version: Option<u64>,
    pub displays: Vec<Display>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    pub effective: EffectiveSettings,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TopologyScreen {
    pub device: String,
    pub screen: String,
    pub local: Rect,
    pub plane: Rect,
    pub gain: f64,
    pub plane_gain: Option<f64>,
    pub reference_per_native_unit: Option<f64>,
    pub size_source: SizeSource,
    pub enabled: bool,
    pub topology_active: bool,
    pub input_online: bool,
    pub active: bool,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SourceScreen {
    pub device: String,
    pub screen: String,
    pub reference_per_native_unit: Option<f64>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SourceDevice {
    pub device: String,
    pub origin: Option<Point>,
    pub requires_target: bool,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Edge {
    pub device: String,
    pub screen: String,
    pub side: oink_display::layout::Side,
    pub at: f64,
    pub segments: Vec<Segment>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Segment {
    pub from: f64,
    pub to: f64,
    pub kind: SegmentKind,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum SegmentKind {
    Wall,
    Portal {
        device: String,
        screen: String,
        gap_distance: f64,
    },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompilationStage {
    Prepared,
    Router,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Compiled {
    pub current_group: bool,
    pub stage: CompilationStage,
    pub sequence: u64,
    pub version: Version,
    pub settings: EffectiveSettings,
    pub screens: Vec<TopologyScreen>,
    pub sources: Vec<SourceScreen>,
    pub source_devices: Vec<SourceDevice>,
    /// None is the unfiltered prepared layout; Some is the exact input-owner gate.
    pub input_online: Option<Vec<String>>,
    pub edges: Vec<Edge>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CompleteLayout {
    /// Configuration-owner publication sequence. Facts/availability need no saved edit.
    pub sequence: u64,
    pub version: Version,
    pub roster_digest: [u8; 32],
    pub saved_members: Vec<String>,
    pub applied_roster_digest: [u8; 32],
    pub applied_version: Version,
    pub source: u32,
    pub paused: bool,
    pub saved: Vec<SavedScreen>,
    pub settings: Settings,
    pub devices: Vec<Device>,
    pub compiled: Compiled,
    pub owner_preferences: Vec<OwnerPreference>,
    pub local_pause: LocalPause,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChangeKind {
    Initialized = 0,
    GroupChanged = 1,
    Arrangement = 2,
    Settings = 4,
    DisplayFacts = 5,
    Membership = 6,
    Availability = 7,
    CompiledTopology = 8,
    Applied = 9,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LayoutChange {
    /// One publication can change several parts, e.g. a peer state with geometry + settings.
    pub kinds: Vec<ChangeKind>,
    pub before: Option<CompleteLayout>,
    pub after: CompleteLayout,
}

/// Never serialized: keys, names and raw device ids cannot pass the projection below.
#[derive(Clone)]
pub(crate) struct Inputs {
    pub group: GroupId,
    pub saved: Arc<Layout>,
    pub version: wire::ConfigVersion,
    pub roster_digest: [u8; 32],
    pub saved_members: Vec<DeviceId>,
    pub applied_roster_digest: [u8; 32],
    pub applied_version: wire::ConfigVersion,
    pub source: wire::LayoutSource,
    pub paused: bool,
    pub owners: Arc<BTreeMap<DeviceId, OwnerLayoutPreferences>>,
    pub owner_preferences: Vec<(DeviceId, Vec<wire::DevicePreference>)>,
    pub local_pause: wire::LocalPauseState,
    pub devices: Vec<DeviceInputs>,
    pub compiled: RouterCompilation,
    pub sequence: u64,
}
#[derive(Clone)]
pub(crate) struct DeviceInputs {
    pub device: DeviceId,
    pub saved_member: bool,
    pub applied_member: bool,
    pub online: bool,
    pub facts: Option<Arc<wire::DeviceFacts>>,
}
#[derive(Clone)]
pub(crate) struct RouterCompilation {
    pub group: GroupId,
    pub version: wire::ConfigVersion,
    pub sequence: u64,
    pub stage: CompilationStage,
    pub topology: oink_routing::TopologyLayout,
    pub source_layout: Arc<Layout>,
    pub input_online: Option<std::collections::BTreeSet<DeviceId>>,
}
fn version(value: &wire::ConfigVersion, anon: &Anonymizer) -> v1::Result<Version> {
    let writer = value.writer()?;
    if writer.algorithm()?.0 != wire::ED25519 {
        return Err(v1::ErrorCode::UnsupportedValue.into());
    }
    let key: [u8; 32] = writer
        .public_key()?
        .0
        .try_into()
        .map_err(|_| v1::ErrorCode::UnsupportedValue)?;
    Ok(Version {
        counter: value.counter()?,
        writer_alias: anon.device(DeviceId(key)),
    })
}
fn rect(value: wire::Rect) -> v1::Result<Rect> {
    Ok(Rect::new(value.x()?, value.y()?, value.w()?, value.h()?))
}
fn size(value: wire::Size) -> v1::Result<Size> {
    Ok(Size::new(value.w()?, value.h()?))
}
impl DeviceInputs {
    fn project(&self, anon: &Anonymizer) -> v1::Result<Device> {
        let facts = self.facts.as_deref();
        let input_capable = facts
            .map(|f| Ok::<_, v1::Error>(f.capabilities()?.supported()?.0.iter().any(|c| c.0 == "oink.input/1")))
            .transpose()?;
        let displays = facts
            .map(|f| f.displays())
            .transpose()?
            .map(|d| d.0)
            .unwrap_or_default()
            .into_iter()
            .map(|d| {
                Ok(Display {
                    screen: anon.screen(self.device, &d.key()?.0),
                    bounds: rect(d.bounds()?)?,
                    pixel_width: d.pixel_width()?,
                    pixel_height: d.pixel_height()?,
                    reference_extent: d.reference_extent()?.map(size).transpose()?,
                    rotation_deg: d.rotation_deg()?,
                    scale: d.scale()?,
                    primary: d.is_primary()?,
                    builtin: d.is_builtin()?,
                    virtual_display: d.is_virtual()?,
                })
            })
            .collect::<v1::Result<Vec<_>>>()?;
        Ok(Device {
            device: anon.device(self.device),
            saved_member: self.saved_member,
            applied_member: self.applied_member,
            online: self.online,
            input_capable,
            facts_version: facts.map(|f| f.fact_sequence()).transpose()?,
            displays,
        })
    }
}
impl Inputs {
    pub fn project(&self, anon: &Anonymizer) -> v1::Result<CompleteLayout> {
        let layout = &self.saved;
        let mut owner_preferences = Vec::new();
        for (device, preferences) in &self.owner_preferences {
            for preference in preferences {
                owner_preferences.push(owner_preference(*device, preference, anon)?);
            }
        }
        owner_preferences.sort_by_key(|p| (p.device_alias.clone(), p.option));
        let mut compiled = self.compiled.project(anon)?;
        compiled.current_group = self.compiled.group == self.group;
        Ok(CompleteLayout {
            sequence: self.sequence,
            version: version(&self.version, anon)?,
            roster_digest: self.roster_digest,
            saved_members: self.saved_members.iter().map(|d| anon.device(*d)).collect(),
            applied_roster_digest: self.applied_roster_digest,
            applied_version: version(&self.applied_version, anon)?,
            source: self.source.number(),
            paused: self.paused,
            saved: layout
                .screens
                .iter()
                .map(|s| {
                    let effective = self.owners.get(&s.id.device).cloned().unwrap_or_default().screen(s);
                    SavedScreen {
                        device: anon.device(s.id.device),
                        screen: anon.screen(s.id.device, &s.id.display),
                        local: s.local,
                        plane: s.rect,
                        size_source: s.size_source,
                        gain: effective.gain,
                        enabled: effective.enabled,
                        primary: s.primary,
                        builtin: s.builtin,
                    }
                })
                .collect(),
            settings: Settings {
                effective: layout.settings.clone(),
            },
            devices: self
                .devices
                .iter()
                .map(|d| d.project(anon))
                .collect::<v1::Result<_>>()?,
            compiled,
            owner_preferences,
            local_pause: LocalPause {
                paused: self.local_pause.paused()?,
                reason: self.local_pause.reason()?.number(),
                revision: self.local_pause.revision()?,
            },
        })
    }
}
impl RouterCompilation {
    pub fn source_devices(&self) -> std::collections::BTreeSet<DeviceId> {
        self.source_layout.screens.iter().map(|s| s.id.device).collect()
    }
    pub fn project(&self, anon: &Anonymizer) -> v1::Result<Compiled> {
        let topology = self.topology.topology();
        Ok(Compiled {
            current_group: true,
            stage: self.stage,
            sequence: self.sequence,
            version: version(&self.version, anon)?,
            settings: topology.settings().clone(),
            input_online: self
                .input_online
                .as_ref()
                .map(|devices| devices.iter().map(|d| anon.device(*d)).collect()),
            source_devices: self
                .source_devices()
                .into_iter()
                .map(|device| SourceDevice {
                    device: anon.device(device),
                    origin: topology.source_origin(device),
                    requires_target: topology.source_requires_target(device),
                })
                .collect(),
            sources: self
                .source_layout
                .screens
                .iter()
                .map(|s| SourceScreen {
                    device: anon.device(s.id.device),
                    screen: anon.screen(s.id.device, &s.id.display),
                    reference_per_native_unit: topology.source_reference_per_native_unit(&s.id),
                })
                .collect(),
            edges: topology
                .edges()
                .into_iter()
                .map(|edge| Edge {
                    device: anon.device(edge.screen.device),
                    screen: anon.screen(edge.screen.device, &edge.screen.display),
                    side: edge.side,
                    at: edge.at,
                    segments: edge
                        .segments
                        .into_iter()
                        .map(|s| Segment {
                            from: s.from,
                            to: s.to,
                            kind: match s.kind {
                                oink_display::layout::SegmentKind::Wall => SegmentKind::Wall,
                                oink_display::layout::SegmentKind::Portal { to, gap_distance } => SegmentKind::Portal {
                                    device: anon.device(to.device),
                                    screen: anon.screen(to.device, &to.display),
                                    gap_distance,
                                },
                            },
                        })
                        .collect(),
                })
                .collect(),
            screens: topology
                .screens()
                .iter()
                .map(|s| {
                    let topology_active = topology.is_active(&s.id);
                    let input_online = self.topology.input_online(s.id.device);
                    TopologyScreen {
                        device: anon.device(s.id.device),
                        screen: anon.screen(s.id.device, &s.id.display),
                        local: s.local,
                        plane: s.rect,
                        gain: s.gain,
                        plane_gain: topology.plane_gain(&s.id),
                        reference_per_native_unit: topology.reference_per_native_unit(&s.id),
                        size_source: s.size_source,
                        enabled: s.enabled,
                        topology_active,
                        input_online,
                        active: topology_active && input_online,
                    }
                })
                .collect(),
        })
    }
}
impl CompleteLayout {
    pub fn changes_from(&self, before: &Self) -> Vec<ChangeKind> {
        let mut kinds = Vec::new();
        let mut mark = |changed, kind| {
            if changed {
                kinds.push(kind);
            }
        };
        mark(
            self.version != before.version || self.saved != before.saved || self.source != before.source,
            ChangeKind::Arrangement,
        );
        mark(
            self.settings != before.settings || self.owner_preferences != before.owner_preferences,
            ChangeKind::Settings,
        );
        mark(
            self.roster_digest != before.roster_digest
                || self.saved_members != before.saved_members
                || self.applied_roster_digest != before.applied_roster_digest
                || self
                    .devices
                    .iter()
                    .filter(|d| d.applied_member)
                    .map(|d| &d.device)
                    .ne(before.devices.iter().filter(|d| d.applied_member).map(|d| &d.device)),
            ChangeKind::Membership,
        );
        mark(self.applied_version != before.applied_version, ChangeKind::Applied);
        // Facts are also published for network telemetry. Do not turn their version-only
        // changes into layout events; the current environment still includes their versions.
        mark(
            self.devices
                .iter()
                .map(|d| (&d.device, &d.displays))
                .ne(before.devices.iter().map(|d| (&d.device, &d.displays))),
            ChangeKind::DisplayFacts,
        );
        mark(
            self.paused != before.paused
                || self.local_pause != before.local_pause
                || self
                    .devices
                    .iter()
                    .map(|d| (&d.device, d.online, d.input_capable))
                    .ne(before.devices.iter().map(|d| (&d.device, d.online, d.input_capable))),
            ChangeKind::Availability,
        );
        mark(
            self.compiled.current_group != before.compiled.current_group
                || self.compiled.stage != before.compiled.stage
                || self.compiled.version != before.compiled.version
                || self.compiled.settings != before.compiled.settings
                || self.compiled.screens != before.compiled.screens
                || self.compiled.sequence != before.compiled.sequence
                || self.compiled.sources != before.compiled.sources
                || self.compiled.source_devices != before.compiled.source_devices
                || self.compiled.input_online != before.compiled.input_online
                || self.compiled.edges != before.compiled.edges,
            ChangeKind::CompiledTopology,
        );
        kinds
    }
}
/// Weak ownership avoids ConfigModule → Recorder → collection Context → ConfigModule.
pub(crate) struct Sink(Weak<Inner>);
impl Sink {
    pub fn new(recorder: &Recorder) -> Self {
        Self(Arc::downgrade(&recorder.inner))
    }
    pub fn same(&self, other: &Self) -> bool {
        Weak::ptr_eq(&self.0, &other.0)
    }
    pub fn active(&self) -> bool {
        self.0
            .upgrade()
            .is_some_and(|inner| inner.enabled.load(Ordering::Acquire) && !inner.stopped.load(Ordering::Acquire))
    }
    pub fn explicitly_disabled(&self) -> bool {
        self.0.upgrade().is_none_or(|inner| {
            !inner.configured_enabled.load(Ordering::Acquire) || inner.stopped.load(Ordering::Acquire)
        })
    }
    pub fn accepts(&self, group: GroupId) -> bool {
        self.0.upgrade().is_some_and(|inner| *lock(&inner.group) == Some(group))
    }
    pub fn dropped(&self) {
        if let Some(inner) = self.0.upgrade() {
            inner.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
    pub fn record(&self, group: GroupId, change: LayoutChange) {
        if let Some(inner) = self.0.upgrade() {
            let recorder = Recorder { inner };
            // Hold the domain guard through enqueue: rekey must not install a
            // new queue/domain between this check and the layout record.
            let domain = lock(&recorder.inner.group);
            if *domain == Some(group) {
                recorder.record(Event::Topology(Box::new(change)));
            }
        }
    }
}

fn owner_preference(
    device: DeviceId,
    preference: &wire::DevicePreference,
    anon: &Anonymizer,
) -> v1::Result<OwnerPreference> {
    use v1::RawValue;
    use v1::schema::WireDto;
    let option = preference.option()?.number();
    let mut effective = OwnerValue {
        scalars: BTreeMap::new(),
        screens: Vec::new(),
        omitted: false,
    };
    match (option, preference.value()?) {
        (1 | 2, RawValue::Uint(mode)) if mode <= 1 => {
            effective.scalars.insert("mode".into(), Scalar::Integer(mode));
        }
        (8 | 18, RawValue::Uint(mode)) if mode <= if option == 8 { 1 } else { 2 } => {
            effective.scalars.insert("mode".into(), Scalar::Integer(mode));
        }
        (3 | 4 | 5 | 6 | 10 | 19, RawValue::Bool(flag)) => {
            effective.scalars.insert("enabled".into(), Scalar::Flag(flag));
        }
        (11, RawValue::Uint(duration)) => {
            effective
                .scalars
                .insert("duration_ms".into(), Scalar::Integer(duration));
        }
        (12, RawValue::F64(speed)) if speed.is_finite() => {
            effective.scalars.insert("speed".into(), Scalar::Number(speed));
        }
        (7, RawValue::Map(raw)) => {
            effective.omitted = raw.keys().any(|k| ![1, 2].contains(k));
            let value = wire::ScreenPreferencesValue::from_raw(raw)?;
            effective
                .scalars
                .insert("device_gain".into(), Scalar::Number(value.device_gain()?));
            for screen in value.screens()?.0 {
                effective.omitted |= screen.raw().keys().any(|k| ![1, 2, 3].contains(k));
                effective.screens.push(OwnerScreen {
                    screen_alias: anon.screen(device, &screen.display()?.0),
                    gain: Some(screen.gain()?),
                    enabled: Some(screen.enabled()?),
                });
            }
            effective.screens.sort_by(|a, b| a.screen_alias.cmp(&b.screen_alias));
        }
        (14, RawValue::Map(raw)) => {
            effective.omitted = raw.keys().any(|k| !(1..=6).contains(k));
            let value = wire::SourceInputPolicyValue::from_raw(raw)?;
            effective.scalars.insert(
                "button_handoff".into(),
                Scalar::Integer(u64::from(value.button_handoff()?.number())),
            );
            for (name, flag) in [
                ("forward_gestures", value.forward_gestures()?),
                (
                    "return_pointer_on_display_sleep",
                    value.return_pointer_on_display_sleep()?,
                ),
                ("wake_displays_on_input", value.wake_displays_on_input()?),
                ("dnd_enabled", value.dnd_enabled()?),
            ] {
                effective.scalars.insert(name.into(), Scalar::Flag(flag));
            }
            if let Some(reference) = value.reference_display()? {
                effective.screens.push(OwnerScreen {
                    screen_alias: anon.screen(device, &reference.0),
                    gain: None,
                    enabled: None,
                });
            }
        }
        (15, RawValue::Map(raw)) => {
            effective.omitted = raw.keys().any(|k| ![1, 2].contains(k));
            let value = wire::ForwardingControlsValue::from_raw(raw)?;
            effective.scalars.insert(
                "volume_percent".into(),
                Scalar::Integer(u64::from(value.volume_percent()?)),
            );
            effective.scalars.insert("muted".into(), Scalar::Flag(value.muted()?));
        }
        // Names, physical key mappings, output/claim identities and future
        // preference trees remain omitted. Their option and revision survive.
        _ => effective.omitted = true,
    }
    Ok(OwnerPreference {
        device_alias: anon.device(device),
        option,
        revision: preference.revision()?,
        effective,
    })
}
