//! Real C0 adversarial queues, owner facts, and offline merge recovery.
use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn joining_group_admits_an_existing_unpaired_member_link() {
    let nodes = start(fresh(3)).await;
    let first = request(&nodes[0], &nodes[1], [21; 16]).await;
    let code = accept(&nodes[0], &nodes[1], &first).await;
    submit(&nodes[1], &first, 1, code);
    complete(&nodes[0], &nodes[1], &first).await;
    assert!(until(|| ready(&nodes[..2])).await);

    // A manual lookup can leave a greeted C0 between different groups. It has
    // no membership authority and must not become ready before the actual join.
    let _ = query(&nodes[2], &nodes[1], [22; 16]).await;
    let peer = nodes[1].identity.id();
    let waiting = nodes[2].links.links().by_peer(peer).unwrap().link;
    assert!(!nodes[2].links.links().ready(peer));
    assert!(!nodes[2].group.members().members.iter().any(|m| m.device == peer));
    assert!(
        nodes[2]
            .group
            .call(move |owner| Ok(owner.sessions[&waiting].peer.is_some()))
            .unwrap()
    );
    // Keep this candidate alive beyond the mesh assertion on both owners. The
    // independent inactivity fallback must not conceal missing reconciliation.
    for (local, remote) in [(2, 1), (1, 2)] {
        let peer = nodes[remote].identity.id();
        nodes[local]
            .group
            .call(move |owner| {
                let id = owner.selected_session(peer).unwrap().0;
                owner.sessions.get_mut(&id).unwrap().deadline = Some(Instant::now() + Duration::from_secs(60));
                Ok(())
            })
            .unwrap();
    }

    // Join through the other member. The already greeted C0 now belongs to a
    // known member, but neither a new greeting nor a new dial will arrive on it.
    let join = request(&nodes[2], &nodes[0], [23; 16]).await;
    let code = accept(&nodes[2], &nodes[0], &join).await;
    submit(&nodes[0], &join, 1, code);
    complete(&nodes[2], &nodes[0], &join).await;
    let mesh = until(|| ready(&nodes)).await;
    stop(&nodes).await;
    assert!(
        mesh,
        "a greeted nonmember C0 must resume admission when the peer becomes a member"
    );
    // The other owner can learn membership after the first Request arrives.
    // Its normal unknown-member rejection and redial are valid; pinning the
    // physical LinkId would require relaxing that authorization boundary.
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unadmitted_link_times_out_and_member_redial_recovers() {
    use oink_network::link::{LinkLost, ModuleEvent};
    use oink_protocol::frame::StreamKind;
    let nodes = start(fresh(2)).await;
    let _ = query(&nodes[0], &nodes[1], [24; 16]).await;
    let peer = nodes[1].identity.id();
    let stalled = nodes[0].links.links().by_peer(peer).unwrap().link;
    let deadline = nodes[0]
        .group
        .call(move |owner| Ok(owner.sessions[&stalled].deadline))
        .unwrap();
    assert!(
        deadline.is_some(),
        "a greeted but unadmitted C0 must retain a bounded deadline"
    );
    let mut events = nodes[0].links.register(&[StreamKind::Input]);
    let remote = nodes[1].group.clone();
    let (entered, entering) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel();
    let blocked = std::thread::spawn(move || {
        remote.call(move |_| {
            // Keep the actual C0 reader and QUIC/heartbeat alive while its
            // configuration owner stops answering admission.
            entered.send(()).unwrap();
            released.recv().unwrap();
            Ok(())
        })
    });
    entering.recv().unwrap();
    let stored = same_group(2).remove(0);
    let roster = stored.state.roster().unwrap();
    let anchor = Anchor {
        group: GroupId(stored.state.group().unwrap().0),
        root: oink_network::roster::root_id(&roster).unwrap(),
    };
    let target_state = State::verify(stored.state, anchor).unwrap();
    let local_state = target_state.clone();
    nodes[0].group.call(move |owner| owner.commit(local_state)).unwrap();
    nodes[0]
        .group
        .call(move |owner| {
            assert!(!owner.sessions[&stalled].ready);
            let pending = owner.sessions[&stalled].deadline;
            owner.begin_admission(stalled).map_err(internal)?;
            assert_eq!(
                owner.sessions[&stalled].deadline, pending,
                "repeating the same admission request is not progress"
            );
            // Advance just this real C0's inactivity boundary; avoid a ten-second
            // wall-clock wait or changing the network owner's heartbeat clock.
            owner.sessions.get_mut(&stalled).unwrap().deadline = Some(Instant::now() - Duration::from_millis(1));
            owner.tick();
            assert!(!owner.sessions.contains_key(&stalled));
            Ok(())
        })
        .unwrap();
    let reason = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(ModuleEvent::LinkLost { link, reason, .. }) = events.next().await
                && link == stalled
            {
                break reason;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(reason, LinkLost::Local(Disconnect::HandshakeTimeout));
    release.send(()).unwrap();
    blocked.join().unwrap().unwrap();
    nodes[1].group.call(move |owner| owner.commit(target_state)).unwrap();
    let recovered = until(|| ready(&nodes)).await;
    let replaced = nodes[0]
        .links
        .links()
        .by_peer(peer)
        .is_some_and(|link| link.link != stalled);
    stop(&nodes).await;
    assert!(
        recovered,
        "the normal member dialer must recover after an unadmitted C0 expires"
    );
    assert!(replaced);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pending_human_pairing_keeps_its_own_lifetime_before_admission() {
    let nodes = start(fresh(2)).await;
    let pair = request(&nodes[0], &nodes[1], [25; 16]).await;
    let peer = nodes[1].identity.id();
    let link = nodes[0].links.links().by_peer(peer).unwrap().link;
    nodes[0]
        .group
        .call(move |owner| {
            owner.sessions.get_mut(&link).unwrap().deadline = Some(Instant::now() - Duration::from_millis(1));
            owner.tick();
            assert!(
                owner.sessions.contains_key(&link),
                "waiting for a human is not stalled member admission"
            );
            assert!(
                owner.next_deadline() > Instant::now(),
                "a suspended idle deadline must not spin the owner"
            );
            Ok(())
        })
        .unwrap();
    let code = accept(&nodes[0], &nodes[1], &pair).await;
    submit(&nodes[1], &pair, 1, code);
    complete(&nodes[0], &nodes[1], &pair).await;
    assert!(until(|| ready(&nodes)).await);
    stop(&nodes).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prepared_pairing_waits_for_a_replacement_greeting_after_selection_race() {
    let nodes = start(fresh(2)).await;
    let client = [27; 16];
    let found = query(&nodes[0], &nodes[1], client).await;
    let peer = nodes[1].links.boot();
    nodes[0]
        .group
        .call(move |owner| {
            let link = owner.peer_link(peer).unwrap();
            owner.links.disconnect(link, Disconnect::HandshakeFailed);
            // Reproduce the network/owner ordering at the reported failure:
            // N0 has already stopped selecting the prepared C0, but its Lost
            // event has not yet removed the stale session from this owner.
            oink_network::link::testing::flush_commands(&owner.links);
            assert!(
                owner
                    .links
                    .links()
                    .by_peer(peer.device_id)
                    .is_none_or(|selected| selected.link != link)
            );
            assert!(owner.sessions.contains_key(&link));
            let result = owner.start_pairing(
                client,
                "selection-race".into(),
                api::PairTarget::PreparedQuery {
                    query_id: found.query_id,
                    generation: found.generation,
                },
            )?;
            assert!(matches!(result, api::PairReply::Value(api::Started::Started { .. })));
            assert_eq!(owner.connecting.len(), 1);
            Ok(())
        })
        .unwrap();
    stop(&nodes).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn advancing_proof_chunks_extend_admission_but_replays_do_not() {
    let nodes = start(fresh(2)).await;
    let _ = query(&nodes[0], &nodes[1], [26; 16]).await;
    let peer = nodes[1].identity.id();
    let link = nodes[0].links.links().by_peer(peer).unwrap().link;
    nodes[0]
        .group
        .call(move |owner| {
            let anchor = owner.state.roster().anchor();
            let context = dto::ObjectContext::from_raw(
                [
                    (1, dto::ObjectContextKind::GroupRef.write()),
                    (
                        2,
                        RawValue::Map([(1, oink_network::group_merge::group_ref(anchor).write())].into()),
                    ),
                ]
                .into(),
            )
            .unwrap();
            let (reference, frames) = oink_network::pair_objects::transfer(
                context.clone(),
                dto::ObjectKind::Transfer,
                &[(1, RawValue::Bytes(vec![7; 32768]))].into(),
            )
            .unwrap();
            owner
                .member
                .inject_lookup(link, anchor, Instant::now() + ADMISSION_TIMEOUT);
            let header = v2::frame::encode(
                &Message::ConfigMergeTransfer(dto::MergeTransfer::from_raw([(1, reference.write())].into()).unwrap()),
                v2::limits::CONFIG_FRAME,
            )
            .unwrap();
            owner.control(link, &header).map_err(internal)?;
            let begin = v2::frame::encode(&frames[0], v2::limits::CONFIG_FRAME).unwrap();
            owner.control(link, &begin).map_err(internal)?;
            let before = Instant::now();
            owner.sessions.get_mut(&link).unwrap().deadline = Some(before - Duration::from_millis(1));
            let chunk = v2::frame::encode(&frames[1], v2::limits::CONFIG_FRAME).unwrap();
            owner.control(link, &chunk).map_err(internal)?;
            let renewed = owner.sessions[&link].deadline.unwrap();
            assert!(renewed >= before + ADMISSION_TIMEOUT);
            owner.tick();
            assert!(
                owner.sessions.contains_key(&link),
                "real proof progress must keep admission alive"
            );
            assert!(owner.control(link, &chunk).is_err(), "an old chunk is not progress");
            assert_eq!(owner.sessions[&link].deadline, Some(renewed));
            owner.control(link, &header).map_err(internal)?;
            assert_eq!(
                owner.sessions[&link].deadline,
                Some(renewed),
                "a repeated proof header is not progress"
            );
            Ok(())
        })
        .unwrap();
    stop(&nodes).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unreceipted_commit_after_peer_reset_keeps_the_link_available_for_pairing() {
    let mut nodes = start(fresh(2)).await;
    let first = request(&nodes[0], &nodes[1], [6; 16]).await;
    let code = accept(&nodes[0], &nodes[1], &first).await;
    submit(&nodes[1], &first, 1, code);
    complete(&nodes[0], &nodes[1], &first).await;
    assert!(until(|| ready(&nodes)).await);
    stop(&nodes).await;
    // Model a crash before the source persisted its receipt. The peer then
    // explicitly resets its unreadable group; its historical proofs survive.
    let path = nodes[0].directory.path().join(oink_network::pair_journal::FILE);
    let mut journal = v2::cbor::decode_map(&std::fs::read(&path).unwrap(), 64 * 1024 * 1024).unwrap();
    let RawValue::Array(rows) = journal.get_mut(&3).unwrap() else {
        panic!("transactions")
    };
    for row in rows {
        let RawValue::Map(row) = row else { panic!("transaction") };
        row.insert(9, false.write());
    }
    std::fs::write(&path, v2::cbor::encode_map(&journal, 64 * 1024 * 1024).unwrap()).unwrap();
    reset_group_state(nodes[1].directory.path(), &nodes[1].identity).unwrap();
    restart(&mut nodes[0]).await;
    restart(&mut nodes[1]).await;
    let target = nodes[0].group.applied().group;
    assert_ne!(nodes[1].group.applied().group, target);
    let _ = query(&nodes[1], &nodes[0], [7; 16]).await;
    let old_link = nodes[0].links.links().by_peer(nodes[1].identity.id()).unwrap().link;
    assert!(
        until(|| nodes[0]
            .group
            .call(move |owner| Ok(owner
                .sessions
                .get(&old_link)
                .is_some_and(|session| session.sent.is_none())
                && owner
                    .shared
                    .migration
                    .load()
                    .as_ref()
                    .as_ref()
                    .is_some_and(|m| m.problem.is_some())
                && !owner.member.expects_any_proof()))
            .unwrap())
        .await,
        "{:?}",
        nodes[0]
            .group
            .call(|o| Ok((
                o.sessions
                    .iter()
                    .map(|(id, s)| (
                        *id,
                        s.ready,
                        s.sent.is_some(),
                        s.peer.as_ref().and_then(|p| p.anchor().ok())
                    ))
                    .collect::<Vec<_>>(),
                o.member.lookups_pending(),
                o.member.expects_any_proof(),
                o.shared.migration.load()
            )))
            .unwrap()
    );
    let second = request(&nodes[1], &nodes[0], [8; 16]).await;
    let code = accept(&nodes[1], &nodes[0], &second).await;
    submit(&nodes[0], &second, 1, code);
    complete(&nodes[1], &nodes[0], &second).await;
    assert!(until(|| ready(&nodes)).await);
    assert_eq!(nodes[1].group.applied().group, target);
    assert_eq!(
        nodes[0].links.links().by_peer(nodes[1].identity.id()).unwrap().link,
        old_link
    );
    stop(&nodes).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn facts_drain_barrier_requires_a_new_publication() {
    // No peer/RTT changes can accidentally advance these retained Facts.
    // A receiver may already hold this sequence while another copy is queued.
    let nodes = start(fresh(1)).await;
    let (observed, marker) = nodes[0]
        .group
        .call(|owner| {
            owner.publish_facts()?;
            let observed = owner.local.value().fact_sequence().map_err(internal)?;
            let marker = publish_facts_barrier(owner)?;
            Ok((observed, marker))
        })
        .unwrap();
    stop(&nodes).await;
    assert!(
        marker > observed,
        "an already observed Facts sequence cannot acknowledge queued frames: observed={observed}, marker={marker}"
    );
}

fn publish_facts_barrier(owner: &mut Owner) -> ApiResult<u64> {
    // Unchanged Facts retain their sequence. A receiver can already have that
    // value while another copy is still queued on C0. Give the drain marker a
    // fresh sequence before publishing so an earlier observation cannot pass.
    let next = owner
        .local
        .value()
        .fact_sequence()
        .map_err(internal)?
        .checked_add(1)
        .unwrap();
    owner
        .local
        .edit(|raw| {
            raw.insert(2, next.write());
            Ok(())
        })
        .map_err(internal)?;
    owner.publish_facts()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn arrangement_result_fences_only_facts_sent_before_its_ack() {
    let nodes = start(same_group(2)).await;
    assert!(until(|| ready(&nodes)).await);
    for node in &nodes {
        node.group.hold_periodic_facts();
    }
    let origin = nodes[0].identity.id();
    let peer = nodes[1].identity.id();
    let published = nodes[0].group.call(publish_facts_barrier).unwrap();
    assert!(
        until(|| nodes[1].group.members().members.iter().any(|m| {
            m.device == origin
                && m.facts
                    .as_ref()
                    .is_some_and(|f| f.fact_sequence().is_ok_and(|s| s >= published))
        }))
        .await
    );
    // A local projection can observe a newer RTT/route and advance Facts without
    // broadcasting. Force that gap; no periodic send may accidentally repair it.
    nodes[0]
        .group
        .call(|owner| {
            let next = owner.local.value().fact_sequence().map_err(internal)? + 1;
            owner
                .local
                .edit(|raw| {
                    raw.insert(2, next.write());
                    Ok(())
                })
                .map_err(internal)?;
            owner.publish()
        })
        .unwrap();
    let mut changes = nodes[1].group.subscribe();
    let result = dto::LayoutArrangementResult::from_raw(
        [
            (1, RawValue::Bytes(vec![19; 16])),
            (2, wire::boot(nodes[0].links.boot()).write()),
            (3, crate::layout_profiles::native::FAILED.write()),
            (5, RawValue::from("os_apply_failed")),
            (6, RawValue::Array(Vec::new())),
        ]
        .into(),
    )
    .unwrap();
    let result = nodes[0].group.fence_arrangement_result(result).unwrap();
    let fenced = result.fact_sequence().unwrap().unwrap();
    assert!(fenced > published);
    nodes[0]
        .group
        .send_device_control(peer, Message::LayoutArrangementResult(result))
        .unwrap();
    // Real C0 delivery of the ack is the barrier. Periodic publication is held
    // and this fixture sends no more Facts; no sleep can repair the missing send.
    let received = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(ConfigChange::DeviceControl {
                message: Message::LayoutArrangementResult(_),
                ..
            }) = changes.next().await
            {
                return nodes[1]
                    .group
                    .call(move |owner| Ok(owner.facts.get(&origin).unwrap().fact_sequence().unwrap()))
                    .unwrap();
            }
        }
    })
    .await
    .unwrap();
    stop(&nodes).await;
    assert!(
        received >= fenced,
        "ack fenced unpublished Facts: received={received}, fenced={fenced}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admitted_member_can_repeat_signed_history_while_readmission_is_pending() {
    repeat_signed_history(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pending_history_fixture_preserves_queued_current_state() {
    repeat_signed_history(true).await;
}

async fn repeat_signed_history(queue_current_state: bool) {
    let nodes = start(fresh(3)).await;
    let first = request(&nodes[0], &nodes[1], [8; 16]).await;
    let code = accept(&nodes[0], &nodes[1], &first).await;
    submit(&nodes[1], &first, 1, code);
    complete(&nodes[0], &nodes[1], &first).await;
    assert!(until(|| ready(&nodes[..2])).await);
    let old = nodes[0].group.call(|owner| Ok(owner.state.roster().anchor())).unwrap();
    let id = request(&nodes[0], &nodes[2], [9; 16]).await;
    let code = accept(&nodes[0], &nodes[2], &id).await;
    submit(&nodes[2], &id, 1, code);
    complete(&nodes[0], &nodes[2], &id).await;
    // Only the previously admitted source/receiver lane is exercised below.
    // Their adoption must finish; the unrelated third-to-receiver dial need
    // not finish its own handshake/retry before we inject that lane's state.
    assert!(
        until(|| ready(&nodes[..2])
            && nodes[1].group.applied().group == nodes[0].group.applied().group
            && nodes[1].group.members().members.len() == 3)
        .await,
        "source/receiver admission after merge: {:?}",
        nodes
            .iter()
            .map(|n| {
                (
                    n.group.members(),
                    n.links.links(),
                    n.group
                        .call(|owner| {
                            Ok(owner
                                .sessions
                                .iter()
                                .map(|(link, s)| (*link, s.peer_boot, s.ready, s.sent.is_some(), s.accepted.is_some()))
                                .collect::<Vec<_>>())
                        })
                        .unwrap(),
                )
            })
            .collect::<Vec<_>>()
    );
    let target = nodes[0].group.applied().group;
    let peer = nodes[1].identity.id();
    let bundle = nodes[0]
        .group
        .call(move |owner| Ok(owner.member.journal.lookup(peer, old, None).unwrap().unwrap()))
        .unwrap();
    let context = dto::ObjectContext::from_raw(
        [
            (1, dto::ObjectContextKind::GroupRef.write()),
            (
                2,
                RawValue::Map([(1, oink_network::group_merge::group_ref(old).write())].into()),
            ),
        ]
        .into(),
    )
    .unwrap();
    let (reference, messages) =
        oink_network::pair_objects::transfer(context, dto::ObjectKind::Transfer, bundle.raw()).unwrap();
    let digest = reference.digest().unwrap().0;
    let origin = nodes[0].identity.id();
    let header = Message::ConfigMergeTransfer(dto::MergeTransfer::from_raw([(1, reference.write())].into()).unwrap());
    let frames = std::iter::once(header)
        .chain(messages)
        .map(|message| v2::frame::encode(&message, v2::limits::CONFIG_FRAME).unwrap())
        .collect::<Vec<_>>();
    let queued = queue_current_state.then(|| {
        nodes[0]
            .group
            .call(|owner| {
                v2::frame::encode(
                    &Message::ConfigState(owner.state.wire().clone()),
                    v2::limits::CONFIG_FRAME,
                )
                .map_err(internal)
            })
            .unwrap()
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    let (incoming, observation, processed) = loop {
        let frames = frames.clone();
        let queued = queued.clone();
        let attempt = nodes[1]
            .group
            .call(move |owner| {
                // Select and inject on one serial owner turn. The public link
                // snapshot can retire a selected C0 between the ready check and
                // this turn when additional shared registers are published.
                let selected = owner.links.links().by_peer(origin).map(|link| link.link);
                let incoming = owner.sessions.iter().find_map(|(id, session)| {
                    (Some(*id) == selected
                        && session.peer_boot.device_id == origin
                        && session.ready
                        && !session.departing)
                        .then_some(*id)
                });
                let Some(incoming) = incoming else {
                    return Ok(None);
                };
                let session = owner
                    .sessions
                    .get_mut(&incoming)
                    .ok_or_else(|| invalid("fixture C0 missing"))?;
                let admitted = session.admitted_anchor.is_some();
                let was_ready = session.ready;
                // This is an injected owner state, not a real readmission. Keep
                // the proof and its observations in one owner turn: live current-
                // group State/Facts would correctly reject this artificial phase.
                // Freezing periodic Facts or observing a prior marker cannot fence
                // later admission/state traffic from the other live peers.
                session.ready = false;
                let replay = (|| {
                    owner
                        .control(incoming, &frames[0])
                        .map_err(|e| internal(format!("history header: {e}")))?;
                    let expected = owner.member.expects_proof(incoming, digest);
                    for frame in &frames[1..] {
                        owner.control(incoming, frame).map_err(|e| {
                            internal(format!(
                                "history frame {:04x}: {e}",
                                v2::frame::OpaqueFrame::parse_exact(frame, v2::limits::CONFIG_FRAME)
                                    .unwrap()
                                    .header
                                    .type_id
                            ))
                        })?;
                    }
                    let session = owner
                        .sessions
                        .get(&incoming)
                        .ok_or_else(|| invalid("proof retired fixture C0"))?;
                    Ok((
                        admitted,
                        was_ready,
                        expected,
                        owner.member.expects_proof(incoming, digest),
                        session.ready,
                        owner.state.roster().anchor().group,
                    ))
                })();
                // Force a current-state frame to arrive after proof processing but
                // before inspecting the synthetic phase. The actor must not consume
                // it until the fixture has restored the live admission state.
                let processed = queued.map(|bytes| {
                    let (ack, processed) = tokio::sync::oneshot::channel();
                    owner
                        .incoming_tx
                        .send(Incoming::Control(incoming, Ok(bytes), ack))
                        .unwrap();
                    processed
                });
                if let Some(session) = owner.sessions.get_mut(&incoming) {
                    session.ready = was_ready;
                }
                replay.map(|observation| Some((incoming, observation, processed)))
            })
            .unwrap();
        if let Some(value) = attempt {
            break value;
        }
        assert!(Instant::now() < deadline, "fixture never observed a ready selected C0");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    // Assertions belong on the test thread; a fixture failure must not panic
    // the ordinary config owner and obscure its cause as NetworkStopped.
    let (admitted, was_ready, expected, still_expected, became_ready, group) = observation;
    assert!(admitted && was_ready);
    assert!(expected, "signed history header was not correlated");
    assert!(!still_expected, "signed history object did not finish");
    assert!(!became_ready, "repeated history must not complete pending admission");
    assert_eq!(group, target);
    if let Some(processed) = processed {
        processed.await.unwrap();
    }
    // Also verify the original live C0 remains usable after queued traffic,
    // through its real TLS reader. A newer Facts sequence acknowledges this
    // fresh marker too because the C0 controls are ordered.
    let origin = nodes[0].identity.id();
    let sequence = nodes[0].group.call(publish_facts_barrier).unwrap();
    assert!(
        until(|| nodes[1]
            .group
            .call(move |owner| Ok(owner
                .facts
                .get(&origin)
                .is_some_and(|facts| facts.fact_sequence().is_ok_and(|seen| seen >= sequence))))
            .unwrap())
        .await
    );
    assert_eq!(
        nodes[1].links.links().by_peer(nodes[0].identity.id()).unwrap().link,
        incoming
    );
    stop(&nodes).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_departure_is_durable_before_waiting_for_new_group_owner_preferences() {
    let nodes = start(same_group(2)).await;
    assert!(until(|| ready(&nodes)).await);
    let original = nodes[0].group.applied().group;
    {
        let _gate = nodes[0].gate.lock().unwrap();
        nodes[0].group.leave_group().unwrap();
        let saved = group_store::load(
            &nodes[0].directory.path().join(group_store::STATE_FILE),
            nodes[0].identity.id(),
        )
        .unwrap()
        .unwrap();
        let state = State::verify(
            saved.state,
            nodes[0].group.call(|owner| Ok(owner.state.roster().anchor())).unwrap(),
        )
        .unwrap();
        assert!(!state.roster().contains(nodes[0].identity.id()));
        assert!(
            nodes[0]
                .group
                .call(|owner| Ok(owner.pending_revocation.is_some()))
                .unwrap()
        );
    }
    assert!(until(|| nodes[0].group.applied().group != original).await);
    assert_eq!(nodes[0].group.applied().members, vec![nodes[0].identity.id()]);
    assert!(until(|| nodes[1].group.applied().members == vec![nodes[1].identity.id()]).await);
    assert_eq!(nodes[1].group.applied().group, original);
    stop(&nodes).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_application_retry_retains_admission_and_accepts_current_facts() {
    let nodes = start(same_group(2)).await;
    assert!(until(|| ready(&nodes)).await);
    let peer = nodes[1].identity.id();
    let link = nodes[0].links.links().by_peer(peer).unwrap().link;
    let before = nodes[0].group.applied().version.clone();
    set_application_failure(&nodes[0], true);
    nodes[1]
        .group
        .edit_settings(Box::new(|settings| settings.walls = !settings.walls))
        .unwrap();
    assert!(until(|| nodes[0].group.call(|owner| Ok(owner.needs_apply)).unwrap()).await);
    let current = nodes[1].group.applied().version.clone();
    assert_ne!(current, before);
    let saved = group_store::load(
        &nodes[0].directory.path().join(group_store::STATE_FILE),
        nodes[0].identity.id(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(saved.layout_register().unwrap().version().unwrap(), current);
    assert_eq!(nodes[0].group.applied().version, before);
    let mut facts = nodes[1].group.local_facts().raw().clone();
    let sequence = nodes[1].group.local_facts().fact_sequence().unwrap() + 1;
    facts.insert(2, sequence.write());
    let facts = dto::DeviceFacts::from_raw(facts).unwrap();
    let reverse = nodes[1].links.links().by_peer(nodes[0].identity.id()).unwrap().link;
    nodes[1]
        .group
        .call(move |owner| owner.send(reverse, Message::ConfigFacts(facts)).map_err(internal))
        .unwrap();
    assert!(
        until(|| nodes[0]
            .group
            .call(move |owner| Ok(owner
                .facts
                .get(&peer)
                .is_some_and(|f| f.fact_sequence() == Ok(sequence))))
            .unwrap())
        .await
    );
    assert_eq!(nodes[0].links.links().by_peer(peer).unwrap().link, link);
    assert_eq!(nodes[0].group.applied().version, before);
    set_application_failure(&nodes[0], false);
    assert!(until(|| nodes[0].group.applied().version == current).await);
    assert_eq!(nodes[0].links.links().by_peer(peer).unwrap().link, link);
    stop(&nodes).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn released_query_rejects_delayed_outbound_probe() {
    let nodes = start(fresh(2)).await;
    let found = query(&nodes[0], &nodes[1], [1; 16]).await;
    let links = nodes[0].links.clone();
    let address = nodes[1].links.local_addr();
    let peer = nodes[1].identity.id();
    let runtime = tokio::runtime::Handle::current();
    nodes[0]
        .group
        .call(move |owner| {
            owner.release_query([1; 16], &found.query_id, found.generation.0);
            // A repeated probe can finish TLS against the existing C0, resolve
            // its query there, and finish N0 only after the last query releases.
            // Deliver that late real transport event while the actor is held.
            let probe = runtime.spawn(async move { links.probe_address(address).await });
            loop {
                let port = &mut owner.port;
                let event = std::thread::scope(|scope| {
                    scope
                        .spawn(move || {
                            tokio::runtime::Builder::new_current_thread()
                                .enable_all()
                                .build()
                                .unwrap()
                                .block_on(async {
                                    tokio::time::timeout(Duration::from_secs(5), port.next())
                                        .await
                                        .expect("replacement probe establishes N0")
                                        .unwrap()
                                })
                        })
                        .join()
                        .unwrap()
                });
                let connected = matches!(&event, ConfigEvent::Connected(event) if event.peer == peer);
                owner.network(event);
                if connected {
                    break;
                }
            }
            probe.abort();
            assert!(
                owner
                    .sessions
                    .values()
                    .all(|session| session.peer_boot.device_id != peer),
                "a released query must not retain its late outgoing C0"
            );
            Ok(())
        })
        .unwrap();
    assert!(until(|| nodes[0].links.links().by_peer(peer).is_none()).await);
    stop(&nodes).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn address_operations_are_scoped_to_the_client_and_release_only_idle_prepared_links() {
    let nodes = start(fresh(2)).await;
    let first = query(&nodes[0], &nodes[1], [1; 16]).await;
    let second = query(&nodes[0], &nodes[1], [2; 16]).await;
    assert_eq!(first.peer.device, second.peer.device);
    nodes[0]
        .group
        .release_pairing_query([1; 16], first.query_id, first.generation.0)
        .unwrap();
    assert!(nodes[0].links.links().by_peer(nodes[1].identity.id()).is_some());
    nodes[0]
        .group
        .release_pairing_query([2; 16], second.query_id, second.generation.0)
        .unwrap();
    assert!(until(|| nodes[0].links.links().by_peer(nodes[1].identity.id()).is_none()).await);
    let op = operation().0;
    let a = nodes[0]
        .group
        .query_pairing_address([3; 16], op.clone(), 1, "bad-address".into(), None)
        .unwrap();
    let b = nodes[0]
        .group
        .query_pairing_address([3; 16], op.clone(), 1, "bad-address".into(), None)
        .unwrap();
    assert_eq!(serde_json::to_value(&a).unwrap(), serde_json::to_value(&b).unwrap());
    let conflict = nodes[0]
        .group
        .query_pairing_address(
            [3; 16],
            op.clone(),
            2,
            "127.0.0.1".into(),
            Some(nodes[1].links.local_addr().port()),
        )
        .unwrap();
    assert!(matches!(conflict, api::PairReply::Problem(_)));
    let other = nodes[0]
        .group
        .query_pairing_address(
            [4; 16],
            op,
            2,
            "127.0.0.1".into(),
            Some(nodes[1].links.local_addr().port()),
        )
        .unwrap();
    assert!(matches!(other, api::PairReply::Value(_)));
    stop(&nodes).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pending_address_query_survives_retirement_before_its_greeting() {
    let nodes = start(same_group(2)).await;
    assert!(until(|| ready(&nodes)).await);
    let peer = nodes[1].links.boot();
    let mut events = nodes[0].group.subscribe();
    let pending_survived = nodes[0]
        .group
        .call(move |owner| {
            let link = owner.peer_link(peer).unwrap();
            // TLS has identified the new probe, but its selected C0/greeting
            // has not arrived. Retire the previous lifetime in this actor turn,
            // before any replacement Connected event can be consumed.
            let deadline = Instant::now() + oink_network::link::ADDRESS_QUERY_BUDGET;
            for (id, boot) in [("pending", None), ("prepared", Some(peer))] {
                owner.queries.insert(
                    id.into(),
                    Query {
                        client: [0; 16],
                        operation: id.into(),
                        generation: 1,
                        address: "127.0.0.1:47470".parse().unwrap(),
                        peer: boot.map(|boot| boot.device_id),
                        boot,
                        deadline: boot.is_none().then_some(deadline),
                        released: false,
                    },
                );
            }
            oink_network::link::testing::set_direct_blocked(&owner.links, peer.device_id, true);
            owner.links.disconnect(link, Disconnect::HandshakeFailed);
            oink_network::link::testing::flush_commands(&owner.links);
            assert!(owner.links.links().by_peer(peer.device_id).is_none());
            owner.probe_result("pending".into(), 1, Ok(peer.device_id));
            assert_eq!(owner.queries["pending"].peer, Some(peer.device_id));
            assert_eq!(owner.queries["pending"].boot, None);
            owner.drop_link(link);
            assert!(
                owner.queries["prepared"].released,
                "the retired prepared boot must be invalidated"
            );
            let pending = &owner.queries["pending"];
            let survived = !pending.released && pending.deadline == Some(deadline);
            // Waiting for a new greeting keeps the original total budget.
            owner.expire_queries(deadline);
            assert!(owner.queries["pending"].released);
            Ok(survived)
        })
        .unwrap();
    let mut results = Vec::new();
    while let Ok(change) = events.0.try_recv() {
        if let ConfigChange::PairingEvent(event) = change
            && let api::PairingEvent::AddressQueryFinished {
                operation_id,
                result: api::AddressQueryResult::Problem { problem },
                ..
            } = *event
        {
            results.push((operation_id, problem.code));
        }
    }
    results.sort();
    stop(&nodes).await;
    assert_eq!(
        results,
        vec![
            ("pending".into(), dto::PairCode::ConnectTimeout.number()),
            ("prepared".into(), dto::PairCode::PreparedPeerLost.number()),
        ]
    );
    assert!(
        pending_survived,
        "an unbound query must await its own greeting or total deadline"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreign_boot_facts_close_only_the_offending_c0_and_current_boot_readmits() {
    let nodes = start(same_group(3)).await;
    assert!(until(|| ready(&nodes)).await);
    let peer = nodes[1].identity.id();
    let old = nodes[0].links.links().by_peer(peer).unwrap().link;
    let other = nodes[0].links.links().by_peer(nodes[2].identity.id()).unwrap().link;
    let mut raw = nodes[0].group.local_facts().raw().clone();
    raw.insert(1, wire::boot(nodes[2].links.boot()).write());
    let forged = dto::DeviceFacts::from_raw(raw).unwrap();
    nodes[0]
        .group
        .call(move |owner| owner.send(old, Message::ConfigFacts(forged)).map_err(internal))
        .unwrap();
    assert!(until(|| ready(&nodes) && nodes[0].links.links().by_peer(peer).is_some_and(|l| l.link != old)).await);
    assert_eq!(
        nodes[0].links.links().by_peer(nodes[2].identity.id()).unwrap().link,
        other
    );
    assert_eq!(
        nodes[1].group.peer_boot(nodes[0].identity.id()),
        Some(nodes[0].links.boot())
    );
    stop(&nodes).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lookup_for_the_current_signed_group_resumes_member_admission() {
    let nodes = start(same_group(2)).await;
    assert!(until(|| ready(&nodes)).await);
    for node in &nodes {
        node.group.hold_periodic_facts();
    }
    for index in [0, 1] {
        let origin = nodes[index].identity.id();
        let sequence = nodes[index].group.call(|owner| owner.publish_facts()).unwrap();
        assert!(
            until(|| nodes[1 - index]
                .group
                .call(move |owner| Ok(owner
                    .facts
                    .get(&origin)
                    .is_some_and(|facts| facts.fact_sequence() == Ok(sequence))))
                .unwrap())
            .await
        );
    }
    // A greeting is immutable: a C0 opened during migration can describe an
    // earlier root even after both actors have applied the same signed target.
    // Model the resulting current-root lookup without unrelated periodic Facts.
    for node in &nodes {
        node.group
            .call(|owner| {
                for session in owner.sessions.values_mut() {
                    session.ready = false;
                }
                owner.publish()
            })
            .unwrap();
    }
    let peer = nodes[1].links.boot();
    nodes[0]
        .group
        .call(move |owner| {
            let link = owner.peer_link(peer).unwrap();
            owner.lookup(link).map_err(internal)
        })
        .unwrap();
    let admitted = until(|| ready(&nodes)).await;
    let no_lookup = nodes.iter().all(|node| {
        node.group
            .call(|owner| {
                Ok(!owner.member.lookups_pending()
                    && owner
                        .shared
                        .migration
                        .load()
                        .as_ref()
                        .as_ref()
                        .is_none_or(|status| status.phase == api::MigrationPhase::Adopted))
            })
            .unwrap()
    });
    stop(&nodes).await;
    assert!(
        admitted && no_lookup,
        "a current-root lookup must not strand signed members in ProofWaiting"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn peer_send_selection_rejects_c0_retired_before_its_lost_event_is_consumed() {
    let nodes = start(same_group(2)).await;
    assert!(until(|| ready(&nodes)).await);
    let peer = nodes[1].links.boot();
    let retired = nodes[0]
        .group
        .call(move |owner| {
            let link = owner.peer_link(peer).unwrap();
            owner.links.disconnect(link, Disconnect::HandshakeFailed);
            // Hold this owner turn while N0 publishes retirement. Its Lost
            // event is necessarily still queued, so the stale session exists.
            // Fence the command, not four seconds of scheduling or an empty
            // peer slot: N0 may already have selected a replacement lifetime.
            oink_network::link::testing::flush_commands(&owner.links);
            assert!(
                owner
                    .links
                    .links()
                    .by_peer(peer.device_id)
                    .is_none_or(|selected| selected.link != link)
            );
            assert!(owner.sessions.contains_key(&link));
            Ok(owner.peer_link(peer).is_none())
        })
        .unwrap();
    stop(&nodes).await;
    assert!(
        retired,
        "a queued Lost event must not leave a retired C0 selectable for sends"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ready_with_a_full_c0_outbox_returns_link_loss_without_panicking() {
    let nodes = start(same_group(2)).await;
    assert!(until(|| ready(&nodes)).await);
    let peer = nodes[1].identity.id();
    let outcome = nodes[0]
        .group
        .call(move |owner| {
            let link = *owner
                .sessions
                .iter()
                .find(|(_, s)| s.peer_boot.device_id == peer)
                .unwrap()
                .0;
            // Model the exact mandatory-queue limit before Ready publishes its
            // first Facts. That send retires C0 synchronously.
            owner.sessions.get_mut(&link).unwrap().outbox = std::iter::repeat_n(vec![0], 256).collect();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| owner.ready(link)));
            Ok(matches!(result, Ok(Err(_))) && !owner.sessions.contains_key(&link))
        })
        .unwrap();
    stop(&nodes).await;
    assert!(
        outcome,
        "Ready must propagate a retired send before reading its session again"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bounded_mandatory_c0_queue_closes_its_link_and_other_members_keep_running() {
    let nodes = start(same_group(3)).await;
    assert!(until(|| ready(&nodes)).await);
    let peer = nodes[1].identity.id();
    let link = nodes[0].links.links().by_peer(peer).unwrap().link;
    let other = nodes[0].links.links().by_peer(nodes[2].identity.id()).unwrap().link;
    let receiver = nodes[1].group.clone();
    let (entered, entering) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel();
    let stalled = tokio::task::spawn_blocking(move || {
        receiver.call(move |_| {
            entered.send(()).unwrap();
            released.recv().unwrap();
            Ok(())
        })
    });
    tokio::task::spawn_blocking(move || entering.recv().unwrap())
        .await
        .unwrap();
    let closed = nodes[0]
        .group
        .call(move |owner| {
            let frame = v2::frame::encode(
                &Message::ConfigState(owner.state.wire().clone()),
                v2::limits::CONFIG_FRAME,
            )
            .unwrap();
            for _ in 0..16384 {
                owner.send_bytes(link, frame.clone());
                if !owner.sessions.contains_key(&link) {
                    return Ok(true);
                }
            }
            Ok(false)
        })
        .unwrap();
    release.send(()).unwrap();
    stalled.await.unwrap().unwrap();
    assert!(closed, "mandatory C0 must never silently drop a frame");
    assert_eq!(
        nodes[0].links.links().by_peer(nodes[2].identity.id()).unwrap().link,
        other
    );
    assert!(until(|| ready(&nodes)).await);
    stop(&nodes).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn offline_source_member_adopts_two_sequential_durable_merges_after_restart() {
    let mut states = same_group(2);
    states.push(fresh(3).pop().unwrap());
    states.push(fresh(4).pop().unwrap());
    let mut nodes = start(states).await;
    assert!(
        until(|| nodes[0]
            .group
            .members()
            .members
            .iter()
            .any(|m| m.device == nodes[1].identity.id() && m.online))
        .await
    );
    nodes[1].group.stop().await;
    nodes[1].links.stop().await;
    for destination in [2usize, 3] {
        let id = request(&nodes[0], &nodes[destination], [destination as u8; 16]).await;
        let code = accept(&nodes[0], &nodes[destination], &id).await;
        submit(&nodes[destination], &id, 1, code);
        complete(&nodes[0], &nodes[destination], &id).await;
    }
    let target = nodes[3].group.applied().group;
    restart(&mut nodes[1]).await;
    assert!(
        until(|| nodes
            .iter()
            .all(|n| n.group.applied().group == target && n.group.members().members.len() == 4))
        .await
    );
    for node in &nodes {
        assert_eq!(node.config.lock().unwrap().owner().active_group(), Some(target));
    }
    stop(&nodes).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn online_removal_retires_authority_without_discarding_queued_revocation() {
    let nodes = start(same_group(2)).await;
    assert!(until(|| ready(&nodes)).await);
    let peer = nodes[1].identity.id();
    let original = nodes[1].group.applied().group;
    let receiver = nodes[1].group.clone();
    let (entered, entering) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel();
    let stalled = tokio::task::spawn_blocking(move || {
        receiver.call(move |_| {
            entered.send(()).unwrap();
            // Dropping release also unblocks this owner if an assertion fails.
            let _ = released.recv();
            Ok(())
        })
    });
    tokio::task::spawn_blocking(move || entering.recv().unwrap())
        .await
        .unwrap();
    // Keep the recipient from applying the queued revocation. Only in this
    // ordering must C0 still exist when remove_member returns: preventing this
    // owner's event loop from running does not stop the peer or transport.
    let draining = nodes[0]
        .group
        .call(move |owner| {
            let link = owner.selected_session(peer).unwrap().0;
            owner.remove_member(peer, None)?;
            Ok(owner.selected_session(peer).is_none()
                && owner
                    .sessions
                    .get(&link)
                    .is_some_and(|s| s.departing && !s.ready && s.deadline.is_some()))
        })
        .unwrap();
    release.send(()).unwrap();
    stalled.await.unwrap().unwrap();
    let delivered =
        until(|| nodes[1].group.applied().group != original && nodes[1].group.members().members.len() == 1).await;
    stop(&nodes).await;
    assert!(draining, "removal must drain its signed revocation before closing C0");
    assert!(delivered, "the retained C0 must deliver the signed revocation");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "slow: revocation ordering requires runtime clock injection"]
async fn online_removal_can_finish_revocation_before_shortcut_reconciliation() {
    let nodes = start(same_group(2)).await;
    assert!(until(|| ready(&nodes)).await);
    let peer = nodes[1].identity.id();
    let original = nodes[1].group.applied().group;
    let links = nodes[0].links.clone();
    let receiver = nodes[1].group.clone();
    let application = nodes[0].application.clone();
    let completed = Arc::new(AtomicBool::new(false));
    let observed = completed.clone();
    let closed = nodes[0]
        .group
        .call(move |owner| {
            let mut applies = 0;
            *application.apply_hook.lock().unwrap() = Some(Box::new(move || {
                applies += 1;
                if applies == 2 {
                    // The revocation was sent by the first commit. Hold shortcut-slot
                    // reconciliation until the recipient has applied it and closed C0.
                    let end = Instant::now() + Duration::from_secs(10);
                    while receiver.applied().group == original || links.links().by_peer(peer).is_some() {
                        assert!(Instant::now() < end, "recipient did not finish revocation");
                        std::thread::yield_now();
                    }
                    observed.store(true, Ordering::Release);
                }
            }));
            let link = owner
                .sessions
                .iter()
                .find(|(_, s)| s.peer_boot.device_id == peer)
                .unwrap()
                .0
                .to_owned();
            owner.remove_member(peer, None)?;
            application.apply_hook.lock().unwrap().take();
            Ok(!owner.sessions.contains_key(&link) && owner.selected_session(peer).is_none())
        })
        .unwrap();
    let delivered = nodes[1].group.applied().group != original && nodes[1].group.members().members.len() == 1;
    stop(&nodes).await;
    assert!(
        completed.load(Ordering::Acquire),
        "fixture must force delivery before reconciliation"
    );
    assert!(
        closed,
        "a completed revocation may synchronously retire C0 during the same owner command"
    );
    assert!(
        delivered,
        "closing C0 is valid only after the recipient applied revocation"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn offline_removed_member_retains_the_signed_tombstone_before_forming_a_fresh_group() {
    let mut nodes = start(same_group(3)).await;
    assert!(until(|| ready(&nodes)).await);
    let old = nodes[0].group.applied().group;
    nodes[1].group.stop().await;
    nodes[1].links.stop().await;
    nodes[0].group.remove_member(nodes[1].identity.id()).unwrap();
    assert!(until(|| nodes[2].group.members().members.len() == 2).await);
    restart(&mut nodes[1]).await;
    assert!(until(|| nodes[1].group.applied().group != old && nodes[1].group.members().members.len() == 1).await);
    assert_eq!(nodes[0].group.applied().group, old);
    assert_eq!(nodes[2].group.applied().group, old);
    assert_eq!(
        nodes[1].config.lock().unwrap().owner().active_group(),
        Some(nodes[1].group.applied().group)
    );
    stop(&nodes).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn in_flight_old_group_frames_do_not_close_a_retired_member_lane() {
    let nodes = start(same_group(2)).await;
    assert!(until(|| ready(&nodes)).await);
    for node in &nodes {
        node.group.hold_periodic_facts();
    }
    let (facts, state) = nodes[1]
        .group
        .call(|owner| Ok((owner.local.value().clone(), owner.state.wire().clone())))
        .unwrap();
    let peer = nodes[1].identity.id();
    let result = nodes[0]
        .group
        .call(move |owner| {
            let link = owner.links.links().by_peer(peer).unwrap().link;
            let old = owner.state.roster().anchor();
            // Freeze the same boundary as project_decision: business authority
            // is retired locally before the peer receives the migration proof.
            // Its already queued Facts/State may still arrive on this C0.
            owner.sessions.get_mut(&link).unwrap().ready = false;
            owner.links.retire(link);
            let next = State::create(&owner.identity, "next", "test", 2).unwrap();
            let next = next
                .with_roster(&next.roster().add(&owner.identity, peer, "peer", "test", 3).unwrap())
                .unwrap();
            owner.commit(next).unwrap();
            let current = owner.state.digest().unwrap();
            assert_ne!(old, owner.state.roster().anchor());
            let frames = [Message::ConfigFacts(facts.clone()), Message::ConfigState(state)];
            let outcomes = frames
                .map(|message| owner.control(link, &v2::frame::encode(&message, v2::limits::CONFIG_FRAME).unwrap()));
            assert_eq!(owner.state.digest().unwrap(), current);
            assert!(!owner.sessions[&link].ready);
            // A bare greeting has no historical member authority.
            owner.sessions.get_mut(&link).unwrap().admitted_anchor = None;
            let unauthenticated = owner.control(
                link,
                &v2::frame::encode(&Message::ConfigFacts(facts), v2::limits::CONFIG_FRAME).unwrap(),
            );
            assert!(unauthenticated.is_err());
            Ok(outcomes)
        })
        .unwrap();
    stop(&nodes).await;
    assert!(result.iter().all(Result::is_ok), "old member frames: {result:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lookup_proof_after_target_projection_resumes_signed_admission() {
    let nodes = start(fresh(2)).await;
    let source_state = nodes[0].group.call(|owner| Ok(owner.state.clone())).unwrap();
    let source = source_state.roster().anchor();
    let pair = request(&nodes[0], &nodes[1], [11; 16]).await;
    let code = accept(&nodes[0], &nodes[1], &pair).await;
    submit(&nodes[1], &pair, 1, code);
    complete(&nodes[0], &nodes[1], &pair).await;
    assert!(until(|| ready(&nodes)).await);
    let requester = nodes[0].identity.id();
    let bundle = nodes[1]
        .group
        .call(move |owner| Ok(owner.member.journal.lookup(requester, source, None).unwrap().unwrap()))
        .unwrap();
    let context = dto::ObjectContext::from_raw(
        [
            (1, dto::ObjectContextKind::GroupRef.write()),
            (
                2,
                RawValue::Map([(1, oink_network::group_merge::group_ref(source).write())].into()),
            ),
        ]
        .into(),
    )
    .unwrap();
    let (reference, messages) =
        oink_network::pair_objects::transfer(context, dto::ObjectKind::Transfer, bundle.raw()).unwrap();
    let header = Message::ConfigMergeTransfer(dto::MergeTransfer::from_raw([(1, reference.write())].into()).unwrap());
    let frames = std::iter::once(header)
        .chain(messages)
        .map(|message| v2::frame::encode(&message, v2::limits::CONFIG_FRAME).unwrap())
        .collect::<Vec<_>>();
    let peer = nodes[1].links.boot();
    let target_state = nodes[0].group.call(|owner| Ok(owner.state.clone())).unwrap();
    // Reopen a real, unadmitted C0 with the source state. The target peer's
    // lookup for its own root is refused until our pending projection wins.
    nodes[0]
        .group
        .call(move |owner| {
            owner.disconnect_all(Disconnect::LeavingGroup);
            owner.commit(source_state)
        })
        .unwrap();
    assert!(
        until(|| nodes[1]
            .group
            .group_migration()
            .as_ref()
            .as_ref()
            .is_some_and(|status| status.problem.is_some()))
        .await
    );
    assert!(
        until(|| nodes[0]
            .group
            .call(move |owner| {
                Ok(owner.selected_session(peer.device_id).is_some_and(|(_, session)| {
                    session.peer.is_some() && !session.ready && session.admitted_anchor.is_none()
                }))
            })
            .unwrap())
        .await
    );
    let (link, resumed) = nodes[0]
        .group
        .call(move |owner| {
            let link = owner.peer_link(peer).unwrap();
            // Force the ordering on one owner turn: this replacement C0 has
            // an outstanding source lookup, then another recovery installs the
            // signed target before the encoded reply reaches its control parser.
            owner
                .member
                .inject_lookup(link, source, Instant::now() + Duration::from_secs(10));
            owner.commit(target_state)?;
            for frame in frames {
                owner.control(link, &frame).map_err(internal)?;
            }
            let session = &owner.sessions[&link];
            Ok((link, session.sent.is_some() && session.deadline.is_some()))
        })
        .unwrap();
    let admitted = resumed && until(|| ready(&nodes)).await;
    let same_link = nodes[0].links.links().by_peer(peer.device_id).map(|entry| entry.link) == Some(link);
    let migration = nodes[0].group.group_migration();
    stop(&nodes).await;
    assert!(
        resumed,
        "a completed lookup must not leave a member C0 parked without admission or a deadline"
    );
    assert!(admitted, "the resumed signed admission must make both members ready");
    assert!(same_link, "readmission must complete on the existing C0");
    assert!(
        migration
            .as_ref()
            .as_ref()
            .is_some_and(|status| status.phase == api::MigrationPhase::Adopted && status.problem.is_none())
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replacing_prepared_query_after_member_removal_refreshes_pairing_context() {
    let nodes = start(fresh(3)).await;
    let pair = request(&nodes[0], &nodes[1], [1; 16]).await;
    let code = accept(&nodes[0], &nodes[1], &pair).await;
    submit(&nodes[1], &pair, 1, code);
    complete(&nodes[0], &nodes[1], &pair).await;
    assert!(until(|| ready(&nodes[..2])).await);
    let _prepared = query(&nodes[2], &nodes[0], [2; 16]).await;
    nodes[0].group.remove_member(nodes[1].identity.id()).unwrap();
    assert!(until(|| nodes[0].group.members().members.len() == 1 && nodes[1].group.members().members.len() == 1).await);
    let next = request(&nodes[2], &nodes[0], [2; 16]).await;
    assert!(
        until(|| nodes[0].group.pairings().requests.iter().any(|r| r.id == next)
            || nodes[2]
                .group
                .pairings()
                .sessions
                .iter()
                .any(|s| s.id == next && s.phase == 8))
        .await
    );
    let outgoing = nodes[2].group.pairings();
    let incoming = nodes[0].group.pairings();
    assert!(
        incoming.requests.iter().any(|r| r.id == next),
        "outgoing={outgoing:?}; incoming={incoming:?}"
    );
    let code = accept(&nodes[2], &nodes[0], &next).await;
    submit(&nodes[0], &next, 1, code);
    complete(&nodes[2], &nodes[0], &next).await;
    assert!(
        until(|| nodes[0]
            .group
            .members()
            .members
            .iter()
            .any(|m| m.device == nodes[2].identity.id() && m.online)
            && nodes[2]
                .group
                .members()
                .members
                .iter()
                .any(|m| m.device == nodes[0].identity.id() && m.online))
        .await
    );
    stop(&nodes).await;
}

/// H16: a peer-chosen PairProblem.winner that this owner cannot re-encode is a
/// protocol violation on that C0, never a panic of the configuration thread.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_peer_pair_problem_closes_its_c0_and_the_owner_keeps_running() {
    let nodes = start(fresh(2)).await;
    let id = request(&nodes[0], &nodes[1], [31; 16]).await;
    assert!(until(|| nodes[1].group.pairings().requests.iter().any(|r| r.id == id)).await);
    let pair = pairing::parse_key(&id).unwrap();
    // The responder declines; the initiator records that failure as a durable
    // local event, which converts the peer's problem.
    let peer = nodes[1].identity.id();
    let mut winner = wire::pair(pair).into_raw();
    winner.insert(99, RawValue::Bytes(vec![7; 9000]));
    let mut problem = pairing::problem(dto::PairCode::Declined, dto::PairStage::Request).into_raw();
    problem.insert(5, RawValue::Map(winner));
    let declined = Message::ConfigPairDeclined(
        dto::PairDeclined::from_raw([(1, wire::pair(pair).write()), (2, RawValue::Map(problem))].into()).unwrap(),
    );
    let frame = v2::frame::encode(&declined, v2::limits::CONFIG_FRAME).unwrap();
    let refused = nodes[0]
        .group
        .call(move |owner| {
            let link = owner.links.links().by_peer(peer).unwrap().link;
            Ok(owner.control(link, &frame).is_err())
        })
        .unwrap();
    assert!(refused, "the peer's malformed problem is refused");
    // The owner thread is alive and serves later commands.
    assert!(nodes[0].group.call(|_| Ok(())).is_ok());
    stop(&nodes).await;
}

/// M35: a cancel refused for a stale revision is not the answer for the next
/// revision of the same pairing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_after_a_stale_revision_uses_the_current_revision() {
    let nodes = start(fresh(2)).await;
    let id = request(&nodes[0], &nodes[1], [32; 16]).await;
    let session = nodes[0]
        .group
        .pairings()
        .sessions
        .iter()
        .find(|s| s.id == id)
        .unwrap()
        .clone();
    let stale = nodes[0]
        .group
        .cancel_pairing(id.clone(), session.revision.0 + 100)
        .unwrap();
    assert!(
        matches!(&stale, api::PairReply::Problem(api::PairRejection::Problem { problem }) if problem.code == dto::PairCode::StaleRevision.number()),
        "{stale:?}"
    );
    assert!(matches!(
        nodes[0].group.cancel_pairing(id, session.revision.0).unwrap(),
        api::PairReply::Value(api::CancelResult::Cancelled)
    ));
    stop(&nodes).await;
}
