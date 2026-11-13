//! The global audio configuration (DeviceFacts key 33), shared by PCs and
//! phones.
//!
//! Two group-wide registers name the only speaker exit and the only
//! microphone sender; every online device also has two non-exclusive
//! switches, speaker "forward" and microphone "receive". A device's
//! three-state mode is derived from them: speaker = exit / forward / local,
//! microphone = send / receive / local. Keeping the exclusive roles in one
//! register makes "at most one exit and one sender" a property of the data:
//! per-device "I am the exit" claims that rewrite themselves on conflict can
//! lose every exit under concurrency.
//!
//! Every register and switch carries a version `(stamp, author)`, where the
//! stamp is a hybrid logical clock taken when the choice was made. The device
//! where the user made a choice authors it and broadcasts the whole
//! configuration; every device adopts the newest version it hears, so all of
//! them converge in any arrival order. A choice made after seeing another
//! one always has a larger stamp; only concurrent choices (a partition, an
//! offline phone, a sleeping PC) are ordered by wall clock.
//!
//! A device that starts (process start, group switch or a whole-group merge)
//! is "unjoined": it publishes no configuration and executes no exclusive
//! role until it merges with what an online member publishes (§2.4 of the
//! design). Old persisted claims yield to an online holder there; choices the
//! user made after the start keep their click stamp and compete by time.
//! Offline devices drop out of the overview: a register naming one is
//! executed as "none".
use oink_foundation::DeviceId;
use oink_protocol::v2::{
    RawValue,
    dto::{self, ControlFailure, ED25519},
    schema::{List, WireDto, WireValue},
};
use std::collections::{BTreeMap, BTreeSet};

/// Low bits of a stamp hold the logical counter; the rest is Unix ms (UTC).
pub const LOGICAL_BITS: u32 = 16;
/// A received stamp further ahead of the local wall clock is logged once,
/// with its author, so a device with a wrong clock can be found. It is never
/// truncated or corrected.
pub const CLOCK_AHEAD_WARNING_MS: u64 = 60_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
pub struct Version {
    pub stamp: u64,
    pub author: DeviceId,
}
impl Version {
    /// The author's wall clock when the choice was made.
    pub fn millis(self) -> u64 {
        self.stamp >> LOGICAL_BITS
    }
    pub fn from_wire(version: &dto::AudioRouteVersion) -> Result<Self, ControlFailure> {
        let invalid = |_| ControlFailure::InvalidValue;
        Ok(Self {
            stamp: version.stamp().map_err(invalid)?,
            author: read_device(&version.author().map_err(invalid)?)?,
        })
    }
    pub fn to_wire(self) -> dto::AudioRouteVersion {
        dto::AudioRouteVersion::from_raw(BTreeMap::from([
            (1, RawValue::Uint(self.stamp)),
            (2, device(self.author)),
        ]))
        .expect("validated nonzero route version")
    }
}
fn device(id: DeviceId) -> RawValue {
    RawValue::Map(BTreeMap::from([
        (1, RawValue::Text(ED25519.into())),
        (2, RawValue::Bytes(id.0.to_vec())),
    ]))
}
fn read_device(value: &dto::DeviceId) -> Result<DeviceId, ControlFailure> {
    value.require_supported().map_err(|_| ControlFailure::Unsupported)?;
    Ok(DeviceId(
        value
            .public_key()
            .map_err(|_| ControlFailure::InvalidValue)?
            .0
            .try_into()
            .map_err(|_| ControlFailure::InvalidValue)?,
    ))
}

/// Hybrid logical clock. Authoring takes `max(wall clock, last + 1)`;
/// receiving only raises `last`. A choice made after seeing another one is
/// therefore always newer, whatever the two devices' clocks say.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Hlc {
    last: u64,
}
impl Hlc {
    pub fn new(last: u64) -> Self {
        Self { last }
    }
    /// The largest stamp authored or received (PCs persist it).
    pub fn last(self) -> u64 {
        self.last
    }
    /// A new stamp for a choice made now. Never zero, strictly increasing,
    /// also when the wall clock steps back; a counter overflow carries into
    /// the millisecond part.
    pub fn stamp(&mut self, wall_ms: u64) -> u64 {
        let wall = wall_ms.min(u64::MAX >> LOGICAL_BITS) << LOGICAL_BITS;
        self.last = if wall > self.last {
            wall
        } else {
            // Unreachable before the year 10889; never wraps to an old stamp.
            self.last.saturating_add(1)
        };
        self.last
    }
    /// Remember a received stamp. Returns how far (ms) it is ahead of this
    /// device's wall clock when that exceeds [`CLOCK_AHEAD_WARNING_MS`] and
    /// the stamp is news here, for one diagnostic log line.
    pub fn observe(&mut self, stamp: u64, wall_ms: u64) -> Option<u64> {
        if stamp <= self.last {
            return None;
        }
        self.last = stamp;
        let millis = stamp >> LOGICAL_BITS;
        (millis > wall_ms.saturating_add(CLOCK_AHEAD_WARNING_MS)).then(|| millis - wall_ms)
    }
}

/// One register version. `target: None` is an explicit "no exit"/"no sender".
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Route {
    pub version: Version,
    pub target: Option<DeviceId>,
}
impl Route {
    fn order(&self) -> (Version, Option<DeviceId>) {
        (self.version, self.target)
    }
    pub fn from_wire(route: &dto::AudioRoute) -> Result<Self, ControlFailure> {
        let invalid = |_| ControlFailure::InvalidValue;
        Ok(Self {
            version: Version::from_wire(&route.version().map_err(invalid)?)?,
            target: route.target().map_err(invalid)?.as_ref().map(read_device).transpose()?,
        })
    }
    pub fn to_wire(self) -> dto::AudioRoute {
        let mut raw = BTreeMap::from([(1, self.version.to_wire().write())]);
        if let Some(target) = self.target {
            raw.insert(2, device(target));
        }
        dto::AudioRoute::from_raw(raw).expect("valid audio route")
    }
}

/// One non-exclusive switch: speaker "forward" or microphone "receive".
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Flag {
    pub on: bool,
    pub version: Version,
}
impl Flag {
    fn order(&self) -> (Version, bool) {
        (self.version, self.on)
    }
    fn newer(self, other: Option<Flag>) -> bool {
        other.is_none_or(|other| self.order() > other.order())
    }
    pub fn from_wire(flag: &dto::AudioFlag) -> Result<Self, ControlFailure> {
        let invalid = |_| ControlFailure::InvalidValue;
        Ok(Self {
            on: flag.on().map_err(invalid)?,
            version: Version::from_wire(&flag.version().map_err(invalid)?)?,
        })
    }
    pub fn to_wire(self) -> dto::AudioFlag {
        dto::AudioFlag::from_raw(BTreeMap::from([
            (1, self.on.write()),
            (2, self.version.to_wire().write()),
        ]))
        .expect("valid audio flag")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Speaker,
    Microphone,
}
pub const ROLES: [Role; 2] = [Role::Speaker, Role::Microphone];
impl Role {
    pub fn index(self) -> usize {
        match self {
            Self::Speaker => 0,
            Self::Microphone => 1,
        }
    }
}

/// A device's three-state mode for one role. `Holder` is the speaker exit or
/// the microphone sender; `Shared` is speaker "forward" or microphone
/// "receive"; `Local` is neither.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Holder,
    Shared,
    Local,
}

/// Phones have neither forward nor receive and fall back to local; a PC
/// that loses a role falls back to forward / receive (user 2026-11-11 Q1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceKind {
    Desktop,
    Mobile,
}

/// The switches of one device. An unknown switch reads as off.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DeviceModes {
    pub forward: Option<Flag>,
    pub receive: Option<Flag>,
}
impl DeviceModes {
    pub fn get(&self, role: Role) -> Option<Flag> {
        match role {
            Role::Speaker => self.forward,
            Role::Microphone => self.receive,
        }
    }
    fn slot(&mut self, role: Role) -> &mut Option<Flag> {
        match role {
            Role::Speaker => &mut self.forward,
            Role::Microphone => &mut self.receive,
        }
    }
    pub fn on(&self, role: Role) -> bool {
        self.get(role).is_some_and(|flag| flag.on)
    }
    /// Adopt each newer switch; returns whether anything changed.
    fn observe(&mut self, other: &DeviceModes) -> bool {
        let mut changed = false;
        for role in ROLES {
            if let Some(received) = other.get(role)
                && received.newer(self.get(role))
            {
                *self.slot(role) = Some(received);
                changed = true;
            }
        }
        changed
    }
}

/// The whole configuration as published in DeviceFacts key 33.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AudioConfig {
    pub speaker: Option<Route>,
    pub microphone: Option<Route>,
    pub devices: BTreeMap<DeviceId, DeviceModes>,
}
impl AudioConfig {
    pub fn get(&self, role: Role) -> Option<Route> {
        match role {
            Role::Speaker => self.speaker,
            Role::Microphone => self.microphone,
        }
    }
    fn slot(&mut self, role: Role) -> &mut Option<Route> {
        match role {
            Role::Speaker => &mut self.speaker,
            Role::Microphone => &mut self.microphone,
        }
    }
    pub fn target(&self, role: Role) -> Option<DeviceId> {
        self.get(role).and_then(|route| route.target)
    }
    /// Adopt every register and switch that is newer than ours. Older or
    /// equal versions change nothing, so the result never depends on the
    /// arrival order. Equal versions converge by target / value.
    pub fn observe(&mut self, other: &AudioConfig) -> bool {
        let mut changed = false;
        for role in ROLES {
            let Some(received) = other.get(role) else { continue };
            let slot = self.slot(role);
            if slot.is_none_or(|current| received.order() > current.order()) {
                *slot = Some(received);
                changed = true;
            }
        }
        for (device, modes) in &other.devices {
            changed |= self.devices.entry(*device).or_default().observe(modes);
        }
        changed
    }
    fn stamps(&self) -> impl Iterator<Item = Version> + '_ {
        ROLES
            .into_iter()
            .filter_map(|role| self.get(role).map(|route| route.version))
            .chain(self.devices.values().flat_map(|modes| {
                ROLES
                    .into_iter()
                    .filter_map(move |role| modes.get(role).map(|f| f.version))
            }))
    }
    pub fn from_wire(config: &dto::AudioConfig) -> Result<Self, ControlFailure> {
        let invalid = |_| ControlFailure::InvalidValue;
        let mut devices = BTreeMap::new();
        for entry in config.devices().map_err(invalid)?.0 {
            let modes = DeviceModes {
                forward: entry
                    .forward()
                    .map_err(invalid)?
                    .as_ref()
                    .map(Flag::from_wire)
                    .transpose()?,
                receive: entry
                    .receive()
                    .map_err(invalid)?
                    .as_ref()
                    .map(Flag::from_wire)
                    .transpose()?,
            };
            // A device listed twice converges like two publications.
            devices
                .entry(read_device(&entry.device().map_err(invalid)?)?)
                .or_insert_with(DeviceModes::default)
                .observe(&modes);
        }
        Ok(Self {
            speaker: config
                .speaker_exit()
                .map_err(invalid)?
                .as_ref()
                .map(Route::from_wire)
                .transpose()?,
            microphone: config
                .microphone_source()
                .map_err(invalid)?
                .as_ref()
                .map(Route::from_wire)
                .transpose()?,
            devices,
        })
    }
    pub fn to_wire(&self) -> dto::AudioConfig {
        let mut raw = BTreeMap::new();
        if let Some(route) = self.speaker {
            raw.insert(1, route.to_wire().write());
        }
        if let Some(route) = self.microphone {
            raw.insert(2, route.to_wire().write());
        }
        let devices = self
            .devices
            .iter()
            .map(|(id, modes)| {
                let mut entry = BTreeMap::from([(1, device(*id))]);
                if let Some(flag) = modes.forward {
                    entry.insert(2, flag.to_wire().write());
                }
                if let Some(flag) = modes.receive {
                    entry.insert(3, flag.to_wire().write());
                }
                dto::AudioDeviceModes::from_raw(entry).expect("valid device modes")
            })
            .collect::<Vec<_>>();
        raw.insert(3, List::<dto::AudioDeviceModes, 64>(devices).write());
        dto::AudioConfig::from_raw(raw).expect("valid audio configuration")
    }
}

/// How this device chose the exclusive role it claims before merging.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Claim {
    /// Persisted from before this start (PC only). Yields to an online holder.
    Carried,
    /// Chosen by the user after this start, with the click's version.
    Fresh(Version),
}

/// This device's own configuration. A PC persists it (device-level, not per
/// group); a phone starts every process all local.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OwnConfig {
    pub speaker_exit: bool,
    pub microphone_send: bool,
    pub forward: Flag,
    pub receive: Flag,
}
impl OwnConfig {
    /// All local: no role, no forward, no receive.
    pub fn local(me: DeviceId) -> Self {
        let off = Flag {
            on: false,
            version: Version { stamp: 1, author: me },
        };
        Self {
            speaker_exit: false,
            microphone_send: false,
            forward: off,
            receive: off,
        }
    }
    pub fn holds(&self, role: Role) -> bool {
        match role {
            Role::Speaker => self.speaker_exit,
            Role::Microphone => self.microphone_send,
        }
    }
    pub fn flag(&self, role: Role) -> Flag {
        match role {
            Role::Speaker => self.forward,
            Role::Microphone => self.receive,
        }
    }
    pub fn mode(&self, role: Role) -> Mode {
        if self.holds(role) {
            Mode::Holder
        } else if self.flag(role).on {
            Mode::Shared
        } else {
            Mode::Local
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PendingFlag {
    flag: Flag,
    fresh: bool,
}
/// The own configuration of an unjoined device, with where each part came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Pending {
    claims: [Option<Claim>; 2],
    flags: [PendingFlag; 2],
}
impl Pending {
    fn carried(own: OwnConfig) -> Self {
        let claim = |held: bool| held.then_some(Claim::Carried);
        let flag = |flag| PendingFlag { flag, fresh: false };
        Self {
            claims: [claim(own.speaker_exit), claim(own.microphone_send)],
            flags: [flag(own.forward), flag(own.receive)],
        }
    }
    fn own(&self) -> OwnConfig {
        OwnConfig {
            speaker_exit: self.claims[0].is_some(),
            microphone_send: self.claims[1].is_some(),
            forward: self.flags[0].flag,
            receive: self.flags[1].flag,
        }
    }
}

/// A linked online member (a ready link and its current Boot's facts) and the
/// configuration it publishes; `None` while that member is still unjoined.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Peer {
    pub device: DeviceId,
    pub config: Option<AudioConfig>,
}

#[derive(Clone, Copy, Debug)]
pub struct Inputs<'a> {
    pub wall_ms: u64,
    /// Linked online members, without this device.
    pub linked: &'a [Peer],
    /// PC: the virtual microphone is installed, so "receive" can work.
    pub receive_ready: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Reconciled {
    /// This pass merged an unjoined device.
    pub merged: bool,
    /// The configuration to publish changed.
    pub published: bool,
    /// This device's own configuration changed (a PC persists it).
    pub own: bool,
    /// Received stamps ahead of this wall clock: (author, ms ahead).
    pub clock_ahead: Vec<(DeviceId, u64)>,
}

/// One device's view of the global audio configuration.
#[derive(Clone, Debug)]
pub struct Engine {
    me: DeviceId,
    kind: DeviceKind,
    hlc: Hlc,
    /// The stamp of this start. A version at or after it is a choice made
    /// after the start (a new choice), never old configuration.
    started: u64,
    merged: bool,
    pending: Pending,
    /// Registers and the switches of online devices, once merged.
    config: AudioConfig,
    online: BTreeSet<DeviceId>,
    linked: BTreeSet<DeviceId>,
    /// The newest switches the linked members list, per device.
    reported: BTreeMap<DeviceId, DeviceModes>,
    holding: [bool; 2],
}

impl Engine {
    /// A device that has just started: unjoined, with its own configuration
    /// (a PC's persisted one, a phone's all-local one) as old configuration.
    pub fn new(me: DeviceId, kind: DeviceKind, own: OwnConfig, hlc_last: u64, wall_ms: u64) -> Self {
        let mut hlc = Hlc::new(hlc_last);
        let started = hlc.stamp(wall_ms);
        Self {
            me,
            kind,
            hlc,
            started,
            merged: false,
            pending: Pending::carried(match kind {
                // A phone resets to all local at every start (§2.6).
                DeviceKind::Mobile => OwnConfig::local(me),
                DeviceKind::Desktop => own,
            }),
            config: AudioConfig::default(),
            online: BTreeSet::from([me]),
            linked: BTreeSet::new(),
            reported: BTreeMap::new(),
            holding: [false; 2],
        }
    }
    /// A group switch (including a whole-group merge) is a new start: the
    /// current own configuration becomes old configuration and the device
    /// merges with the new group (user 2026-11-11 Q3 and B).
    pub fn restart(&mut self, wall_ms: u64) {
        let own = self.own();
        *self = Self::new(self.me, self.kind, own, self.hlc.last(), wall_ms);
    }
    pub fn me(&self) -> DeviceId {
        self.me
    }
    pub fn merged(&self) -> bool {
        self.merged
    }
    pub fn hlc_last(&self) -> u64 {
        self.hlc.last()
    }
    pub fn online(&self) -> &BTreeSet<DeviceId> {
        &self.online
    }
    /// The register as held (its target may be offline; see `resolve`).
    pub fn route(&self, role: Role) -> Option<Route> {
        self.merged.then(|| self.config.get(role)).flatten()
    }
    /// The executed holder: the register's target while it is online. An
    /// unjoined device executes no exclusive role.
    pub fn holder(&self, role: Role) -> Option<DeviceId> {
        self.route(role)?.target.filter(|target| self.online.contains(target))
    }
    /// What this device executes for one role. `available` holds the online
    /// devices that are awake and not paused right now.
    pub fn resolve(&self, role: Role, available: &BTreeSet<DeviceId>) -> Resolution {
        match (self.route(role), self.holder(role)) {
            (Some(route), Some(target)) => Resolution::Exit {
                version: route.version,
                target,
                available: available.contains(&target),
            },
            _ => Resolution::None,
        }
    }
    /// This device's own configuration: before merging the pending one,
    /// afterwards the overview's entry for this device.
    pub fn own(&self) -> OwnConfig {
        if !self.merged {
            return self.pending.own();
        }
        let modes = self.config.devices.get(&self.me).copied().unwrap_or_default();
        let fallback = OwnConfig::local(self.me);
        OwnConfig {
            speaker_exit: self.holder(Role::Speaker) == Some(self.me),
            microphone_send: self.holder(Role::Microphone) == Some(self.me),
            forward: modes.forward.unwrap_or(fallback.forward),
            receive: modes.receive.unwrap_or(fallback.receive),
        }
    }
    /// A device's mode as the overview derives it; `None` for a device that is
    /// not online here, or for another device while this one is unjoined.
    pub fn mode(&self, device: DeviceId, role: Role) -> Option<Mode> {
        if device == self.me {
            return Some(self.own().mode(role));
        }
        if !self.merged || !self.online.contains(&device) {
            return None;
        }
        Some(if self.holder(role) == Some(device) {
            Mode::Holder
        } else if self.config.devices.get(&device).is_some_and(|modes| modes.on(role)) {
            Mode::Shared
        } else {
            Mode::Local
        })
    }
    /// A device's own forward / receive switch as the overview holds it;
    /// `None` when it is unknown here (offline, never published, or another
    /// device while this one is unjoined).
    pub fn switch(&self, device: DeviceId, role: Role) -> Option<bool> {
        if device == self.me {
            return Some(self.own().flag(role).on);
        }
        if !self.merged || !self.online.contains(&device) {
            return None;
        }
        self.config.devices.get(&device)?.get(role).map(|flag| flag.on)
    }
    /// The overview this device shows: every online device's modes.
    pub fn overview(&self) -> Overview {
        let devices = self
            .online
            .iter()
            .filter(|device| self.merged || **device == self.me)
            .filter_map(|device| {
                Some(DeviceOverview {
                    device: *device,
                    speaker: self.mode(*device, Role::Speaker)?,
                    microphone: self.mode(*device, Role::Microphone)?,
                    forward: self.switch(*device, Role::Speaker),
                    receive: self.switch(*device, Role::Microphone),
                })
            })
            .collect();
        Overview {
            joined: self.merged,
            speaker: self.route(Role::Speaker),
            microphone: self.route(Role::Microphone),
            devices,
        }
    }
    /// What to publish in DeviceFacts key 33; `None` while unjoined, which
    /// tells the others this device has not merged yet. It lists this device,
    /// the devices it links to, and a switch it authored for a device further
    /// away until a linked member relays it (never a device nobody vouches for).
    pub fn publication(&self) -> Option<AudioConfig> {
        if !self.merged {
            return None;
        }
        let devices = self
            .config
            .devices
            .iter()
            .filter(|(device, modes)| {
                **device == self.me
                    || self.linked.contains(*device)
                    || ROLES.into_iter().any(|role| {
                        modes
                            .get(role)
                            .is_some_and(|flag| flag.newer(self.reported.get(*device).and_then(|r| r.get(role))))
                    })
            })
            .map(|(device, modes)| (*device, *modes))
            .collect();
        Some(AudioConfig {
            speaker: self.config.speaker,
            microphone: self.config.microphone,
            devices,
        })
    }

    /// One pass over what the linked members publish: merge if this device
    /// is unjoined and can, then adopt newer versions and apply this device's
    /// own reactions (a PC that becomes a holder turns its fallback switch on;
    /// a phone withdraws a role somebody else gave it).
    pub fn reconcile(&mut self, inputs: Inputs<'_>) -> Reconciled {
        let before = (self.publication(), self.own());
        let mut result = Reconciled::default();
        for peer in inputs.linked {
            let Some(config) = &peer.config else { continue };
            for version in config.stamps() {
                if let Some(ahead) = self.hlc.observe(version.stamp, inputs.wall_ms) {
                    result.clock_ahead.push((version.author, ahead));
                }
            }
        }
        self.linked = inputs.linked.iter().map(|peer| peer.device).collect();
        self.online = self
            .linked
            .iter()
            .copied()
            .chain([self.me])
            .chain(
                inputs
                    .linked
                    .iter()
                    .filter_map(|peer| peer.config.as_ref())
                    .flat_map(|config| config.devices.keys().copied()),
            )
            .collect();
        self.reported.clear();
        for config in inputs.linked.iter().filter_map(|peer| peer.config.as_ref()) {
            for (device, modes) in &config.devices {
                self.reported.entry(*device).or_default().observe(modes);
            }
        }
        if !self.merged {
            let received: Vec<_> = inputs.linked.iter().filter_map(|peer| peer.config.as_ref()).collect();
            if !received.is_empty() {
                let mut group = AudioConfig::default();
                for config in received {
                    group.observe(config);
                }
                self.merge(group, inputs);
            } else if self.founds(inputs.linked) {
                self.merge(AudioConfig::default(), inputs);
            } else {
                return result;
            }
            result.merged = true;
        }
        for config in inputs.linked.iter().filter_map(|peer| peer.config.as_ref()) {
            self.config.observe(config);
        }
        let online = &self.online;
        self.config.devices.retain(|device, _| online.contains(device));
        self.react(inputs);
        let after = (self.publication(), self.own());
        result.published = before.0 != after.0;
        result.own = before.1 != after.1;
        result
    }

    /// The founder rule: when no linked online member has merged either, the
    /// device with the smallest ID merges with an empty configuration. With
    /// no linked member at all nobody merges yet: alone, no role has any
    /// effect, and merging early would let a just-started device's old claim
    /// beat the group's holder once the links come up.
    fn founds(&self, linked: &[Peer]) -> bool {
        !linked.is_empty() && linked.iter().all(|peer| peer.config.is_none() && peer.device > self.me)
    }

    /// §2.4: merge this unjoined device's own configuration with `group`. The
    /// conflict check only prevents a second exit / sender: an exclusive claim
    /// conflicts only with an online holder.
    fn merge(&mut self, group: AudioConfig, inputs: Inputs<'_>) {
        let now = self.hlc.stamp(inputs.wall_ms);
        let me = self.me;
        let mine = |stamp| Version { stamp, author: me };
        self.config = group;
        // Switches first: "add myself". Old switches are re-stamped so this
        // device's configuration beats any stale entry somebody still holds,
        // unless that entry is a choice made after this start (a setter's).
        let received = self.config.devices.get(&me).copied().unwrap_or_default();
        let mut own = DeviceModes::default();
        for role in ROLES {
            let pending = self.pending.flags[role.index()];
            let theirs = received.get(role);
            let candidate = if pending.fresh {
                pending.flag
            } else if theirs.is_some_and(|flag| flag.version.stamp >= self.started) {
                theirs.expect("checked")
            } else {
                Flag {
                    on: pending.flag.on,
                    version: mine(now),
                }
            };
            *own.slot(role) = Some(match theirs {
                Some(theirs) if !candidate.newer(Some(theirs)) => theirs,
                _ => candidate,
            });
        }
        self.config.devices.insert(me, own);
        let mut fallbacks = Vec::new();
        for role in ROLES {
            let register = self.config.get(role);
            let target = register.and_then(|route| route.target);
            let holder = target.filter(|target| self.online.contains(target));
            match self.pending.claims[role.index()] {
                Some(Claim::Carried) => match holder {
                    Some(holder) if holder == me => {}
                    // Old configuration yields to an online holder (user A).
                    Some(_) => fallbacks.push(role),
                    // Vacant: the old claim takes the role, but as old
                    // configuration it stays weaker than any real choice
                    // (see `weak`).
                    None => {
                        *self.config.slot(role) = Some(Route {
                            version: weak(register, me),
                            target: Some(me),
                        })
                    }
                },
                Some(Claim::Fresh(version)) => match holder {
                    // A phone holds a role only by its own version (§2.7).
                    Some(holder)
                        if holder == me
                            && self.kind == DeviceKind::Mobile
                            && register.is_some_and(|route| route.version.author != me) =>
                    {
                        *self.config.slot(role) = Some(Route {
                            version: mine(now),
                            target: Some(me),
                        });
                    }
                    Some(holder) if holder == me => {}
                    // Choices compete by time (user Q2).
                    Some(_) if register.is_some_and(|route| route.version >= version) => fallbacks.push(role),
                    Some(_) => {
                        *self.config.slot(role) = Some(Route {
                            version,
                            target: Some(me),
                        })
                    }
                    // No conflict (user 2026-11-11): the conflict check only
                    // prevents a second exit / sender. The group has none,
                    // whether nobody was chosen, somebody later chose "no
                    // exit", or the holder is offline (an offline device is
                    // removed from the global configuration and has no
                    // influence on it). The new choice applies, authored at
                    // the merge.
                    None => {
                        *self.config.slot(role) = Some(Route {
                            version: mine(now),
                            target: Some(me),
                        });
                    }
                },
                // Not claiming, but named from before this start (a phone
                // restarted, a PC lost its store): withdraw.
                None if holder == Some(me) && register.is_some_and(|route| route.version.stamp < self.started) => {
                    *self.config.slot(role) = Some(Route {
                        version: weak(register, me),
                        target: None,
                    });
                }
                None => {}
            }
        }
        self.merged = true;
        for role in fallbacks {
            let stamp = self.hlc.stamp(inputs.wall_ms);
            self.fall_back(role, mine(stamp), inputs.receive_ready);
        }
    }

    /// The previous holder of a role falls back (user Q1 / A): a phone to
    /// local, a PC's speaker to forward and its microphone to receive (local
    /// when the virtual microphone is not installed).
    fn fall_back(&mut self, role: Role, version: Version, receive_ready: bool) {
        if self.kind == DeviceKind::Mobile {
            return;
        }
        let on = role == Role::Speaker || receive_ready;
        let modes = self.config.devices.entry(self.me).or_default();
        *modes.slot(role) = Some(Flag { on, version });
    }

    fn react(&mut self, inputs: Inputs<'_>) {
        self.config.devices.entry(self.me).or_default();
        // A phone alone authors its forward ON. Any setter may turn it OFF.
        // Reject a remote enable even when it arrived through another member.
        if self.kind == DeviceKind::Mobile
            && self.config.devices[&self.me]
                .get(Role::Speaker)
                .is_some_and(|flag| flag.on && flag.version.author != self.me)
        {
            let version = Version {
                stamp: self.hlc.stamp(inputs.wall_ms),
                author: self.me,
            };
            self.write_flag(self.me, Role::Speaker, false, version);
        }
        for role in ROLES {
            let route = self.config.get(role);
            let mine = route.and_then(|route| route.target) == Some(self.me);
            match self.kind {
                // Only the phone itself makes it the exit or the sender
                // (§2.7); a version naming it from anybody else is withdrawn.
                DeviceKind::Mobile if mine && route.is_some_and(|route| route.version.author != self.me) => {
                    *self.config.slot(role) = Some(Route {
                        version: weak(route, self.me),
                        target: None,
                    });
                }
                // A PC that becomes the holder turns its fallback switch on,
                // so losing the role later leaves it forwarding / receiving.
                DeviceKind::Desktop if mine && !self.holding[role.index()] => {
                    let wanted = role == Role::Speaker || inputs.receive_ready;
                    if wanted && !self.config.devices[&self.me].on(role) {
                        let stamp = self.hlc.stamp(inputs.wall_ms);
                        self.write_flag(self.me, role, true, Version { stamp, author: self.me });
                    }
                }
                _ => {}
            }
            self.holding[role.index()] = self.config.target(role) == Some(self.me);
        }
    }

    fn write_flag(&mut self, device: DeviceId, role: Role, on: bool, version: Version) {
        *self.config.devices.entry(device).or_default().slot(role) = Some(Flag { on, version });
    }

    /// This device turns its own switch on or off without touching a
    /// register: an automatic local change (forwarding stopped because the
    /// local output was taken over). Returns whether it changed.
    pub fn set_own_switch(&mut self, wall_ms: u64, role: Role, on: bool) -> bool {
        if self.own().flag(role).on == on {
            return false;
        }
        let version = Version {
            stamp: self.hlc.stamp(wall_ms),
            author: self.me,
        };
        if self.merged {
            self.write_flag(self.me, role, on, version);
        } else {
            self.pending.flags[role.index()] = PendingFlag {
                flag: Flag { on, version },
                fresh: true,
            };
        }
        true
    }

    /// A user on this device sets `device`'s mode for `role` (setter decides,
    /// §2.5). Returns whether anything was authored. Before merging, only this
    /// device's own mode can change; it is recorded as a new choice with the
    /// click's stamp. The caller checks capabilities and permissions.
    pub fn select(&mut self, wall_ms: u64, device: DeviceId, role: Role, mode: Mode) -> Result<bool, ControlFailure> {
        if device == self.me && self.kind == DeviceKind::Mobile && role == Role::Microphone && mode == Mode::Shared {
            return Err(ControlFailure::Unsupported);
        }
        if !self.merged {
            if device != self.me {
                return Err(ControlFailure::Busy);
            }
            // A choice made now replaces the old configuration of that item,
            // even when it repeats it (user Q2: it competes by its own stamp).
            let index = role.index();
            let on = mode == Mode::Shared;
            let flag = self.pending.flags[index];
            let unchanged = match mode {
                Mode::Holder => matches!(self.pending.claims[index], Some(Claim::Fresh(_))),
                _ => self.pending.claims[index].is_none() && flag.fresh && flag.flag.on == on,
            };
            if unchanged {
                return Ok(false);
            }
            let version = Version {
                stamp: self.hlc.stamp(wall_ms),
                author: self.me,
            };
            if mode == Mode::Holder {
                if self.kind == DeviceKind::Mobile && role == Role::Speaker {
                    self.pending.flags[index] = PendingFlag {
                        flag: Flag { on: false, version },
                        fresh: true,
                    };
                }
                self.pending.claims[index] = Some(Claim::Fresh(version));
            } else {
                self.pending.claims[index] = None;
                self.pending.flags[index] = PendingFlag {
                    flag: Flag { on, version },
                    fresh: true,
                };
            }
            return Ok(true);
        }
        if !self.online.contains(&device) {
            return Err(ControlFailure::Offline);
        }
        let current = self.config.target(role);
        let on = mode == Mode::Shared;
        let unchanged = match mode {
            Mode::Holder => current == Some(device),
            _ => {
                current != Some(device)
                    && self
                        .config
                        .devices
                        .get(&device)
                        .and_then(|modes| modes.get(role))
                        .is_some_and(|flag| flag.on == on)
            }
        };
        if unchanged {
            return Ok(false);
        }
        let version = Version {
            stamp: self.hlc.stamp(wall_ms),
            author: self.me,
        };
        match mode {
            Mode::Holder => {
                if device == self.me && self.kind == DeviceKind::Mobile && role == Role::Speaker {
                    self.write_flag(device, role, false, version);
                }
                *self.config.slot(role) = Some(Route {
                    version,
                    target: Some(device),
                })
            }
            _ => {
                // Clear(X) only while X is still the holder; the switch takes
                // the same stamp, so "exit -> local" never shows forward.
                if current == Some(device) {
                    *self.config.slot(role) = Some(Route { version, target: None });
                }
                self.write_flag(device, role, on, version);
            }
        }
        self.holding[role.index()] = self.config.target(role) == Some(self.me) && self.holding[role.index()];
        Ok(true)
    }
}

/// The version of an automatic register write that replaces `register`: a
/// vacant role taken by old configuration, or a stale self-target withdrawn.
/// It is just newer than what it replaces, never "now": nobody chose it now,
/// so any real choice made meanwhile (in another partition, by a group this
/// device is joining, or by an online member) stays newer and wins. Merging a
/// whole group therefore keeps the target group's exit even when the joining
/// devices happen to found among themselves first (user 2026-11-11 B).
fn weak(register: Option<Route>, author: DeviceId) -> Version {
    Version {
        stamp: register.map_or(1, |route| route.version.stamp.saturating_add(1)),
        author,
    }
}

/// A device's modes and switches as one engine derives them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DeviceOverview {
    pub device: DeviceId,
    pub speaker: Mode,
    pub microphone: Mode,
    /// The speaker "forward" switch; `None` when unknown here.
    pub forward: Option<bool>,
    /// The microphone "receive" switch; `None` when unknown here.
    pub receive: Option<bool>,
}
/// A snapshot of one device's global audio overview for its own UI: the
/// registers (their versions fence UI tokens) and every online device's
/// modes. While unjoined it holds only this device's own configuration.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Overview {
    pub joined: bool,
    pub speaker: Option<Route>,
    pub microphone: Option<Route>,
    pub devices: Vec<DeviceOverview>,
}
impl Overview {
    pub fn route(&self, role: Role) -> Option<Route> {
        match role {
            Role::Speaker => self.speaker,
            Role::Microphone => self.microphone,
        }
    }
    fn device(&self, device: DeviceId) -> Option<&DeviceOverview> {
        self.devices.iter().find(|entry| entry.device == device)
    }
    pub fn mode(&self, device: DeviceId, role: Role) -> Option<Mode> {
        let entry = self.device(device)?;
        Some(match role {
            Role::Speaker => entry.speaker,
            Role::Microphone => entry.microphone,
        })
    }
    pub fn switch(&self, device: DeviceId, role: Role) -> Option<bool> {
        let entry = self.device(device)?;
        match role {
            Role::Speaker => entry.forward,
            Role::Microphone => entry.receive,
        }
    }
    /// The executed holder of a role (online, after merging).
    pub fn holder(&self, role: Role) -> Option<DeviceId> {
        if !self.joined {
            return None;
        }
        self.devices
            .iter()
            .find(|entry| self.mode(entry.device, role) == Some(Mode::Holder))
            .map(|entry| entry.device)
    }
}

/// What this device executes for one register. There is no "selected but
/// unavailable while offline" state: an offline holder is "none".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Resolution {
    /// Only the audio owner's own failure to read or store its configuration.
    #[default]
    Unknown,
    None,
    /// `available=false`: the holder is online but asleep or paused. Its role
    /// stays in force and forwarding resumes by itself when it wakes.
    Exit {
        version: Version,
        target: DeviceId,
        available: bool,
    },
}
impl Resolution {
    pub fn version(self) -> Option<Version> {
        match self {
            Self::Exit { version, .. } => Some(version),
            _ => None,
        }
    }
    pub fn target(self) -> Option<DeviceId> {
        match self {
            Self::Exit { target, .. } => Some(target),
            _ => None,
        }
    }
    pub fn available(self) -> bool {
        matches!(self, Self::Exit { available: true, .. })
    }
}

#[cfg(test)]
mod tests;
