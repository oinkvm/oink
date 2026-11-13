//! The global audio configuration: HLC, the §2.4 merge table for PCs and
//! phones, the founder rule, convergence and the §2.5 rules after merging.
use super::*;

fn id(n: u8) -> DeviceId {
    DeviceId([n; 32])
}
fn at(ms: u64) -> u64 {
    ms << LOGICAL_BITS
}
fn v(ms: u64, author: u8) -> Version {
    Version {
        stamp: at(ms),
        author: id(author),
    }
}
fn flag(on: bool, ms: u64, author: u8) -> Flag {
    Flag {
        on,
        version: v(ms, author),
    }
}
fn own(me: u8, exit: bool, send: bool, forward: bool, receive: bool) -> OwnConfig {
    OwnConfig {
        speaker_exit: exit,
        microphone_send: send,
        forward: Flag {
            on: forward,
            version: Version {
                stamp: 1,
                author: id(me),
            },
        },
        receive: Flag {
            on: receive,
            version: Version {
                stamp: 1,
                author: id(me),
            },
        },
    }
}
fn speaker(route: Option<Route>) -> AudioConfig {
    AudioConfig {
        speaker: route,
        ..AudioConfig::default()
    }
}
fn route(ms: u64, author: u8, target: Option<u8>) -> Route {
    Route {
        version: v(ms, author),
        target: target.map(id),
    }
}
fn peer(device: u8, config: Option<AudioConfig>) -> Peer {
    Peer {
        device: id(device),
        config,
    }
}
fn pass(engine: &mut Engine, wall_ms: u64, linked: &[Peer]) -> Reconciled {
    engine.reconcile(Inputs {
        wall_ms,
        linked,
        receive_ready: true,
    })
}

// ---- hybrid logical clock -------------------------------------------------

#[test]
fn the_clock_stays_monotonic_when_the_wall_clock_steps_back() {
    let mut clock = Hlc::default();
    let first = clock.stamp(1_000);
    assert_eq!(first, at(1_000));
    let back = clock.stamp(400);
    assert!(back > first, "a stepped-back wall clock never repeats a stamp");
    assert_eq!(back, at(1_000) + 1);
    assert_eq!(clock.stamp(1_000), at(1_000) + 2, "same millisecond: the counter moves");
    assert_eq!(clock.stamp(1_001), at(1_001));
    assert_ne!(Hlc::default().stamp(0), 0, "a stamp is never zero");
}

#[test]
fn a_choice_made_after_seeing_another_is_newer_whatever_the_clocks_say() {
    // A's clock runs 30 s ahead of B's.
    let mut a = Hlc::default();
    let mut b = Hlc::default();
    let seen = a.stamp(40_000);
    assert_eq!(b.observe(seen, 10_000), None, "30 s ahead is below the warning");
    let later = b.stamp(10_001);
    assert!(later > seen, "causal order does not depend on clock skew");
    assert_eq!(b.last(), later);
}

#[test]
fn the_counter_overflow_carries_into_the_millisecond_part() {
    let mut clock = Hlc::new(at(5) + 0xffff);
    assert_eq!(clock.stamp(5), at(6));
    assert_eq!(clock.stamp(5), at(6) + 1);
}

#[test]
fn a_stamp_far_ahead_is_reported_once_and_never_truncated() {
    let mut clock = Hlc::default();
    let ahead = at(1_000 + CLOCK_AHEAD_WARNING_MS + 5);
    assert_eq!(clock.observe(ahead, 1_000), Some(CLOCK_AHEAD_WARNING_MS + 5));
    assert_eq!(clock.last(), ahead, "kept as received");
    assert_eq!(clock.observe(ahead, 1_000), None, "known stamps are not reported again");
    assert!(clock.stamp(1_000) > ahead);
}

#[test]
fn equal_stamps_order_by_author_then_by_target_or_value() {
    let mut x = speaker(Some(route(7, 1, Some(1))));
    let y = speaker(Some(route(7, 2, Some(1))));
    x.observe(&y);
    assert_eq!(x.speaker, y.speaker, "the larger device ID wins a tie");
    // Same version, different targets / values (an author lost its store).
    let a = AudioConfig {
        speaker: Some(route(7, 1, Some(1))),
        microphone: None,
        devices: BTreeMap::from([(
            id(3),
            DeviceModes {
                forward: Some(flag(false, 7, 1)),
                receive: None,
            },
        )]),
    };
    let b = AudioConfig {
        speaker: Some(route(7, 1, Some(2))),
        microphone: None,
        devices: BTreeMap::from([(
            id(3),
            DeviceModes {
                forward: Some(flag(true, 7, 1)),
                receive: None,
            },
        )]),
    };
    let (mut first, mut second) = (a.clone(), b.clone());
    first.observe(&b);
    second.observe(&a);
    assert_eq!(first, second);
    assert!(first.devices[&id(3)].forward.unwrap().on);
    assert!(!first.observe(&a), "an older or equal version changes nothing");
}

#[test]
fn the_configuration_round_trips_through_the_wire() {
    let config = AudioConfig {
        speaker: Some(route(9, 2, Some(3))),
        microphone: Some(route(4, 1, None)),
        devices: BTreeMap::from([
            (
                id(2),
                DeviceModes {
                    forward: Some(flag(true, 3, 2)),
                    receive: Some(flag(false, 3, 2)),
                },
            ),
            (
                id(3),
                DeviceModes {
                    forward: None,
                    receive: Some(flag(true, 5, 1)),
                },
            ),
        ]),
    };
    assert_eq!(AudioConfig::from_wire(&config.to_wire()).unwrap(), config);
}

// ---- §2.4 merge table -------------------------------------------------------

/// The holder of the received register: this device, another online one, or none.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Target {
    Me,
    Other,
    Nobody,
}
/// This device's own claim: carried from before the start, chosen after the
/// start (click at 250 ms), or none.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Own {
    Old,
    Fresh,
    No,
}
const ME: u8 = 5;
const OTHER: u8 = 9;
/// Starts at 200 ms, the group register was chosen at `register_ms`, merges at 400 ms.
fn merge_cell(kind: DeviceKind, claim: Own, target: Target, register_ms: u64) -> Engine {
    let mut engine = Engine::new(id(ME), kind, own(ME, claim == Own::Old, false, false, false), 0, 200);
    if claim == Own::Fresh {
        assert_eq!(engine.select(250, id(ME), Role::Speaker, Mode::Holder), Ok(true));
    }
    let register = match target {
        Target::Me => Some(route(register_ms, OTHER, Some(ME))),
        Target::Other => Some(route(register_ms, OTHER, Some(OTHER))),
        Target::Nobody => None,
    };
    let mut group = speaker(register);
    group.devices.insert(
        id(OTHER),
        DeviceModes {
            forward: Some(flag(false, 100, OTHER)),
            receive: None,
        },
    );
    assert!(pass(&mut engine, 400, &[peer(OTHER, Some(group))]).merged);
    engine
}

#[test]
fn a_pc_merges_by_the_table() {
    use {Own::*, Target::*};
    let d = DeviceKind::Desktop;
    // Old claim: T = me keeps it without authoring.
    let e = merge_cell(d, Old, Me, 100);
    assert_eq!(e.route(Role::Speaker), Some(route(100, OTHER, Some(ME))));
    assert_eq!(e.mode(id(ME), Role::Speaker), Some(Mode::Holder));
    // Old claim yields to another online holder and falls back to forward (user A).
    let e = merge_cell(d, Old, Other, 100);
    assert_eq!(e.holder(Role::Speaker), Some(id(OTHER)));
    assert_eq!(e.mode(id(ME), Role::Speaker), Some(Mode::Shared));
    // Old claim with nobody holding: it takes the role, weaker than any real
    // choice (just above what it replaces).
    let e = merge_cell(d, Old, Nobody, 100);
    let exit = e.route(Role::Speaker).unwrap();
    assert_eq!(exit.target, Some(id(ME)));
    assert_eq!(
        exit.version,
        Version {
            stamp: 1,
            author: id(ME)
        }
    );
    // New choice: keeps T = me.
    let e = merge_cell(d, Fresh, Me, 100);
    assert_eq!(e.route(Role::Speaker), Some(route(100, OTHER, Some(ME))));
    // New choice against another holder: the later choice wins (user Q2).
    let e = merge_cell(d, Fresh, Other, 100);
    assert_eq!(e.route(Role::Speaker).unwrap().target, Some(id(ME)));
    assert_eq!(
        e.route(Role::Speaker).unwrap().version.millis(),
        250,
        "the click stamp, not the merge"
    );
    let e = merge_cell(d, Fresh, Other, 300);
    assert_eq!(e.holder(Role::Speaker), Some(id(OTHER)));
    assert_eq!(
        e.mode(id(ME), Role::Speaker),
        Some(Mode::Shared),
        "a PC that loses forwards"
    );
    // New choice with nobody holding: no conflict, it applies at the merge.
    let e = merge_cell(d, Fresh, Nobody, 100);
    let exit = e.route(Role::Speaker).unwrap();
    assert_eq!(exit.target, Some(id(ME)));
    assert!(exit.version.stamp >= at(400) && exit.version.author == id(ME));
    // Not claiming but named from before the start: withdrawn, just above
    // the stale version.
    let e = merge_cell(d, No, Me, 100);
    let cleared = e.route(Role::Speaker).unwrap();
    assert_eq!((cleared.target, cleared.version.author), (None, id(ME)));
    assert_eq!(cleared.version.stamp, at(100) + 1);
    // ...unless somebody chose it after this start (a setter's new choice).
    let e = merge_cell(d, No, Me, 300);
    assert_eq!(e.holder(Role::Speaker), Some(id(ME)));
    // Not claiming: nothing to do.
    let e = merge_cell(d, No, Other, 100);
    assert_eq!(e.route(Role::Speaker), Some(route(100, OTHER, Some(OTHER))));
    assert_eq!(e.mode(id(ME), Role::Speaker), Some(Mode::Local));
    let e = merge_cell(d, No, Nobody, 100);
    assert_eq!(e.route(Role::Speaker), None);
}

#[test]
fn a_phone_merges_by_the_table_and_falls_back_to_local() {
    use {Own::*, Target::*};
    let m = DeviceKind::Mobile;
    // A phone starts all local: an "old claim" never exists, so it never yields.
    let e = merge_cell(m, Old, Other, 100);
    assert_eq!(e.mode(id(ME), Role::Speaker), Some(Mode::Local));
    assert_eq!(e.route(Role::Speaker), Some(route(100, OTHER, Some(OTHER))));
    // Named from before its restart (it was the exit when killed): withdrawn.
    let e = merge_cell(m, Old, Me, 100);
    assert_eq!(e.route(Role::Speaker).unwrap().target, None);
    let e = merge_cell(m, No, Me, 100);
    assert_eq!(e.route(Role::Speaker).unwrap().target, None);
    // Turned on before joining: earlier group choice -> the phone takes over.
    let e = merge_cell(m, Fresh, Other, 100);
    assert_eq!(e.holder(Role::Speaker), Some(id(ME)));
    // Later group choice -> the phone goes back to local.
    let e = merge_cell(m, Fresh, Other, 300);
    assert_eq!(e.holder(Role::Speaker), Some(id(OTHER)));
    assert_eq!(e.mode(id(ME), Role::Speaker), Some(Mode::Local));
    let e = merge_cell(m, Fresh, Me, 100);
    assert_eq!(e.holder(Role::Speaker), Some(id(ME)));
    let e = merge_cell(m, Fresh, Nobody, 100);
    let exit = e.route(Role::Speaker).unwrap();
    assert_eq!(exit.target, Some(id(ME)));
    assert!(exit.version.stamp >= at(400), "authored at the merge");
    let e = merge_cell(m, No, Nobody, 100);
    assert_eq!(e.route(Role::Speaker), None);
    let e = merge_cell(m, No, Other, 100);
    assert_eq!(e.mode(id(ME), Role::Speaker), Some(Mode::Local));
}

#[test]
fn a_phone_starts_all_local_whatever_it_is_given() {
    let phone = Engine::new(id(ME), DeviceKind::Mobile, own(ME, true, true, true, true), 0, 10);
    assert_eq!(phone.own(), OwnConfig::local(id(ME)));
    assert_eq!(phone.publication(), None, "unjoined devices publish nothing");
    let mut phone = phone;
    assert_eq!(
        phone.select(11, id(ME), Role::Microphone, Mode::Shared),
        Err(ControlFailure::Unsupported)
    );
}

#[test]
fn a_pc_without_the_virtual_microphone_yields_its_sender_role_to_local() {
    let mut engine = Engine::new(id(ME), DeviceKind::Desktop, own(ME, false, true, false, true), 0, 200);
    let group = AudioConfig {
        microphone: Some(route(100, OTHER, Some(OTHER))),
        ..AudioConfig::default()
    };
    engine.reconcile(Inputs {
        wall_ms: 400,
        linked: &[peer(OTHER, Some(group.clone()))],
        receive_ready: false,
    });
    assert_eq!(engine.mode(id(ME), Role::Microphone), Some(Mode::Local));
    let mut ready = Engine::new(id(ME), DeviceKind::Desktop, own(ME, false, true, false, false), 0, 200);
    pass(&mut ready, 400, &[peer(OTHER, Some(group))]);
    assert_eq!(
        ready.mode(id(ME), Role::Microphone),
        Some(Mode::Shared),
        "falls back to receive"
    );
}

#[test]
fn new_choices_keep_their_stamp_and_old_switches_are_restamped() {
    // Old forward switch: re-stamped at merge, beating a stale entry about me.
    let mut engine = Engine::new(id(ME), DeviceKind::Desktop, own(ME, false, false, true, false), 0, 200);
    let mut group = AudioConfig::default();
    group.devices.insert(
        id(ME),
        DeviceModes {
            forward: Some(flag(false, 100, OTHER)),
            receive: None,
        },
    );
    pass(&mut engine, 400, &[peer(OTHER, Some(group))]);
    let forward = engine.own().forward;
    assert!(forward.on && forward.version.stamp >= at(400) && forward.version.author == id(ME));
    // A setter changed my switch after my start (300); my own new choice at
    // 250 is older and loses, at 350 it is newer and wins.
    for (click, expected) in [(250, true), (350, false)] {
        let mut engine = Engine::new(id(ME), DeviceKind::Desktop, own(ME, false, false, false, false), 0, 200);
        assert_eq!(engine.select(click, id(ME), Role::Speaker, Mode::Local), Ok(true));
        let mut group = AudioConfig::default();
        group.devices.insert(
            id(ME),
            DeviceModes {
                forward: Some(flag(true, 300, OTHER)),
                receive: None,
            },
        );
        pass(&mut engine, 400, &[peer(OTHER, Some(group))]);
        assert_eq!(engine.own().forward.on, expected, "click at {click}");
        if !expected {
            assert_eq!(engine.own().forward.version, v(350, ME), "never re-stamped");
        }
    }
}

#[test]
fn a_holder_that_is_not_online_executes_as_none() {
    let mut engine = Engine::new(id(ME), DeviceKind::Desktop, own(ME, false, false, true, false), 0, 200);
    // OTHER publishes a register naming a device it does not list.
    let group = speaker(Some(route(100, OTHER, Some(7))));
    pass(&mut engine, 400, &[peer(OTHER, Some(group))]);
    assert_eq!(
        engine.route(Role::Speaker).unwrap().target,
        Some(id(7)),
        "the register is kept"
    );
    assert_eq!(engine.holder(Role::Speaker), None);
    assert_eq!(
        engine.resolve(Role::Speaker, &BTreeSet::from([id(7)])),
        Resolution::None
    );
    assert_eq!(
        engine.mode(id(7), Role::Speaker),
        None,
        "offline devices are not in the overview"
    );
    // Online but asleep: the role stays in force, unavailable.
    let mut listed = speaker(Some(route(100, OTHER, Some(OTHER))));
    listed.devices.insert(id(OTHER), DeviceModes::default());
    pass(&mut engine, 500, &[peer(OTHER, Some(listed))]);
    assert_eq!(
        engine.resolve(Role::Speaker, &BTreeSet::new()),
        Resolution::Exit {
            version: v(100, OTHER),
            target: id(OTHER),
            available: false
        }
    );
}

// ---- founder rule -------------------------------------------------------------

#[test]
fn the_smallest_unjoined_device_founds_the_configuration() {
    let mut small = Engine::new(id(1), DeviceKind::Desktop, own(1, true, false, false, false), 0, 10);
    let mut large = Engine::new(id(2), DeviceKind::Desktop, own(2, true, false, false, false), 0, 10);
    // Alone (links still coming up): nobody merges and no role is executed.
    assert!(!pass(&mut small, 20, &[]).merged);
    assert_eq!(small.resolve(Role::Speaker, &BTreeSet::from([id(1)])), Resolution::None);
    assert_eq!(
        small.mode(id(1), Role::Speaker),
        Some(Mode::Holder),
        "its own configuration still shows"
    );
    // Both unjoined, linked: only the smaller founds.
    assert!(!pass(&mut large, 30, &[peer(1, None)]).merged);
    assert!(pass(&mut small, 30, &[peer(2, None)]).merged);
    assert_eq!(small.holder(Role::Speaker), Some(id(1)));
    // The other one merges with it and yields its old claim: exactly one exit.
    pass(&mut large, 40, &[peer(1, small.publication())]);
    assert_eq!(large.holder(Role::Speaker), Some(id(1)));
    assert_eq!(large.mode(id(2), Role::Speaker), Some(Mode::Shared));
}

#[test]
fn a_chain_has_two_founders_that_converge_by_time() {
    // 1 - 3 - 2: devices 1 and 2 are each the smallest of their links.
    let mut net = Net::new(&[
        (1, DeviceKind::Desktop, true),
        (3, DeviceKind::Desktop, false),
        (2, DeviceKind::Desktop, true),
    ]);
    net.link(0, 1);
    net.link(1, 2);
    net.settle(100);
    assert!(net.nodes.iter().all(|node| node.engine.merged()));
    let holders = net.holders(Role::Speaker);
    assert_eq!(holders.len(), 1, "exactly one exit: {holders:?}");
    assert!(net.agree());
}

// ---- convergence and §2.5 ---------------------------------------------------------

/// A small network of engines exchanging their publications.
struct Node {
    engine: Engine,
    skew_ms: i64,
    receive_ready: bool,
}
struct Net {
    nodes: Vec<Node>,
    links: BTreeSet<(usize, usize)>,
}
impl Net {
    fn new(devices: &[(u8, DeviceKind, bool)]) -> Self {
        Self {
            nodes: devices
                .iter()
                .map(|&(n, kind, exit)| Node {
                    engine: Engine::new(id(n), kind, own(n, exit, false, false, false), 0, 1),
                    skew_ms: 0,
                    receive_ready: true,
                })
                .collect(),
            links: BTreeSet::new(),
        }
    }
    fn link(&mut self, a: usize, b: usize) {
        self.links.insert((a.min(b), a.max(b)));
    }
    fn unlink(&mut self, a: usize, b: usize) {
        self.links.remove(&(a.min(b), a.max(b)));
    }
    fn wall(&self, node: usize, real_ms: u64) -> u64 {
        (real_ms as i64 + self.nodes[node].skew_ms) as u64
    }
    fn settle(&mut self, real_ms: u64) {
        for round in 0..12 {
            let published: Vec<_> = self.nodes.iter().map(|node| node.engine.publication()).collect();
            for index in 0..self.nodes.len() {
                let linked: Vec<_> = self
                    .links
                    .iter()
                    .filter_map(|&(a, b)| match index {
                        _ if a == index => Some(b),
                        _ if b == index => Some(a),
                        _ => None,
                    })
                    .map(|other| Peer {
                        device: self.nodes[other].engine.me(),
                        config: published[other].clone(),
                    })
                    .collect();
                let wall = self.wall(index, real_ms + round);
                let ready = self.nodes[index].receive_ready;
                self.nodes[index].engine.reconcile(Inputs {
                    wall_ms: wall,
                    linked: &linked,
                    receive_ready: ready,
                });
            }
        }
    }
    fn select(&mut self, node: usize, real_ms: u64, device: u8, role: Role, mode: Mode) {
        let wall = self.wall(node, real_ms);
        self.nodes[node].engine.select(wall, id(device), role, mode).unwrap();
    }
    /// Devices that execute themselves as the holder.
    fn holders(&self, role: Role) -> Vec<DeviceId> {
        self.nodes
            .iter()
            .filter(|node| node.engine.holder(role) == Some(node.engine.me()))
            .map(|node| node.engine.me())
            .collect()
    }
    fn agree(&self) -> bool {
        let first = self.nodes[0].engine.route(Role::Speaker);
        self.nodes.iter().all(|node| node.engine.route(Role::Speaker) == first)
    }
    fn mode(&self, node: usize, role: Role) -> Mode {
        let engine = &self.nodes[node].engine;
        engine.mode(engine.me(), role).unwrap()
    }
}

#[test]
fn concurrent_setters_converge_on_exactly_one_exit_in_any_order() {
    // §2.2: A makes B the exit while D makes C the exit, at the same wall time.
    let devices = [1, 2, 3, 4];
    let mut engines: Vec<Engine> = devices
        .iter()
        .map(|&n| Engine::new(id(n), DeviceKind::Desktop, own(n, false, false, false, false), 0, 1))
        .collect();
    let everyone: Vec<Peer> = devices.iter().map(|&n| peer(n, None)).collect();
    // Device 1 founds; everybody merges with it and lists everybody.
    for _ in 0..3 {
        let published: Vec<_> = engines.iter().map(Engine::publication).collect();
        for (index, engine) in engines.iter_mut().enumerate() {
            let linked: Vec<_> = everyone
                .iter()
                .enumerate()
                .filter(|(other, _)| *other != index)
                .map(|(other, p)| Peer {
                    device: p.device,
                    config: published[other].clone(),
                })
                .collect();
            pass(engine, 10, &linked);
        }
    }
    engines[0].select(100, id(2), Role::Speaker, Mode::Holder).unwrap();
    engines[3].select(100, id(3), Role::Speaker, Mode::Holder).unwrap();
    let a = engines[0].publication().unwrap();
    let d = engines[3].publication().unwrap();
    for order in [[&a, &d], [&d, &a]] {
        let mut seen = AudioConfig::default();
        for config in order {
            seen.observe(config);
        }
        assert_eq!(seen.speaker.unwrap().target, Some(id(3)), "(stamp, author) decides");
    }
}

#[test]
fn a_pc_that_becomes_a_holder_turns_its_switch_on_and_falls_back_to_it() {
    let mut net = Net::new(&[
        (1, DeviceKind::Desktop, false),
        (2, DeviceKind::Desktop, false),
        (3, DeviceKind::Mobile, false),
    ]);
    net.link(0, 1);
    net.link(0, 2);
    net.link(1, 2);
    net.settle(10);
    // PC 1 makes PC 2 the exit and the microphone sender.
    net.select(0, 100, 2, Role::Speaker, Mode::Holder);
    net.select(0, 100, 2, Role::Microphone, Mode::Holder);
    net.settle(110);
    assert_eq!(net.holders(Role::Speaker), [id(2)]);
    let own = net.nodes[1].engine.own();
    assert!(own.forward.on && own.receive.on, "masked while holding");
    // The phone takes both roles: PC 2 forwards and receives (user Q1).
    net.select(2, 200, 3, Role::Speaker, Mode::Holder);
    net.select(2, 200, 3, Role::Microphone, Mode::Holder);
    net.settle(210);
    assert_eq!(net.holders(Role::Speaker), [id(3)]);
    assert_eq!(net.mode(1, Role::Speaker), Mode::Shared);
    assert_eq!(net.mode(1, Role::Microphone), Mode::Shared);
    // PC 1 takes them back: the phone falls back to local.
    net.select(0, 300, 1, Role::Speaker, Mode::Holder);
    net.select(0, 300, 1, Role::Microphone, Mode::Holder);
    net.settle(310);
    assert_eq!(net.mode(2, Role::Speaker), Mode::Local);
    assert_eq!(net.mode(2, Role::Microphone), Mode::Local);
    // Every device sees the same modes.
    for node in &net.nodes {
        assert_eq!(node.engine.mode(id(3), Role::Speaker), Some(Mode::Local));
        assert_eq!(node.engine.mode(id(2), Role::Speaker), Some(Mode::Shared));
        assert_eq!(node.engine.mode(id(1), Role::Speaker), Some(Mode::Holder));
    }
}

#[test]
fn a_setter_clears_the_holder_and_writes_its_switch_with_one_stamp() {
    let mut net = Net::new(&[(1, DeviceKind::Desktop, false), (2, DeviceKind::Desktop, false)]);
    net.link(0, 1);
    net.settle(10);
    net.select(0, 100, 2, Role::Speaker, Mode::Holder);
    net.settle(110);
    for (mode, on) in [(Mode::Local, false), (Mode::Holder, true), (Mode::Shared, true)] {
        if mode == Mode::Holder {
            net.select(0, 200, 2, Role::Speaker, Mode::Holder);
            net.settle(210);
            continue;
        }
        net.select(0, 150, 2, Role::Speaker, mode);
        let engine = &net.nodes[0].engine;
        let cleared = engine.route(Role::Speaker).unwrap();
        let switch = engine.config.devices[&id(2)].forward.unwrap();
        assert_eq!(cleared.target, None);
        assert_eq!(switch.version, cleared.version, "one stamp: never shows the other mode");
        assert_eq!(switch.on, on);
        net.settle(160);
        assert_eq!(net.mode(1, Role::Speaker), mode);
    }
    // Clear only while the device is still the holder: a late click on an
    // old card does not remove a newer holder.
    net.select(1, 300, 1, Role::Speaker, Mode::Holder);
    net.settle(310);
    net.select(0, 320, 2, Role::Speaker, Mode::Local);
    net.settle(330);
    assert_eq!(net.holders(Role::Speaker), [id(1)]);
}

#[test]
fn a_phone_withdraws_a_role_another_device_gave_it() {
    let mut net = Net::new(&[(1, DeviceKind::Desktop, false), (2, DeviceKind::Mobile, false)]);
    net.link(0, 1);
    net.settle(10);
    net.select(0, 100, 2, Role::Speaker, Mode::Holder);
    net.settle(110);
    assert!(net.holders(Role::Speaker).is_empty());
    let route = net.nodes[0].engine.route(Role::Speaker).unwrap();
    assert_eq!((route.target, route.version.author), (None, id(2)));
}

#[test]
fn the_publication_lists_one_hop_and_relays_a_pending_choice() {
    // 1 - 2 - 3 (1 and 3 not linked).
    let mut net = Net::new(&[
        (1, DeviceKind::Desktop, false),
        (2, DeviceKind::Desktop, false),
        (3, DeviceKind::Desktop, false),
    ]);
    net.link(0, 1);
    net.link(1, 2);
    net.settle(10);
    let listed = |net: &Net, node: usize| {
        net.nodes[node]
            .engine
            .publication()
            .unwrap()
            .devices
            .keys()
            .copied()
            .collect::<Vec<_>>()
    };
    assert_eq!(listed(&net, 0), [id(1), id(2)]);
    assert_eq!(listed(&net, 1), [id(1), id(2), id(3)]);
    assert!(net.nodes[0].engine.online().contains(&id(3)), "known through device 2");
    // Device 1 turns device 3's forward on: it publishes that entry until 2 relays it.
    net.select(0, 100, 3, Role::Speaker, Mode::Shared);
    assert!(listed(&net, 0).contains(&id(3)));
    net.settle(110);
    assert_eq!(net.mode(2, Role::Speaker), Mode::Shared);
    assert_eq!(
        listed(&net, 0),
        [id(1), id(2)],
        "relayed: device 1 stops vouching for 3"
    );
    // Device 3 goes offline: it drops out of every overview, no ghost entry.
    net.unlink(1, 2);
    net.settle(200);
    assert!(!net.nodes[0].engine.online().contains(&id(3)));
    assert_eq!(net.nodes[0].engine.mode(id(3), Role::Speaker), None);
}

#[test]
fn a_restarted_pc_with_a_persisted_exit_yields_and_forwards() {
    // F5: Mac (1) is the exit; Windows (2) restarts claiming the exit.
    let mut net = Net::new(&[(1, DeviceKind::Desktop, false), (2, DeviceKind::Desktop, false)]);
    net.link(0, 1);
    net.settle(10);
    net.select(0, 100, 1, Role::Speaker, Mode::Holder);
    net.settle(110);
    net.unlink(0, 1);
    net.nodes[1].engine = Engine::new(id(2), DeviceKind::Desktop, own(2, true, false, false, false), 0, 5_000);
    net.settle(5_000);
    assert!(!net.nodes[1].engine.merged(), "no links yet: no merge, no role");
    net.link(0, 1);
    net.settle(5_100);
    assert_eq!(net.holders(Role::Speaker), [id(1)]);
    assert_eq!(net.mode(1, Role::Speaker), Mode::Shared);
}

#[test]
fn a_new_choice_on_a_restarted_pc_competes_by_time() {
    let mut net = Net::new(&[(1, DeviceKind::Desktop, false), (2, DeviceKind::Desktop, false)]);
    net.link(0, 1);
    net.settle(10);
    net.select(0, 100, 1, Role::Speaker, Mode::Holder);
    net.settle(110);
    net.unlink(0, 1);
    net.nodes[1].engine = Engine::new(id(2), DeviceKind::Desktop, own(2, false, false, false, false), 0, 5_000);
    // The user picks Windows as the exit before it connects.
    net.select(1, 5_010, 2, Role::Speaker, Mode::Holder);
    net.link(0, 1);
    net.settle(5_100);
    assert_eq!(net.holders(Role::Speaker), [id(2)]);
    assert_eq!(
        net.mode(0, Role::Speaker),
        Mode::Shared,
        "the Mac falls back to forward"
    );
}

#[test]
fn an_offline_exit_drops_out_and_comes_back_unless_replaced() {
    let mut net = Net::new(&[
        (1, DeviceKind::Desktop, false),
        (2, DeviceKind::Desktop, false),
        (3, DeviceKind::Desktop, false),
    ]);
    for (a, b) in [(0, 1), (0, 2), (1, 2)] {
        net.link(a, b);
    }
    net.settle(10);
    net.select(0, 100, 3, Role::Speaker, Mode::Holder);
    net.settle(110);
    // Device 3 drops off without a restart.
    net.unlink(0, 2);
    net.unlink(1, 2);
    net.settle(200);
    assert_eq!(
        net.nodes[0].engine.holder(Role::Speaker),
        None,
        "no exit while it is offline"
    );
    assert_eq!(net.nodes[0].engine.route(Role::Speaker).unwrap().target, Some(id(3)));
    // Back without anybody replacing it: it is the exit again by itself.
    net.link(0, 2);
    net.link(1, 2);
    net.settle(300);
    assert_eq!(net.holders(Role::Speaker), [id(3)]);
    // Offline again; meanwhile device 1 becomes the exit.
    net.unlink(0, 2);
    net.unlink(1, 2);
    net.settle(400);
    net.select(0, 410, 1, Role::Speaker, Mode::Holder);
    net.settle(420);
    // Restarted with its persisted claim: yields and forwards.
    let restarted = net.nodes[2].engine.own();
    net.nodes[2].engine = Engine::new(id(3), DeviceKind::Desktop, restarted, 0, 500);
    net.link(0, 2);
    net.link(1, 2);
    net.settle(510);
    assert_eq!(net.holders(Role::Speaker), [id(1)]);
    assert_eq!(net.mode(2, Role::Speaker), Mode::Shared);
}

#[test]
fn a_reconnect_without_restart_competes_by_time_both_ways() {
    // Device 2 drops off and comes back without restarting (user Q3).
    for chosen_while_offline in [false, true] {
        let mut net = Net::new(&[(1, DeviceKind::Desktop, false), (2, DeviceKind::Desktop, false)]);
        net.link(0, 1);
        net.settle(10);
        let first = if chosen_while_offline { 1 } else { 2 };
        net.select(0, 100, first, Role::Speaker, Mode::Holder);
        net.settle(110);
        net.unlink(0, 1);
        net.settle(200);
        if chosen_while_offline {
            // It is picked as the exit on its side of the partition: it wins.
            net.select(1, 300, 2, Role::Speaker, Mode::Holder);
        } else {
            // Somebody else is picked while it is away: it falls back.
            net.select(0, 300, 1, Role::Speaker, Mode::Holder);
        }
        net.link(0, 1);
        net.settle(400);
        let (winner, loser) = if chosen_while_offline { (id(2), 0) } else { (id(1), 1) };
        assert_eq!(
            net.holders(Role::Speaker),
            [winner],
            "offline choice: {chosen_while_offline}"
        );
        assert_eq!(
            net.mode(loser, Role::Speaker),
            Mode::Shared,
            "the PC that lost forwards"
        );
        assert!(net.agree());
    }
}

#[test]
fn a_healed_partition_keeps_the_later_choice_and_causal_order_under_clock_skew() {
    let mut net = Net::new(&[(1, DeviceKind::Desktop, false), (2, DeviceKind::Desktop, false)]);
    // Device 1's clock is 20 s ahead.
    net.nodes[0].skew_ms = 20_000;
    net.link(0, 1);
    net.settle(1_000);
    // Online: device 1 picks itself, then device 2 (slow clock) picks itself
    // after seeing that. Causal order wins although 2's wall clock is behind.
    net.select(0, 2_000, 1, Role::Speaker, Mode::Holder);
    net.settle(2_010);
    assert_eq!(net.holders(Role::Speaker), [id(1)]);
    net.select(1, 3_000, 2, Role::Speaker, Mode::Holder);
    net.settle(3_010);
    assert_eq!(net.holders(Role::Speaker), [id(2)]);
    // Partition: concurrent choices are ordered by wall clock. 30 s apart in
    // real time, more than the skew: the later one wins after healing.
    net.unlink(0, 1);
    net.settle(5_000);
    net.select(0, 10_000, 1, Role::Speaker, Mode::Holder);
    net.select(1, 39_000, 2, Role::Speaker, Mode::Local);
    net.select(1, 40_000, 2, Role::Speaker, Mode::Holder);
    net.settle(40_010);
    net.link(0, 1);
    net.settle(41_000);
    assert_eq!(net.holders(Role::Speaker), [id(2)]);
    assert_eq!(net.mode(0, Role::Speaker), Mode::Shared);
    assert!(net.agree());
}

#[test]
fn a_whole_group_merge_restarts_and_keeps_the_target_groups_exit() {
    // F6 / user B: PC 2 was the exit of its former group; it joins the group
    // of 1 and 3, whose exit (1) stays. PC 2 falls back to forward.
    let mut net = Net::new(&[
        (1, DeviceKind::Desktop, false),
        (2, DeviceKind::Desktop, true),
        (3, DeviceKind::Desktop, false),
    ]);
    net.link(0, 2);
    net.settle(10);
    net.select(0, 50, 1, Role::Speaker, Mode::Holder);
    net.settle(60);
    // Joining another group is a new start: its configuration becomes old.
    net.nodes[1].engine.restart(200);
    assert!(net.nodes[1].engine.own().speaker_exit);
    net.link(0, 1);
    net.link(1, 2);
    net.settle(210);
    assert_eq!(net.holders(Role::Speaker), [id(1)]);
    assert_eq!(net.mode(1, Role::Speaker), Mode::Shared);
}

#[test]
fn an_unjoined_device_changes_only_itself() {
    let mut engine = Engine::new(id(ME), DeviceKind::Desktop, own(ME, true, false, false, false), 0, 10);
    assert_eq!(
        engine.select(20, id(OTHER), Role::Speaker, Mode::Holder),
        Err(ControlFailure::Busy)
    );
    // Re-choosing the carried exit makes it a new choice.
    assert_eq!(engine.select(20, id(ME), Role::Speaker, Mode::Holder), Ok(true));
    assert_eq!(engine.select(21, id(ME), Role::Speaker, Mode::Holder), Ok(false));
    assert_eq!(engine.select(22, id(ME), Role::Speaker, Mode::Shared), Ok(true));
    assert_eq!(engine.own().mode(Role::Speaker), Mode::Shared);
}

#[test]
fn the_overview_lists_online_devices_and_only_this_one_while_unjoined() {
    let mut engine = Engine::new(id(ME), DeviceKind::Desktop, own(ME, true, false, true, false), 0, 10);
    let unjoined = engine.overview();
    assert!(!unjoined.joined);
    assert_eq!(unjoined.devices.len(), 1);
    assert_eq!(
        unjoined.mode(id(ME), Role::Speaker),
        Some(Mode::Holder),
        "its own configuration"
    );
    assert_eq!(unjoined.holder(Role::Speaker), None, "nothing executed yet");
    let mut group = speaker(Some(route(5, OTHER, Some(OTHER))));
    group.devices.insert(
        id(OTHER),
        DeviceModes {
            forward: Some(flag(false, 5, OTHER)),
            receive: Some(flag(true, 5, OTHER)),
        },
    );
    pass(&mut engine, 20, &[peer(OTHER, Some(group))]);
    let overview = engine.overview();
    assert!(overview.joined);
    assert_eq!(overview.holder(Role::Speaker), Some(id(OTHER)));
    assert_eq!(
        overview.mode(id(ME), Role::Speaker),
        Some(Mode::Shared),
        "yielded to forward"
    );
    assert_eq!(overview.mode(id(OTHER), Role::Microphone), Some(Mode::Shared));
    assert_eq!(overview.switch(id(OTHER), Role::Speaker), Some(false));
    assert_eq!(overview.route(Role::Speaker), engine.route(Role::Speaker));
    // An automatic local change of this device's own switch.
    assert!(engine.set_own_switch(30, Role::Speaker, false));
    assert!(!engine.set_own_switch(31, Role::Speaker, false));
    assert_eq!(engine.mode(id(ME), Role::Speaker), Some(Mode::Local));
    assert_eq!(
        engine.route(Role::Speaker).unwrap().target,
        Some(id(OTHER)),
        "registers untouched"
    );
}

#[test]
fn old_configuration_that_fills_a_vacancy_loses_to_any_real_choice() {
    // F6 with an unlucky order: the joining PCs (5, 6) link to each other
    // before the target group; the smaller founds, the other one's old exit
    // fills the vacancy. When the target group (exit chosen for real) is
    // reached, its exit stays.
    let mut net = Net::new(&[
        (5, DeviceKind::Desktop, false),
        (6, DeviceKind::Desktop, true),
        (9, DeviceKind::Desktop, false),
    ]);
    net.nodes[2].engine = Engine::new(id(9), DeviceKind::Desktop, own(9, false, false, false, false), 0, 1);
    net.link(0, 1);
    net.settle(100);
    assert_eq!(net.holders(Role::Speaker), [id(6)], "the vacancy is filled");
    // The target group: device 9 alone chose itself (a real choice).
    let mut target = Engine::new(id(9), DeviceKind::Desktop, own(9, false, false, false, false), 0, 1);
    target.reconcile(Inputs {
        wall_ms: 50,
        linked: &[peer(10, None)],
        receive_ready: true,
    });
    target.select(60, id(9), Role::Speaker, Mode::Holder).unwrap();
    net.nodes[2].engine = target;
    net.link(1, 2);
    net.settle(200);
    assert_eq!(net.holders(Role::Speaker), [id(9)]);
    assert_eq!(net.mode(1, Role::Speaker), Mode::Shared, "the joining PC forwards");
}

/// User 2026-11-11: the conflict check only prevents a second exit. A group
/// without an online exit (a later explicit "no exit", or an offline holder,
/// which is removed from the configuration) is no conflict: the phone's new
/// choice applies, authored at the merge, and the others adopt it.
#[test]
fn a_new_choice_applies_when_the_group_has_no_online_exit() {
    for later_none in [true, false] {
        // PC 1 is the exit; PC 2 and the phone (3) are members.
        let mut net = Net::new(&[
            (1, DeviceKind::Desktop, false),
            (2, DeviceKind::Desktop, false),
            (3, DeviceKind::Mobile, false),
        ]);
        for (a, b) in [(0, 1), (0, 2), (1, 2)] {
            net.link(a, b);
        }
        net.settle(10);
        net.select(0, 100, 1, Role::Speaker, Mode::Holder);
        net.settle(110);
        // The phone restarts away from the group and turns its speaker on at 210.
        net.unlink(0, 2);
        net.unlink(1, 2);
        net.nodes[2].engine = Engine::new(id(3), DeviceKind::Mobile, OwnConfig::local(id(3)), 0, 200);
        net.select(2, 210, 3, Role::Speaker, Mode::Holder);
        if later_none {
            // Later, PC 2 explicitly stops the exit: the group has none.
            net.select(1, 300, 1, Role::Speaker, Mode::Local);
            net.settle(310);
            assert!(net.holders(Role::Speaker).is_empty());
            net.link(0, 2);
            net.link(1, 2);
        } else {
            // The exit (PC 1) goes offline; PC 2 has no exit then.
            net.unlink(0, 1);
            net.settle(310);
            assert_eq!(net.nodes[1].engine.holder(Role::Speaker), None);
            net.link(1, 2);
        }
        net.settle(400);
        assert!(net.nodes[2].engine.merged());
        let exit = net.nodes[2].engine.route(Role::Speaker).unwrap();
        assert_eq!(
            exit.target,
            Some(id(3)),
            "the new choice applies (later none: {later_none})"
        );
        assert!(exit.version.stamp >= at(400), "authored at the merge");
        assert_eq!(net.nodes[1].engine.holder(Role::Speaker), Some(id(3)), "adopted");
        if !later_none {
            // The old exit comes back without a restart: its choice is older,
            // it falls back to forward.
            net.link(0, 1);
            net.link(0, 2);
            net.settle(500);
            assert_eq!(net.holders(Role::Speaker), [id(3)]);
            assert_eq!(net.mode(0, Role::Speaker), Mode::Shared);
        }
        assert!(net.agree() || !later_none);
    }
}

#[test]
fn phone_forward_is_self_enabled_remotely_stoppable_and_exclusive_with_exit() {
    let mut net = Net::new(&[(1, DeviceKind::Desktop, false), (2, DeviceKind::Mobile, false)]);
    net.link(0, 1);
    net.settle(20);
    net.select(1, 30, 2, Role::Speaker, Mode::Shared);
    net.settle(40);
    assert_eq!(net.mode(1, Role::Speaker), Mode::Shared);
    net.select(0, 50, 2, Role::Speaker, Mode::Local);
    net.settle(60);
    assert_eq!(net.mode(1, Role::Speaker), Mode::Local);
    // A PC cannot author ON: the phone withdraws the unauthorized flag.
    net.select(0, 70, 2, Role::Speaker, Mode::Shared);
    net.settle(80);
    assert_eq!(net.mode(1, Role::Speaker), Mode::Local);
    net.select(1, 90, 2, Role::Speaker, Mode::Shared);
    net.select(1, 100, 2, Role::Speaker, Mode::Holder);
    net.settle(110);
    net.select(0, 120, 1, Role::Speaker, Mode::Holder);
    net.settle(130);
    assert_eq!(
        net.mode(1, Role::Speaker),
        Mode::Local,
        "losing exit never resumes phone forwarding"
    );
}
