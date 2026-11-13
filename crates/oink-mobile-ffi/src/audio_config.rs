//! The phone's view of the global audio configuration (protocol-v2 6.1).
//!
//! Nothing is persisted: every core start (and every group switch) begins all
//! local and unjoined, and merges with what the first linked member publishes
//! (`oink_audio::route::Engine`). The phone's own configuration is its endpoint
//! intent: turning the speaker or microphone on makes it the group exit or
//! sender, turning it off withdraws it. As a setter it also changes the modes
//! of the PCs it shows (exit, forward, sender, receive) without asking them.
use oink_audio::route::{AudioConfig, DeviceKind, Engine, Inputs, Mode, OwnConfig, Peer, ROLES, Role, Route, Version};
use oink_foundation::{DeviceId, GroupId};
use oink_protocol::v2::{self, RawValue, dto, schema::WireValue};

/// DeviceFacts key of the global audio configuration.
const AUDIO_CONFIG: u32 = 33;

pub(crate) fn wall_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub(crate) struct PhoneAudio {
    group: Option<GroupId>,
    engine: Option<Engine>,
    /// The endpoint intent generation each role's own choice was made for.
    applied_intents: [Option<u64>; 2],
    applied_forward: Option<u64>,
}

impl PhoneAudio {
    pub(crate) fn new() -> Self {
        Self {
            group: None,
            engine: None,
            applied_intents: [None; 2],
            applied_forward: None,
        }
    }

    /// A new group is a new start: all local, unjoined (user 2026-11-11 Q3).
    /// An endpoint that is on at a group switch belongs to the old
    /// configuration, which the new start resets to local: it is taken as
    /// already applied, never as a new choice. Re-applying it would stamp the
    /// switch moment as a click and take the exit from the group being
    /// joined (§2.5: the target group's exit is kept). Only a later intent
    /// (a new generation) is a new choice; after merging, `follow_roles`
    /// closes the endpoint left on.
    fn enter(&mut self, group: GroupId, local: DeviceId, intents: [Option<u64>; 2], wall_ms: u64) {
        if self.group != Some(group) || self.engine.is_none() {
            let switched = self.engine.is_some();
            let last = self.engine.as_ref().map_or(0, Engine::hlc_last);
            self.engine = Some(Engine::new(
                local,
                DeviceKind::Mobile,
                OwnConfig::local(local),
                last,
                wall_ms,
            ));
            self.group = Some(group);
            self.applied_forward = None;
            self.applied_intents = if switched { intents } else { [None; 2] };
        }
    }

    #[cfg(test)]
    pub(crate) fn engine(&self) -> Option<&Engine> {
        self.engine.as_ref()
    }

    pub(crate) fn joined(&self) -> bool {
        self.engine.as_ref().is_some_and(Engine::merged)
    }

    /// One pass: turn new endpoint intents into this phone's own choices (a
    /// newly enabled endpoint claims the role, a disabled one withdraws it),
    /// then merge or adopt what the linked members publish. Returns, per
    /// role, the register version while this phone executes that role.
    #[cfg(test)]
    pub(crate) fn reconcile(
        &mut self,
        group: GroupId,
        local: DeviceId,
        linked: &[Peer],
        intents: [Option<u64>; 2],
        wall_ms: u64,
    ) -> [Option<Version>; 2] {
        self.reconcile_with_forward(group, local, linked, intents, None, wall_ms)
    }

    pub(crate) fn reconcile_with_forward(
        &mut self,
        group: GroupId,
        local: DeviceId,
        linked: &[Peer],
        intents: [Option<u64>; 2],
        forward: Option<u64>,
        wall_ms: u64,
    ) -> [Option<Version>; 2] {
        self.enter(group, local, intents, wall_ms);
        let mut applied = self.applied_intents;
        let engine = self.engine.as_mut().expect("entered");
        for role in ROLES {
            let index = role.index();
            // One select(Shared) clears our old exit and enables forwarding
            // with the same HLC stamp, only after native capture succeeded.
            let mode = match (role, forward, self.applied_forward) {
                (Role::Speaker, Some(_), old) if old != forward => Some(Mode::Shared),
                (Role::Speaker, Some(_), _) => None,
                (Role::Speaker, None, Some(_)) => Some(if intents[index].is_some() {
                    Mode::Holder
                } else {
                    Mode::Local
                }),
                _ => match intents[index] {
                    Some(_) if intents[index] != applied[index] => Some(Mode::Holder),
                    None if applied[index].is_some() => Some(Mode::Local),
                    _ => None,
                },
            };
            if let Some(mode) = mode
                && let Err(reason) = engine.select(wall_ms, local, role, mode)
            {
                tracing::warn!(reason = reason.number(), "cannot apply the phone's own audio choice");
            }
            applied[index] = intents[index];
        }
        let result = engine.reconcile(Inputs {
            wall_ms,
            linked,
            receive_ready: false,
        });
        for (author, ahead_ms) in result.clock_ahead {
            tracing::warn!(
                author = %author.short(),
                ahead_ms,
                "received an audio configuration stamp far ahead of this clock"
            );
        }
        let versions = ROLES.map(|role| {
            engine
                .route(role)
                .filter(|_| engine.holder(role) == Some(local))
                .map(|route| route.version)
        });
        self.applied_intents = applied;
        self.applied_forward = forward;
        versions
    }

    /// This phone's user sets a device's mode from its card (setter decides).
    pub(crate) fn select(
        &mut self,
        device: DeviceId,
        role: Role,
        mode: Mode,
    ) -> Result<dto::ControlOutcome, dto::ControlFailure> {
        let engine = self.engine.as_mut().ok_or(dto::ControlFailure::Busy)?;
        Ok(if engine.select(wall_ms(), device, role, mode)? {
            dto::ControlOutcome::Applied
        } else {
            dto::ControlOutcome::NoChange
        })
    }

    /// Whether this phone's own native session for `role` may run. Before
    /// merging (alone, or the group not reached yet) the phone's own
    /// configuration decides: a speaker or microphone the user turned on runs
    /// locally at once. Nobody sends to it or demands from it until it is the
    /// group's holder, so this never makes a second exit / sender. After
    /// merging it runs only while the phone holds the role.
    pub(crate) fn runs_own(&self, role: Role) -> bool {
        match &self.engine {
            Some(engine) if engine.merged() => engine.holder(role) == Some(engine.me()),
            Some(engine) => engine.own().holds(role),
            None => false,
        }
    }

    pub(crate) fn route(&self, role: Role) -> Option<Route> {
        self.engine.as_ref()?.route(role)
    }

    pub(crate) fn holder(&self, role: Role) -> Option<DeviceId> {
        self.engine.as_ref()?.holder(role)
    }

    pub(crate) fn mode(&self, device: DeviceId, role: Role) -> Option<Mode> {
        self.engine.as_ref()?.mode(device, role)
    }

    pub(crate) fn switch(&self, device: DeviceId, role: Role) -> Option<bool> {
        self.engine.as_ref()?.switch(device, role)
    }

    /// Publish what this phone executes (DeviceFacts key 33); nothing while
    /// it has not merged, which tells the others it is still unjoined.
    pub(crate) fn apply_facts(&self, raw: &mut v2::RawMap, microphone_controls: dto::DevicePreference) {
        match self.engine.as_ref().and_then(Engine::publication) {
            Some(config) => {
                raw.insert(AUDIO_CONFIG, config.to_wire().write());
            }
            None => {
                raw.remove(&AUDIO_CONFIG);
            }
        }
        raw.insert(18, RawValue::Array(vec![microphone_controls.write()]));
    }
}

/// What a linked member publishes, for the engine: `None` while it is unjoined
/// (or publishes a configuration this build cannot read).
pub(crate) fn peer_config(facts: &dto::DeviceFacts) -> Option<AudioConfig> {
    let wire = facts.audio_config().ok()??;
    match AudioConfig::from_wire(&wire) {
        Ok(config) => Some(config),
        Err(reason) => {
            tracing::warn!(
                reason = reason.number(),
                "ignoring an audio configuration this build cannot read"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oink_protocol::v2::schema::WireDto;

    fn id(n: u8) -> DeviceId {
        DeviceId([n; 32])
    }
    const GROUP: GroupId = GroupId([7; 16]);
    const PHONE: u8 = 50;

    /// A merged PC (smaller ID than the phone, so it founds).
    struct Pc(Engine);
    impl Pc {
        fn new(n: u8, ms: u64) -> Self {
            let pc = id(n);
            let mut engine = Engine::new(pc, DeviceKind::Desktop, OwnConfig::local(pc), 0, ms);
            engine.reconcile(Inputs {
                wall_ms: ms,
                linked: &[Peer {
                    device: id(PHONE),
                    config: None,
                }],
                receive_ready: true,
            });
            assert!(engine.merged());
            Self(engine)
        }
        /// A PC that is the exit since `ms`.
        fn exit(n: u8, ms: u64) -> Self {
            let mut pc = Self::new(n, ms);
            pc.select(ms, n, Role::Speaker, Mode::Holder);
            pc
        }
        fn select(&mut self, ms: u64, device: u8, role: Role, mode: Mode) {
            self.0.select(ms, id(device), role, mode).unwrap();
        }
        /// Hear the phone (or nothing) and publish.
        fn pass(&mut self, ms: u64, phone: Option<&PhoneAudio>) -> Peer {
            let linked: Vec<_> = phone
                .map(|audio| Peer {
                    device: id(PHONE),
                    config: audio.engine().and_then(Engine::publication),
                })
                .into_iter()
                .collect();
            self.0.reconcile(Inputs {
                wall_ms: ms,
                linked: &linked,
                receive_ready: true,
            });
            Peer {
                device: self.0.me(),
                config: self.0.publication(),
            }
        }
        fn mode(&self, role: Role) -> Mode {
            self.0.mode(self.0.me(), role).unwrap()
        }
    }
    fn controls() -> dto::DevicePreference {
        dto::DevicePreference::from_raw(
            [
                (1, dto::LocalOption::MicrophoneControls.write()),
                (2, 1u64.write()),
                (3, RawValue::Map([(1, 100u16.write()), (2, false.write())].into())),
            ]
            .into(),
        )
        .unwrap()
    }

    #[test]
    fn a_phone_starts_all_local_and_withdraws_a_stale_self_target() {
        let phone = id(PHONE);
        // Before its restart the phone was the exit (its own, older version).
        let mut stale = Pc::new(1, 1_000).pass(1_000, None);
        stale.config.as_mut().unwrap().speaker = Some(Route {
            version: Version {
                stamp: 1_000 << 16,
                author: phone,
            },
            target: Some(phone),
        });
        let mut audio = PhoneAudio::new();
        assert!(!audio.joined());
        let versions = audio.reconcile(GROUP, phone, &[stale], [None, None], 5_000);
        assert_eq!(versions, [None, None], "all local after the start");
        assert!(audio.joined());
        let route = audio.route(Role::Speaker).unwrap();
        assert_eq!((route.target, route.version.author), (None, phone), "withdrawn");
        let mut facts = v2::RawMap::new();
        audio.apply_facts(&mut facts, controls());
        assert!(facts.contains_key(&AUDIO_CONFIG));
    }

    /// Q2 / F4: the speaker turned on before the phone joined is a new choice
    /// with its own stamp. An earlier exit yields (the PC falls back to
    /// forward); a later exit wins and the phone goes back to local.
    #[test]
    fn turned_on_before_joining_it_wins_against_an_earlier_exit_and_loses_to_a_later_one() {
        let phone = id(PHONE);
        for (group_exit_ms, phone_wins) in [(1_000, true), (9_000, false)] {
            let mut audio = PhoneAudio::new();
            // Not linked yet: the speaker is turned on at 5 000.
            assert_eq!(audio.reconcile(GROUP, phone, &[], [Some(1), None], 5_000), [None, None]);
            assert!(!audio.joined());
            let mut facts = v2::RawMap::new();
            audio.apply_facts(&mut facts, controls());
            assert!(!facts.contains_key(&AUDIO_CONFIG), "unjoined: nothing published");
            // Then it links to a PC whose exit was chosen earlier / later.
            let mut pc = Pc::exit(1, group_exit_ms);
            let published = pc.pass(group_exit_ms, None);
            let versions = audio.reconcile(GROUP, phone, &[published], [Some(1), None], 10_000);
            assert_eq!(versions[0].is_some(), phone_wins, "exit chosen at {group_exit_ms}");
            let holder = if phone_wins { phone } else { id(1) };
            assert_eq!(audio.holder(Role::Speaker), Some(holder));
            assert_eq!(
                audio.mode(phone, Role::Speaker),
                Some(if phone_wins { Mode::Holder } else { Mode::Local })
            );
            // The PC hears the phone: it forwards when it lost the exit.
            pc.pass(10_010, Some(&audio));
            assert_eq!(
                pc.mode(Role::Speaker),
                if phone_wins { Mode::Shared } else { Mode::Holder }
            );
        }
    }

    /// iOS AudioLifecycleTests (integration round 3): a phone that turns its
    /// speaker on alone (unjoined) runs its own session at once; it publishes
    /// no role, so nobody sends to it until it merged and holds the exit.
    #[test]
    fn its_own_session_runs_before_merging_and_follows_the_role_after() {
        let phone = id(PHONE);
        let mut audio = PhoneAudio::new();
        assert!(!audio.runs_own(Role::Speaker));
        let versions = audio.reconcile(GROUP, phone, &[], [Some(1), None], 5_000);
        assert_eq!(versions, [None, None], "no role, no register version yet");
        assert!(!audio.joined());
        assert!(audio.runs_own(Role::Speaker), "the user's own speaker runs alone");
        assert!(!audio.runs_own(Role::Microphone));
        // Joined against a later online exit: the phone loses the role.
        let peer = Pc::exit(1, 9_000).pass(9_000, None);
        audio.reconcile(GROUP, phone, &[peer], [Some(1), None], 10_000);
        assert!(audio.joined());
        assert!(!audio.runs_own(Role::Speaker));
    }

    /// User 2026-11-11: a later explicit "no exit" in the group is no conflict:
    /// the speaker the phone turned on before joining applies at the merge.
    #[test]
    fn turned_on_before_joining_it_applies_when_the_group_stopped_its_exit_later() {
        let phone = id(PHONE);
        let mut audio = PhoneAudio::new();
        audio.reconcile(GROUP, phone, &[], [Some(1), None], 5_000);
        let mut pc = Pc::exit(1, 1_000);
        pc.select(9_000, 1, Role::Speaker, Mode::Local);
        let published = pc.pass(9_000, None);
        let versions = audio.reconcile(GROUP, phone, &[published], [Some(1), None], 10_000);
        assert!(versions[0].is_some());
        assert_eq!(audio.holder(Role::Speaker), Some(phone));
        assert!(
            audio.route(Role::Speaker).unwrap().version.stamp >= 10_000 << 16,
            "authored at the merge"
        );
        pc.pass(10_010, Some(&audio));
        assert_eq!(pc.0.holder(Role::Speaker), Some(phone), "the PC adopts it");
    }

    /// Q1: another device becomes the exit or the sender later; the phone is
    /// local again (media_session then closes the endpoint with Remote).
    #[test]
    fn a_later_choice_elsewhere_or_a_pc_clear_takes_the_role_away() {
        let phone = id(PHONE);
        let mut pc = Pc::new(1, 1_000);
        let mut audio = PhoneAudio::new();
        let peer = pc.pass(1_000, None);
        let versions = audio.reconcile(GROUP, phone, &[peer], [Some(1), Some(2)], 2_000);
        assert!(versions.iter().all(Option::is_some), "the phone holds both roles");
        // The PC picks itself as the exit.
        let peer = pc.pass(3_000, Some(&audio));
        pc.select(3_000, 1, Role::Speaker, Mode::Holder);
        let peer = Peer {
            config: pc.0.publication(),
            ..peer
        };
        let versions = audio.reconcile(GROUP, phone, &[peer], [Some(1), Some(2)], 3_100);
        assert_eq!(versions[0], None);
        assert_eq!(audio.mode(phone, Role::Speaker), Some(Mode::Local));
        assert!(versions[1].is_some(), "the other role is untouched");
        // The PC turns the phone's microphone back to local: Clear(phone).
        pc.select(4_000, PHONE, Role::Microphone, Mode::Local);
        let peer = pc.pass(4_000, Some(&audio));
        let versions = audio.reconcile(GROUP, phone, &[peer], [None, Some(2)], 4_100);
        assert_eq!(versions, [None, None]);
        assert_eq!(audio.mode(phone, Role::Microphone), Some(Mode::Local));
    }

    /// Only the phone itself makes it the exit or the sender (§2.7).
    #[test]
    fn a_role_given_by_another_device_is_withdrawn() {
        let phone = id(PHONE);
        let mut pc = Pc::new(1, 1_000);
        let mut audio = PhoneAudio::new();
        let peer = pc.pass(1_000, None);
        audio.reconcile(GROUP, phone, &[peer], [None, None], 2_000);
        let peer = pc.pass(2_000, Some(&audio));
        pc.select(3_000, PHONE, Role::Speaker, Mode::Holder);
        let peer = Peer {
            config: pc.0.publication(),
            ..peer
        };
        let versions = audio.reconcile(GROUP, phone, &[peer], [None, None], 3_100);
        assert_eq!(versions, [None, None]);
        let route = audio.route(Role::Speaker).unwrap();
        assert_eq!((route.target, route.version.author), (None, phone));
    }

    #[test]
    fn the_phone_sets_a_pcs_forward_and_receive_switches_as_the_setter() {
        let phone = id(PHONE);
        let pc = id(1);
        let mut audio = PhoneAudio::new();
        let peer = Pc::exit(1, 1_000).pass(1_000, None);
        audio.reconcile(GROUP, phone, &[peer], [None, None], 2_000);
        // The PC is the exit: "forward" clears it and turns its switch on.
        assert_eq!(
            audio.select(pc, Role::Speaker, Mode::Shared),
            Ok(dto::ControlOutcome::Applied)
        );
        assert_eq!(audio.holder(Role::Speaker), None);
        assert_eq!(audio.mode(pc, Role::Speaker), Some(Mode::Shared));
        assert_eq!(audio.switch(pc, Role::Speaker), Some(true));
        assert_eq!(
            audio.select(pc, Role::Microphone, Mode::Shared),
            Ok(dto::ControlOutcome::Applied)
        );
        assert_eq!(audio.mode(pc, Role::Microphone), Some(Mode::Shared));
        assert_eq!(
            audio.select(pc, Role::Microphone, Mode::Shared),
            Ok(dto::ControlOutcome::NoChange)
        );
        // A phone can forward, but cannot receive a remote microphone.
        assert_eq!(
            audio.select(phone, Role::Microphone, Mode::Shared),
            Err(dto::ControlFailure::Unsupported)
        );
    }

    #[test]
    fn a_group_switch_is_a_new_start() {
        let phone = id(PHONE);
        let mut audio = PhoneAudio::new();
        let peer = Pc::new(1, 1_000).pass(1_000, None);
        audio.reconcile(GROUP, phone, &[peer], [Some(1), None], 2_000);
        assert!(audio.joined());
        audio.reconcile(GroupId([8; 16]), phone, &[], [Some(1), None], 3_000);
        assert!(!audio.joined(), "unjoined in the new group");
    }

    /// 2026-11-11 report: a phone whose speaker is on (the exit of its old
    /// group, or alone) joins the PCs' group as a whole-group merge. The
    /// switch is a new start, all local: the speaker that was on is old
    /// configuration, not a click at the switch moment, so the PC keeps the
    /// exit (§2.5). Turning the phone's speaker off afterwards does not clear
    /// the PC's exit either.
    #[test]
    fn a_whole_group_join_keeps_the_target_groups_exit() {
        let phone = id(PHONE);
        let mut audio = PhoneAudio::new();
        // In its old group, not linked: the speaker is turned on at 5 000.
        audio.reconcile(GroupId([8; 16]), phone, &[], [Some(1), None], 5_000);
        assert!(audio.runs_own(Role::Speaker));
        // The PCs' exit was chosen at 1 000. The phone switches to their
        // group at 10 000 with the same endpoint still on.
        let mut pc = Pc::exit(1, 1_000);
        audio.reconcile(GROUP, phone, &[], [Some(1), None], 10_000);
        assert!(!audio.runs_own(Role::Speaker), "a new start is all local");
        let published = pc.pass(10_000, None);
        let versions = audio.reconcile(GROUP, phone, &[published], [Some(1), None], 10_010);
        assert!(audio.joined());
        assert_eq!(versions, [None, None], "the phone does not take the exit");
        assert_eq!(audio.holder(Role::Speaker), Some(id(1)));
        assert_eq!(audio.mode(phone, Role::Speaker), Some(Mode::Local));
        let published = pc.pass(10_020, Some(&audio));
        assert_eq!(pc.mode(Role::Speaker), Mode::Holder, "the PC keeps the exit");
        // The phone's speaker is turned off (media_session closes it once
        // merged): it is not the exit, so nothing is cleared.
        audio.reconcile(GROUP, phone, &[published], [None, None], 11_000);
        assert_eq!(audio.holder(Role::Speaker), Some(id(1)));
        pc.pass(11_010, Some(&audio));
        assert_eq!(pc.mode(Role::Speaker), Mode::Holder);
        // Turning it on again later is a new choice and competes by time.
        let published = pc.pass(12_000, Some(&audio));
        let versions = audio.reconcile(GROUP, phone, &[published], [Some(2), None], 12_000);
        assert!(versions[0].is_some(), "a later click wins against the earlier exit");
    }

    #[test]
    fn successful_capture_clears_the_phone_exit_and_enables_forward_with_one_stamp() {
        let phone = id(PHONE);
        let mut audio = PhoneAudio::new();
        let mut pc = Pc::new(1, 1_000);
        let peer = pc.pass(1_000, None);
        audio.reconcile_with_forward(GROUP, phone, &[peer], [Some(1), None], None, 2_000);
        assert_eq!(audio.holder(Role::Speaker), Some(phone));
        // Capture has not started: the old intent and exit remain untouched.
        let peer = pc.pass(2_100, Some(&audio));
        audio.reconcile_with_forward(GROUP, phone, &[peer], [Some(1), None], None, 2_200);
        assert_eq!(audio.holder(Role::Speaker), Some(phone));
        // Native success atomically changes the speaker intent to forward.
        let peer = pc.pass(2_300, Some(&audio));
        audio.reconcile_with_forward(GROUP, phone, &[peer], [None, None], Some(2), 2_400);
        let config = audio.engine().unwrap().publication().unwrap();
        let route = config.speaker.unwrap();
        let flag = config.devices[&phone].get(Role::Speaker).unwrap();
        assert_eq!(route.target, None);
        assert!(flag.on);
        assert_eq!(route.version, flag.version);
        assert_eq!(audio.mode(phone, Role::Speaker), Some(Mode::Shared));
    }
}
