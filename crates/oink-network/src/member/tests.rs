//! Every decision branch of the shared member logic, without sockets. The
//! desktop and the phone both execute these outputs; node-level scenarios
//! (oink-agent tests `mobile_membership_nodes`, `pairing_nodes`) cover the
//! request gates and full merges end to end.
use super::*;

fn identity(n: u8) -> Arc<Identity> {
    Arc::new(Identity::from_seed([n; 32]))
}

fn boot(identity: &Identity, n: u8) -> Boot {
    Boot {
        device_id: identity.id(),
        nonce: [n; 16],
    }
}

fn member(identity: &Arc<Identity>, dir: &Path) -> Membership {
    let (member, pending) = Membership::start(&dir.join("journal.cbor"), identity.clone(), boot(identity, 1)).unwrap();
    assert!(pending.is_none());
    member
}

/// Two-member group {a, b} created by a.
fn pair_state(a: &Identity, b: &Identity) -> State {
    let state = State::create(a, "a", "test", 1).unwrap();
    state
        .with_roster(&state.roster().add(a, b.id(), "b", "test", 2).unwrap())
        .unwrap()
}

fn facts(peer: Boot, greeted: Anchor, ready: bool) -> LinkFacts {
    LinkFacts {
        boot: peer,
        greeted,
        ready,
        departing: false,
        admitted: ready.then_some(greeted),
    }
}

struct NoLinks;
impl Links for NoLinks {
    fn facts(&self, _: LinkId) -> Option<LinkFacts> {
        None
    }
    fn greeted(&self) -> Vec<(LinkId, LinkFacts)> {
        Vec::new()
    }
}

const LINK: LinkId = LinkId(7);

#[test]
fn a_peer_problem_must_reencode_within_the_local_bound() {
    let pair = PairId {
        initiator_boot: boot(&identity(1), 1),
        random: [2; 16],
    };
    let mut problem = pairing::problem(dto::PairCode::Declined, dto::PairStage::Request).into_raw();
    problem.insert(5, wire::pair(pair).write());
    validate_problem(&dto::PairProblem::from_raw(problem.clone()).unwrap()).unwrap();
    let mut winner = wire::pair(pair).into_raw();
    winner.insert(99, RawValue::Bytes(vec![1; 9000]));
    problem.insert(5, RawValue::Map(winner));
    assert!(validate_problem(&dto::PairProblem::from_raw(problem).unwrap()).is_err());
}

#[test]
fn an_unhandled_message_gets_a_typed_error_on_its_own_c0() {
    let message = Message::ProtocolError(
        dto::ProtocolError::from_raw([(1, 6u16.write()), (2, dto::ErrorScope::Stream.write())].into()).unwrap(),
    );
    let Out::Send { link, message: reply } = unhandled(LINK, &message).unwrap() else {
        panic!()
    };
    assert_eq!(link, LINK);
    let Message::ProtocolError(error) = reply else { panic!() };
    assert_eq!(u64::from(error.code().unwrap()), v2::ErrorCode::UnknownMessage.number());
    assert_eq!(error.scope().unwrap(), dto::ErrorScope::Message);
    assert_eq!(error.offending_type().unwrap(), Some(message.type_id()));
}

#[test]
fn merge_failures_map_to_the_same_pairing_reasons_everywhere() {
    let wire = |code| pair_merge::Error::Wire(v2::Error::from(code));
    for (error, code) in [
        (
            pair_merge::Error::Journal(pair_journal::Error::Io(std::io::Error::other("disk"))),
            dto::PairCode::LocalStorageFailed,
        ),
        (wire(v2::ErrorCode::UnsupportedValue), dto::PairCode::UnsupportedLayout),
        (wire(v2::ErrorCode::LimitExceeded), dto::PairCode::MergeTooLarge),
        (
            wire(v2::ErrorCode::AuthenticationFailed),
            dto::PairCode::AuthenticationFailed,
        ),
        (wire(v2::ErrorCode::StateConflict), dto::PairCode::StateChanged),
        (
            pair_merge::Error::Journal(pair_journal::Error::Conflict),
            dto::PairCode::MergeConflict,
        ),
    ] {
        assert_eq!(merge_problem(&error).code().unwrap(), code, "{error}");
    }
}

#[test]
fn greeting_decides_admission_lookup_or_refusal() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b, c) = (identity(1), identity(2), identity(3));
    let mut m = member(&a, dir.path());
    let group = pair_state(&a, &b);
    let anchor = group.roster().anchor();
    let other = State::create(&b, "b", "test", 3).unwrap().roster().anchor();
    let now = Instant::now();

    // Same root, member, we dialed: admission.
    let outs = m
        .greeted(LINK, &facts(boot(&b, 2), anchor, false), true, &group, now)
        .unwrap();
    assert!(matches!(outs[..], [Out::BeginAdmission { link: LINK }]));
    // Same root, member, it dialed: it sends the Request.
    assert!(
        m.greeted(LINK, &facts(boot(&b, 2), anchor, false), false, &group, now)
            .unwrap()
            .is_empty()
    );
    // Same root, never a member: refused.
    assert!(
        m.greeted(LINK, &facts(boot(&c, 3), anchor, false), true, &group, now)
            .is_err()
    );
    // Same root, a member we removed: wait for its Request.
    let removed = group
        .with_roster(&group.roster().remove(&a, b.id(), None, 3).unwrap())
        .unwrap();
    let outs = m
        .greeted(LINK, &facts(boot(&b, 2), anchor, false), true, &removed, now)
        .unwrap();
    assert!(matches!(outs[..], [Out::AwaitRemoved { link: LINK }]));
    // Another root, member: look up the transition out of our root.
    let outs = m
        .greeted(LINK, &facts(boot(&b, 2), other, false), true, &group, now)
        .unwrap();
    assert!(outs.iter().any(|o| matches!(o, Out::OldMemberLane { .. })));
    assert!(outs.iter().any(|o| matches!(o, Out::ArmAdmission { .. })));
    assert!(outs.iter().any(|o| matches!(
        o,
        Out::Migration(Migration {
            phase: MigrationPhase::ProofWaiting,
            problem: None,
            ..
        })
    )));
    assert!(outs.iter().any(|o| matches!(
        o,
        Out::Send {
            message: Message::ConfigMergeLookup(_),
            ..
        }
    )));
    assert!(m.looking_up(LINK));
    // The member answers it has no proof: the lookup ends, nothing applied.
    let ended = Message::ConfigPairEnded(
        dto::PairEnded::from_raw(
            [
                (
                    2,
                    pairing::problem(dto::PairCode::MergeProofUnavailable, dto::PairStage::Adopt).write(),
                ),
                (4, group_merge::group_ref(anchor).write()),
            ]
            .into(),
        )
        .unwrap(),
    );
    let outs = m
        .recovery_message(LINK, &facts(boot(&b, 2), other, false), ended, &group, now)
        .unwrap();
    assert!(!m.looking_up(LINK));
    assert!(matches!(
        outs[..],
        [Out::Migration(Migration {
            phase: MigrationPhase::ProofWaiting,
            problem: Some(_),
            ..
        })]
    ));
    // Another root, not a member: a pairing candidate, nothing to do.
    let stranger = State::create(&c, "c", "test", 1).unwrap();
    assert!(
        m.greeted(LinkId(8), &facts(boot(&b, 2), other, false), true, &stranger, now)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn admission_request_accepts_answers_a_removed_member_or_revokes() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b, c) = (identity(1), identity(2), identity(3));
    let mut ma = member(&a, dir.path());
    let group = pair_state(&a, &b);
    let anchor = group.roster().anchor();
    let request = |state: &State, from: &Identity| {
        let Message::ConfigRequest(request) = admission::request(state, from.id(), 0).unwrap() else {
            panic!()
        };
        request
    };
    // An ordinary member: commit the union, accept, reply.
    let outs = ma
        .request(
            LINK,
            &facts(boot(&b, 2), anchor, false),
            &request(&group, &b),
            &group,
            1,
        )
        .unwrap();
    assert!(matches!(
        outs[..],
        [
            Out::Commit(_),
            Out::Accepted { link: LINK, .. },
            Out::Send {
                message: Message::ConfigReply(_),
                ..
            }
        ]
    ));
    // A member we removed: our signed state answers, the C0 is retired.
    let removed = group
        .with_roster(&group.roster().remove(&a, b.id(), None, 3).unwrap())
        .unwrap();
    let outs = ma
        .request(
            LINK,
            &facts(boot(&b, 2), anchor, false),
            &request(&group, &b),
            &removed,
            1,
        )
        .unwrap();
    assert!(matches!(
        outs[..],
        [
            Out::Send {
                message: Message::ConfigReply(_),
                ..
            },
            Out::RetireRemoved { link: LINK }
        ]
    ));
    // Never a member: violation.
    assert!(
        ma.request(
            LINK,
            &facts(boot(&c, 3), anchor, false),
            &request(&group, &b),
            &group,
            1
        )
        .is_err()
    );
    // The requester's signed state removed us: revocation.
    let mut mb = member(&b, &dir.path().join("b"));
    let without_b = group
        .with_roster(&group.roster().remove(&a, b.id(), None, 3).unwrap())
        .unwrap();
    let outs = mb
        .request(
            LINK,
            &facts(boot(&a, 1), anchor, false),
            &request(&without_b, &a),
            &group,
            1,
        )
        .unwrap();
    assert!(matches!(&outs[..], [Out::Revoked(state)] if !state.roster().contains(b.id())));
}

#[test]
fn admission_reply_admits_or_revokes() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (identity(1), identity(2));
    let mut mb = member(&b, dir.path());
    let group = pair_state(&a, &b);
    let anchor = group.roster().anchor();
    let reply = |state: &State| {
        let Message::ConfigReply(reply) = admission::reply(state, a.id(), 0).unwrap() else {
            panic!()
        };
        reply
    };
    let outs = mb
        .reply(LINK, &facts(boot(&a, 1), anchor, false), &group, &reply(&group), &group)
        .unwrap();
    assert!(matches!(
        outs[..],
        [
            Out::Commit(_),
            Out::HalfOpen { link: LINK },
            Out::Send {
                message: Message::ConfigDone(_),
                ..
            },
            Out::Ready { link: LINK },
            Out::SendStateIfDiffers { link: LINK, .. }
        ]
    ));
    let without_b = group
        .with_roster(&group.roster().remove(&a, b.id(), None, 3).unwrap())
        .unwrap();
    let outs = mb
        .reply(
            LINK,
            &facts(boot(&a, 1), anchor, false),
            &group,
            &reply(&without_b),
            &group,
        )
        .unwrap();
    assert!(matches!(&outs[..], [Out::Revoked(state)] if !state.roster().contains(b.id())));
    let Message::ConfigDone(done) = admission::done(&group).unwrap() else {
        panic!()
    };
    assert!(matches!(
        mb.done(LINK, &group, &done).unwrap()[..],
        [Out::Ready { link: LINK }]
    ));
}

#[test]
fn a_members_state_update_commits_closes_removed_members_or_revokes() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b, c) = (identity(1), identity(2), identity(3));
    let mut ma = member(&a, dir.path());
    let group = pair_state(&a, &b);
    let three = group
        .with_roster(&group.roster().add(&a, c.id(), "c", "test", 3).unwrap())
        .unwrap();
    let without_c = three
        .with_roster(&three.roster().remove(&b, c.id(), None, 4).unwrap())
        .unwrap();
    let outs = ma.state_update(LINK, b.id(), without_c.wire(), &three).unwrap();
    assert!(matches!(&outs[..], [Out::Commit(_), Out::CloseMembers { devices, .. }] if devices == &vec![c.id()]));
    let without_a = three
        .with_roster(&three.roster().remove(&b, a.id(), None, 4).unwrap())
        .unwrap();
    let outs = ma.state_update(LINK, b.id(), without_a.wire(), &three).unwrap();
    assert!(matches!(&outs[..], [Out::Revoked(_)]));
    // An identical update only commits (the owner skips an unchanged state).
    let outs = ma.state_update(LINK, b.id(), three.wire(), &three).unwrap();
    assert!(matches!(outs[..], [Out::Commit(_)]));
}

#[test]
fn retired_lanes_drain_only_the_root_they_were_admitted_under() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (identity(1), identity(2));
    let m = member(&a, dir.path());
    let old = pair_state(&a, &b);
    let new = State::create(&a, "a", "test", 9).unwrap();
    let new = new
        .with_roster(&new.roster().add(&a, b.id(), "b", "test", 10).unwrap())
        .unwrap();
    let mut lane = facts(boot(&b, 2), old.roster().anchor(), false);
    lane.admitted = Some(old.roster().anchor());
    assert_eq!(
        m.retired(&lane, &new, old.roster().anchor().group.0),
        Some(old.roster().anchor())
    );
    assert_eq!(m.retired(&lane, &new, new.roster().anchor().group.0), None);
    lane.ready = true;
    assert_eq!(m.retired(&lane, &new, old.roster().anchor().group.0), None);
}

#[test]
fn lookup_answers_proof_unavailable_or_resumes_admission_for_the_shared_root() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (identity(1), identity(2));
    let mut m = member(&a, dir.path());
    let group = pair_state(&a, &b);
    let anchor = group.roster().anchor();
    let lookup = |source: Anchor| {
        Message::ConfigMergeLookup(
            dto::MergeLookup::from_raw(
                [
                    (1, RawValue::Bytes(source.group.0.to_vec())),
                    (2, RawValue::Bytes(source.root.to_vec())),
                    (3, wire::device(b.id()).write()),
                ]
                .into(),
            )
            .unwrap(),
        )
    };
    let now = Instant::now();
    let outs = m
        .recovery_message(LINK, &facts(boot(&b, 2), anchor, false), lookup(anchor), &group, now)
        .unwrap();
    assert!(matches!(outs[..], [Out::BeginAdmission { link: LINK }]));
    let other = State::create(&b, "b", "test", 5).unwrap().roster().anchor();
    let outs = m
        .recovery_message(LINK, &facts(boot(&b, 2), anchor, false), lookup(other), &group, now)
        .unwrap();
    let [
        Out::Send {
            message: Message::ConfigPairEnded(ended),
            ..
        },
    ] = &outs[..]
    else {
        panic!("{outs:?}")
    };
    assert_eq!(
        ended.problem().unwrap().code().unwrap(),
        dto::PairCode::MergeProofUnavailable
    );
    // A lookup in someone else's name is refused.
    let mut forged = lookup(anchor).raw().clone();
    forged.insert(3, wire::device(a.id()).write());
    let forged = Message::ConfigMergeLookup(dto::MergeLookup::from_raw(forged).unwrap());
    assert!(
        m.recovery_message(LINK, &facts(boot(&b, 2), anchor, false), forged, &group, now)
            .is_err()
    );
}

#[test]
fn decision_queries_without_a_shared_decision_and_stray_receipts_are_violations() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (identity(1), identity(2));
    let mut m = member(&a, dir.path());
    let group = pair_state(&a, &b);
    let pair = PairId {
        initiator_boot: boot(&b, 2),
        random: [3; 16],
    };
    let query = Message::ConfigPairDecisionQuery(
        dto::PairDecisionQuery::from_raw(
            [
                (1, wire::pair(pair).write()),
                (2, RawValue::Bytes(vec![1; 32])),
                (3, 1u64.write()),
            ]
            .into(),
        )
        .unwrap(),
    );
    let lane = facts(boot(&b, 2), group.roster().anchor(), true);
    assert!(m.recovery_message(LINK, &lane, query, &group, Instant::now()).is_err());
    let receipt = Message::ConfigPairReceipt(
        dto::PairReceipt::from_raw(
            [
                (1, wire::pair(pair).write()),
                (2, RawValue::Bytes(vec![1; 32])),
                (3, RawValue::Bytes(vec![2; 32])),
                (4, false.write()),
            ]
            .into(),
        )
        .unwrap(),
    );
    assert!(
        m.recovery_message(LINK, &lane, receipt, &group, Instant::now())
            .is_err()
    );
}

#[test]
fn edits_are_free_without_a_merge_in_flight_and_removal_is_signed() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (identity(1), identity(2));
    let m = member(&a, dir.path());
    assert!(m.edit_allowed().unwrap());
    let group = pair_state(&a, &b);
    let next = m.removal(&group, b.id(), None, 5).unwrap();
    assert!(!next.roster().contains(b.id()));
    let left = m.removal(&group, a.id(), None, 5).unwrap();
    assert!(!left.roster().contains(a.id()));
}

#[test]
fn cancel_refuses_a_stale_revision_and_an_unknown_pairing() {
    let dir = tempfile::tempdir().unwrap();
    let a = identity(1);
    let mut m = member(&a, dir.path());
    let group = State::create(&a, "a", "test", 1).unwrap();
    let pair = PairId {
        initiator_boot: boot(&a, 1),
        random: [4; 16],
    };
    let problem = m.cancel(pair, 1, &group).unwrap_err();
    assert_eq!(problem.code().unwrap(), dto::PairCode::PairClosed);
}

#[test]
fn a_lost_link_drops_its_lookup_and_staging() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (identity(1), identity(2));
    let mut m = member(&a, dir.path());
    let group = pair_state(&a, &b);
    let other = State::create(&b, "b", "test", 3).unwrap().roster().anchor();
    m.greeted(LINK, &facts(boot(&b, 2), other, false), true, &group, Instant::now())
        .unwrap();
    assert!(m.looking_up(LINK));
    assert!(m.link_lost(LINK, Some(boot(&b, 2)), &group).is_empty());
    assert!(!m.looking_up(LINK));
    assert!(m.projection(&group, &[]).unwrap().is_none());
    assert!(m.tick(Instant::now(), &group, &NoLinks).is_empty());
}

/// M34: a merge effect queued behind a projection waits for the installation,
/// even when ending another pairing (supersede, released permit) re-enters
/// the effect queue first. Otherwise the source admits on its C0 with its old
/// group state and the target refuses the request.
#[test]
fn effects_behind_a_projection_wait_for_its_installation() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (identity(1), identity(2));
    let mut m = member(&a, dir.path());
    let current = State::create(&a, "a", "test", 1).unwrap();
    let pair = PairId {
        initiator_boot: boot(&a, 1),
        random: [3; 16],
    };
    let peer = boot(&b, 2);
    let outs = m.merge_effects(
        vec![pair_merge::Effect::Project, pair_merge::Effect::Admit { pair, peer }],
        &current,
    );
    assert!(matches!(outs.as_slice(), [Out::Project]), "{outs:?}");
    let outs = m.merge_effects(Vec::new(), &current);
    assert!(
        outs.is_empty(),
        "admitted before the projection was installed: {outs:?}"
    );
    let outs = m.resume(&current, &NoLinks).unwrap();
    assert!(
        outs.iter()
            .any(|out| matches!(out, Out::AdmitPeer { peer: admitted } if *admitted == peer)),
        "{outs:?}"
    );
}

#[test]
fn startup_returns_a_committed_projection_until_it_is_replayed() {
    let fixture = crate::legacy_merge::test_fixture::fixture();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.cbor");
    let mut raw = v2::cbor::decode_map(&fixture.journal, usize::MAX).unwrap();
    let target = fixture.bundle.target_state().unwrap();
    raw.insert(
        9,
        RawValue::Map(
            [
                (1, wire::pair(fixture.pair).write()),
                (2, 1u64.write()),
                (3, target.write()),
            ]
            .into(),
        ),
    );
    std::fs::write(&path, v2::cbor::encode_map(&raw, usize::MAX).unwrap()).unwrap();
    let identity = Arc::new(fixture.target);
    let (mut m, pending) = Membership::start(&path, identity.clone(), boot(&identity, 1)).unwrap();
    assert_eq!(pending.as_ref(), Some(&target));
    m.replayed(group_merge::digest(&target).unwrap()).unwrap();
    let (_, pending) = Membership::start(&path, identity.clone(), boot(&identity, 2)).unwrap();
    assert!(pending.is_none());
}

/// M33: a merge committed by 0.3.x (format-1 proof) reaches a source member
/// that upgraded before it learned the merge. The member knows its group by
/// the v2 root of the same roster; the proof is found by that root, verified
/// under its own definitions and installed as its v2 projection.
#[test]
fn a_0_3_merge_proof_is_found_by_its_v2_root_and_adopted_as_its_projection() {
    let fixture = crate::legacy_merge::test_fixture::fixture();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("b.cbor");
    std::fs::write(&path, &fixture.journal).unwrap();
    let a = Arc::new(fixture.source);
    let b = Arc::new(fixture.target);
    let (mut mb, _) = Membership::start(&path, b.clone(), boot(&b, 2)).unwrap();
    let mut ma = member(&a, &dir.path().join("a"));
    let (source, target) = crate::legacy_merge::anchors(&fixture.bundle).unwrap();
    let a_state = State::verify(
        crate::legacy_merge::project_state(&fixture.source_state).unwrap(),
        source,
    )
    .unwrap();
    let b_state = State::verify(
        crate::legacy_merge::project_state(&fixture.bundle.target_state().unwrap()).unwrap(),
        target,
    )
    .unwrap();
    let now = Instant::now();
    let a_lane = facts(boot(&b, 2), target, false);
    // In the field a fellow source member that adopted the merge answers;
    // the fixture's only other journal is the target's, which serves it alike.
    ma.inject_lookup(LINK, source, now + Duration::from_secs(10));
    let request = Message::ConfigMergeLookup(
        dto::MergeLookup::from_raw(
            [
                (1, RawValue::Bytes(source.group.0.to_vec())),
                (2, RawValue::Bytes(source.root.to_vec())),
                (3, wire::device(a.id()).write()),
            ]
            .into(),
        )
        .unwrap(),
    );
    let answers = mb
        .recovery_message(LINK, &facts(boot(&a, 1), source, false), request, &b_state, now)
        .unwrap();
    assert!(
        matches!(
            answers.first(),
            Some(Out::Send {
                message: Message::ConfigMergeTransfer(_),
                ..
            })
        ),
        "{answers:?}"
    );
    let mut project = false;
    for answer in answers {
        let Out::Send { message, .. } = answer else { panic!() };
        let outs = ma.recovery_message(LINK, &a_lane, message, &a_state, now).unwrap();
        project |= outs.iter().any(|out| matches!(out, Out::Project));
    }
    assert!(project, "the proof was adopted");
    let projection = ma.projection(&a_state, &[]).unwrap().unwrap();
    assert_eq!(projection.candidate.roster().anchor(), target);
    assert!(projection.candidate.roster().contains(a.id()) && projection.candidate.roster().contains(b.id()));
    let outs = ma
        .projected(projection.digest, &projection.candidate, &NoLinks)
        .unwrap();
    assert!(outs.is_empty());
}
