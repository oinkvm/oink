//! Bounded event-order exploration using the production router and capability
//! objects. Native outcomes are explicit inputs, never inferred OS behaviour.
use super::*;
use crate::{Os, RouterConfig};

fn permutations(values: &mut [u8], at: usize, out: &mut Vec<Vec<u8>>) {
    if at == values.len() {
        out.push(values.to_vec());
        return;
    }
    for i in at..values.len() {
        values.swap(at, i);
        permutations(values, at + 1, out);
        values.swap(at, i);
    }
}
fn router(me: DeviceId, peer: DeviceId, os: Os) -> Router {
    let mut r = Router::new(
        RouterConfig::new(
            oink_foundation::session::Boot {
                device_id: me,
                nonce: [me.0[0]; 16],
            },
            "model",
            os,
        ),
        crate::router::connection_tests::session_layout(me, peer),
    );
    r.set_dnd_available(true);
    r.test_connect(peer, 1);
    r.out.clear();
    r
}
fn notice(source: DeviceId, target: DeviceId, input: DeviceId) -> DragNotice {
    let id = ClipId {
        origin: source,
        seq: DND_SEQ_BIT | 1,
    };
    DragNotice {
        key: DragKey {
            offer: Router::dnd_offer(id),
            native_generation: 1,
        },
        revision: 1,
        input,
        state: DragState::Offered {
            holder: Holder {
                device: target,
                epoch: 2,
            },
            mode: SourceMode::Held,
        },
        metadata: MetadataVersion {
            revision: 1,
            readiness: MetadataReadiness::Ready,
        },
        preview: PreviewAvailability::Absent,
        retained_inventory: None,
        items: vec![ClipData::Text("model".into())],
    }
}

#[test]
fn every_role_os_partition_preserves_one_original_up_in_both_catcher_loss_orders() {
    let devices = [DeviceId([1; 32]), DeviceId([2; 32]), DeviceId([3; 32])];
    // S=I=T, S=I, S=T, I=T, and all distinct: 2+4+4+4+8 OS assignments.
    let mut configurations = 0;
    for (input, target, count) in [(0, 0, 1), (0, 1, 2), (1, 0, 2), (1, 1, 2), (1, 2, 3)] {
        for bits in 0..1 << count {
            configurations += 1;
            let os = |i| if bits & (1 << i) == 0 { Os::MacOS } else { Os::Windows };
            for up_first in [false, true] {
                for receipt_ok in [false, true] {
                    let mut r = router(devices[0], devices[1], os(0));
                    if count == 3 {
                        r.test_connect(devices[2], 1);
                    }
                    for (i, device) in devices.iter().enumerate().take(count).skip(1) {
                        r.peers.get_mut(device).unwrap().os = os(i);
                    }
                    let id = ClipId {
                        origin: devices[0],
                        seq: DND_SEQ_BIT | 1,
                    };
                    let offer = Router::dnd_offer(id);
                    let mut s = source::Source::new(
                        offer,
                        devices[input],
                        Holder {
                            device: devices[target],
                            epoch: 2,
                        },
                    );
                    s.phase = source::Phase::Held;
                    s.confirmed = true;
                    s.ever_held = true;
                    s.native = NativeObservation::Held;
                    r.dnd.sources.insert(offer, s);
                    r.dnd.native_source = Some(offer);
                    r.dnd_take_source_press(offer, devices[input]);
                    r.out.clear();
                    if up_first {
                        assert!(r.dnd_original_up(devices[input]));
                    }
                    r.on_dnd_event(DndEvent::SourceCatcherLost { id });
                    // Duplicate/late native callbacks have no new authority.
                    r.on_dnd_event(DndEvent::SourceEnded { id, accepted: false });
                    if !up_first {
                        assert!(!r.dnd_original_up(devices[input]));
                    }
                    r.on_dnd_event(DndEvent::SourceButtonReleased { id, ok: receipt_ok });
                    r.on_dnd_event(DndEvent::SourceButtonReleased { id, ok: !receipt_ok });
                    assert_eq!(
                        r.out
                            .iter()
                            .filter(|o| matches!(o, Output::Dnd(DndCmd::ReleaseSourceButton { .. })))
                            .count(),
                        usize::from(up_first)
                    );
                    let records: Vec<_> = r
                        .out
                        .iter()
                        .filter_map(|o| match o {
                            Output::Dnd(DndCmd::Integrity { record, .. }) => Some(record),
                            _ => None,
                        })
                        .collect();
                    assert_eq!(
                        records.len(),
                        1,
                        "partition={input}/{target}, os={bits}, up_first={up_first}"
                    );
                    assert_eq!(records[0].native, NativeObservation::CatcherLost);
                    assert_eq!(records[0].source_os, os(0));
                    assert_eq!(records[0].input_os, os(input));
                    assert_eq!(records[0].target_os, os(target));
                    assert!(r.dnd.presses.is_empty());
                    assert!(r.dnd.sources.is_empty());
                }
            }
        }
    }
    assert_eq!(configurations, 22);
}

#[test]
fn release_enter_offer_focus_and_async_down_all_permutations_never_revive() {
    let me = DeviceId([1; 32]);
    let source = DeviceId([2; 32]);
    let mut orders = Vec::new();
    permutations(&mut [0, 1, 2, 3, 4], 0, &mut orders);
    for os in [Os::MacOS, Os::Windows] {
        for order in &orders {
            let mut r = router(me, source, os);
            let offered = notice(source, me, source);
            let id = offered.key.offer.clip;
            let mut released = offered.clone_without_items();
            released.revision = 2;
            released.state = DragState::Released {
                holder: Holder { device: me, epoch: 2 },
                mode: SourceMode::Held,
            };
            let mut down = false;
            let mut grants = Vec::new();
            for event in order {
                match event {
                    0 => r.on_dnd_msg(source, DndMsg::Enter { id, epoch: 2 }),
                    1 => r.on_dnd_notice(source, offered.clone(), false, 3),
                    2 => {
                        r.holder = me;
                        r.epoch = 2;
                        r.dnd_focus_entered(source, 2, Point::new(4., 5.));
                    }
                    3 => r.on_dnd_notice(source, released.clone(), false, 4),
                    4 => {
                        for g in &grants {
                            let g: &DndTarget = g;
                            if g.claim_down() {
                                g.down_submitted(true);
                                down = true;
                            }
                        }
                    }
                    _ => unreachable!(),
                }
                for o in r.out.drain(..) {
                    if let Output::Dnd(DndCmd::Begin { grant, .. }) = o {
                        grants.push(grant);
                    }
                }
            }
            assert!(
                r.dnd.pending.is_none(),
                "released cannot leave a pending expectation: {order:?}"
            );
            let mut ups = 0;
            for g in grants {
                assert!(!g.begin_current(), "{order:?}");
                assert!(!g.claim_down());
                if g.claim_up() {
                    ups += 1;
                }
                assert!(!g.claim_up());
            }
            assert_eq!(ups, usize::from(down), "{order:?}");
        }
    }
}

#[test]
fn metadata_revision_and_source_session_revoke_only_their_preparation() {
    let me = DeviceId([1; 32]);
    let source = DeviceId([2; 32]);
    let mut r = router(me, source, Os::MacOS);
    let mut n = notice(source, me, source);
    n.state = DragState::Announced;
    n.items.clear();
    n.metadata.readiness = MetadataReadiness::Pending;
    r.on_dnd_notice(source, n.clone(), false, 1);
    assert!(r.dnd.preparations.is_empty());
    n.revision += 1;
    n.metadata.readiness = MetadataReadiness::Ready;
    r.on_dnd_notice(source, n.clone(), false, 2);
    let old = r.dnd.preparations[&source].clone();
    let delivery = DndTarget::new(old.clone(), Holder { device: me, epoch: 2 }, n.revision, source);
    delivery.delivered();
    r.dnd.deliveries.insert(n.key, delivery.clone());
    n.revision += 1;
    n.metadata.revision += 1;
    r.on_dnd_notice(source, n.clone(), false, 3);
    assert!(!old.current());
    assert!(delivery.content_current());
    let fresh = r.dnd.preparations[&source].clone();
    r.dnd_peer_lost(source);
    assert!(!fresh.current());
    assert!(!delivery.content_current());
}

#[test]
fn an_older_delivery_terminal_survives_a_newer_drag_announcement() {
    let me = DeviceId([1; 32]);
    let source = DeviceId([2; 32]);
    let mut r = router(me, source, Os::MacOS);
    let n = notice(source, me, source);
    r.on_dnd_notice(source, n.clone(), false, 1);
    let g = DndTarget::new(
        r.dnd.preparations[&source].clone(),
        Holder { device: me, epoch: 2 },
        n.revision,
        source,
    );
    g.delivered();
    r.dnd.deliveries.insert(n.key, g.clone());
    let mut next = n.clone();
    next.key.native_generation += 1;
    next.key.offer.clip.seq += 1;
    next.key.offer.generation += 1;
    r.on_dnd_notice(source, next, false, 2);
    assert!(g.content_current());
    let mut end = n.clone_without_items();
    end.revision += 1;
    end.state = DragState::Ended { accepted: true };
    r.on_dnd_notice(source, end, false, 3);
    assert!(!g.content_current());
    assert!(r.dnd.deliveries.is_empty());
}

#[test]
fn accepted_native_end_keeps_content_until_target_stored() {
    let me = DeviceId([1; 32]);
    let target = DeviceId([2; 32]);
    let mut r = router(me, target, Os::MacOS);
    let id = ClipId {
        origin: me,
        seq: DND_SEQ_BIT | 1,
    };
    let offer = Router::dnd_offer(id);
    let holder = Holder {
        device: target,
        epoch: 2,
    };
    let mut s = source::Source::new(offer, me, holder);
    s.phase = source::Phase::Committing;
    s.ever_held = true;
    r.dnd.sources.insert(offer, s);
    r.dnd.native_source = Some(offer);
    r.dnd_take_source_press(offer, me);
    r.dnd_release_source_button(offer);
    r.on_dnd_event(DndEvent::SourceButtonReleased { id, ok: true });
    r.out.clear();
    r.on_dnd_event(DndEvent::SourceEnded { id, accepted: true });
    assert_eq!(r.dnd.sources[&offer].phase, source::Phase::Receiving);
    assert!(
        !r.out
            .iter()
            .any(|o| matches!(o, Output::Dnd(DndCmd::ReleaseData { .. } | DndCmd::Integrity { .. })))
    );
    assert!(r.on_dnd_delivery_receipt(
        target,
        oink_foundation::content::DeliveryId {
            offer,
            receiver: target,
            sequence: 2
        },
        true
    ));
    assert!(r.dnd.sources.is_empty());
    assert_eq!(
        r.out
            .iter()
            .filter(|o| matches!(o, Output::Dnd(DndCmd::Integrity { .. })))
            .count(),
        1
    );
}

#[test]
fn stored_text_waits_for_native_source_drop_before_publishing_end_or_cancelling_readers() {
    let me = DeviceId([1; 32]);
    let target = DeviceId([2; 32]);
    let mut r = router(me, target, Os::MacOS);
    r.peers.get_mut(&target).unwrap().os = Os::Windows;
    let id = ClipId {
        origin: me,
        seq: DND_SEQ_BIT | 1,
    };
    let offer = Router::dnd_offer(id);
    let holder = Holder {
        device: target,
        epoch: 2,
    };
    let mut s = source::Source::new(offer, target, holder);
    s.items = vec![ClipData::Text("text".into())];
    s.phase = source::Phase::Committing;
    s.ever_held = true;
    r.dnd.sources.insert(offer, s);
    r.dnd.native_source = Some(offer);
    r.dnd_take_source_press(offer, target);
    r.dnd_release_source_button(offer);
    r.on_dnd_event(DndEvent::SourceButtonReleased { id, ok: true });
    r.out.clear();
    let delivery = oink_foundation::content::DeliveryId {
        offer,
        receiver: target,
        sequence: 2,
    };

    // Windows can consume already cached hover text and ACK it while AppKit
    // is still inside performDragOperation, before Deferred::complete returns.
    assert!(r.on_dnd_delivery_receipt(target, delivery, true));
    assert_eq!(r.dnd.sources[&offer].phase, source::Phase::Committing);
    assert!(
        !r.out.iter().any(|o| {
            matches!(o, Output::Dnd(DndCmd::ReleaseData { .. } | DndCmd::Integrity { .. }))
                || matches!(o, Output::Send { msg: Message::Dnd(DndMsg::Notice(notice)), .. }
                if notice.state.terminal())
        }),
        "a stored receipt must not cancel the in-flight native snapshot or host"
    );
    assert!(!r.on_dnd_delivery_receipt(target, delivery, true));
    r.on_dnd_event(DndEvent::SourceEnded { id, accepted: true });
    assert!(!r.dnd.sources.contains_key(&offer));
    assert!(r.out.iter().any(|o| matches!(o,
        Output::Send { msg: Message::Dnd(DndMsg::Notice(notice)), .. }
            if notice.state == DragState::Ended { accepted: true }
    )));
    assert_eq!(
        r.out
            .iter()
            .filter(|o| matches!(o,
                Output::Dnd(DndCmd::ReleaseData { id: found }) if *found == id
            ))
            .count(),
        1
    );
    assert_eq!(
        r.out
            .iter()
            .filter(|o| matches!(o, Output::Dnd(DndCmd::Integrity { .. })))
            .count(),
        1
    );
}

#[test]
fn an_old_release_receipt_cannot_consume_the_next_physical_up() {
    let me = DeviceId([1; 32]);
    let input = DeviceId([2; 32]);
    let mut r = router(me, input, Os::Windows);
    let mut old = notice(me, me, input).key;
    let mut p = press::OriginalPress::new(old, input);
    p.withhold();
    p.physical_up();
    p.submit();
    r.dnd.presses.insert(old.offer, p);
    old.offer.clip.seq += 1;
    old.offer.generation += 1;
    old.native_generation += 1;
    r.dnd.presses.insert(old.offer, press::OriginalPress::new(old, input));
    assert!(
        !r.dnd_original_up(input),
        "the next ordinary UP returns to its own input ledger"
    );
    assert_eq!(r.dnd.presses.len(), 1);
    let previous = r.dnd.presses.keys().next().unwrap().clip;
    r.on_dnd_event(DndEvent::SourceButtonReleased {
        id: previous,
        ok: false,
    });
    assert!(r.dnd.presses.is_empty());
}

#[test]
fn late_source_effect_failure_cannot_close_a_newer_holder_epoch() {
    let me = DeviceId([1; 32]);
    let peer = DeviceId([2; 32]);
    let mut r = router(me, peer, Os::MacOS);
    let id = ClipId {
        origin: me,
        seq: DND_SEQ_BIT | 1,
    };
    let offer = Router::dnd_offer(id);
    let mut s = source::Source::new(offer, me, Holder { device: peer, epoch: 3 });
    s.phase = source::Phase::Held;
    r.dnd.sources.insert(offer, s);
    r.on_dnd_event(DndEvent::SourceEffectFailed {
        id,
        epoch: 2,
        effect: DndSourceEffect::Resume,
    });
    assert_eq!(r.dnd.sources[&offer].phase, source::Phase::Held);
}

#[test]
fn failed_proxy_up_revokes_that_host_without_retrying_up_or_touching_another_epoch() {
    let me = DeviceId([1; 32]);
    let source = DeviceId([2; 32]);
    let mut r = router(me, source, Os::MacOS);
    let n = notice(source, me, source);
    let id = n.key.offer.clip;
    r.on_dnd_msg(source, DndMsg::Enter { id, epoch: 2 });
    r.holder = me;
    r.epoch = 2;
    r.dnd_focus_entered(source, 2, Point::new(2., 3.));
    r.on_dnd_notice(source, n, false, 1);
    let grant = r.dnd.target.clone().unwrap();
    assert!(grant.claim_down());
    grant.down_submitted(true);
    assert!(grant.claim_up());
    r.out.clear();
    r.on_dnd_event(DndEvent::HostReleaseFailed { id, epoch: 1 });
    assert!(grant.content_current());
    r.on_dnd_event(DndEvent::HostReleaseFailed { id, epoch: 2 });
    assert!(!grant.content_current());
    assert!(!grant.claim_up());
    assert!(matches!(r.dnd.state, State::Idle));
    assert!(
        r.out
            .iter()
            .any(|o| matches!(o,Output::Dnd(DndCmd::Cancel {id:active,epoch:2,..}) if *active==id))
    );
}

#[test]
fn retired_returned_capture_cannot_compete_with_recaptured_native_drag_for_the_original_up() {
    returned_capture_then_recapture(false);
}

#[test]
fn abandoned_probe_retires_returned_capture_before_the_next_original_up() {
    returned_capture_then_recapture(true);
}

fn returned_capture_then_recapture(abandon_probe: bool) {
    let me = DeviceId([1; 32]);
    let peer = DeviceId([2; 32]);
    let other = DeviceId([3; 32]);
    for os in [Os::MacOS, Os::Windows] {
        for input in [me, peer, other] {
            let mut r = router(me, peer, os);
            r.test_connect(other, 1);
            let old = ClipId {
                origin: me,
                seq: DND_SEQ_BIT | 1,
            };
            let old_offer = Router::dnd_offer(old);
            let new = ClipId {
                origin: me,
                seq: DND_SEQ_BIT | 2,
            };
            let new_offer = Router::dnd_offer(new);
            r.dnd_source_learned(old, input, vec![ClipData::Text("drag".into())]);
            r.dnd_take_source_press(old_offer, input);
            let source = r.dnd.sources.get_mut(&old_offer).unwrap();
            source.ever_held = true;
            source.native = NativeObservation::Resumed;
            r.dnd_return_source_press(old_offer);
            if abandon_probe {
                // After a native return, the pointer can touch and leave an
                // edge before the platform reports another capture. Retiring
                // this probe must retire the old observation with its source.
                r.dnd.state = State::Probing {
                    input,
                    gesture: EdgeGesture::fixture(r.me(), peer, Point::new(1., 1.), (1., 0.), 0.),
                    content: None,
                };
                r.out.clear();
                r.dnd_off_edge();
                assert!(!r.dnd.sources.contains_key(&old_offer));
                assert!(
                    !r.dnd.presses.contains_key(&old_offer),
                    "abandoned probe orphaned the returned press and can steal the next UP"
                );
                assert!(r.out.iter().any(|o| matches!(o,
                    Output::Dnd(DndCmd::Integrity { id, record }) if *id == old
                        && record.physical == integrity::PhysicalObservation::Down
                        && record.disposition == SourceButtonDisposition::ReturnedToOriginalRoute
                )));
                assert!(
                    !r.out.iter().any(|o| matches!(
                        o,
                        Output::Dnd(DndCmd::ReleaseSourceButton { .. } | DndCmd::FinishSource { .. })
                    )),
                    "the ordinary route still owns the original DOWN"
                );
            }
            // Native backends issue a fresh capture ticket when the same
            // still-held source is probed again after returning to its app.
            r.dnd_source_learned(new, input, vec![ClipData::Text("drag".into())]);
            r.dnd_take_source_press(new_offer, input);
            let source = r.dnd.sources.get_mut(&new_offer).unwrap();
            source.ever_held = true;
            source.phase = source::Phase::Held;
            source.holder = Holder { device: peer, epoch: 3 };
            assert!(
                !r.dnd.presses.contains_key(&old_offer),
                "retired ordinary observation still matches the real UP"
            );
            assert_eq!(r.dnd.presses.len(), 1);
            r.out.clear();
            assert!(r.dnd_original_up(input));
            assert_eq!(r.dnd.sources[&new_offer].phase, source::Phase::Released);
            assert!(r.out.iter().any(
                |o| matches!(o, Output::Send { to, msg: Message::Dnd(DndMsg::Notice(n)), .. }
                if *to == peer && n.key.offer == new_offer && matches!(n.state, DragState::Released { .. }))
            ));
        }
    }
}

fn group_metadata() -> Vec<ClipData> {
    use oink_foundation::content::{ContentSize, FileNode, NodeId, NodeKind};
    vec![
        ClipData::FileRoots(vec![FileNode {
            exec_bits: None,
            id: NodeId(1),
            parent: None,
            name: "model.txt".into(),
            kind: NodeKind::File,
            size: ContentSize::Unknown,
            version: 1,
            modified_ms: None,
        }]),
        ClipData::Text("source-only payload".into()),
    ]
}
fn sent_notices(r: &Router) -> Vec<(DeviceId, DragNotice)> {
    r.out
        .iter()
        .filter_map(|o| match o {
            Output::Send {
                to,
                msg: Message::Dnd(DndMsg::Notice(n)),
                ..
            } => Some((*to, n.clone())),
            _ => None,
        })
        .collect()
}

#[test]
fn all_members_receive_early_metadata_and_matching_end_without_layout_or_capability_filters() {
    let source = DeviceId([1; 32]);
    let target = DeviceId([2; 32]);
    let members: HashSet<_> = (2..=22).map(|i| DeviceId([i; 32])).collect();
    let holder = Holder {
        device: target,
        epoch: 2,
    };
    for state in [
        DragState::Released {
            holder,
            mode: SourceMode::Held,
        },
        DragState::Revoked {
            scope: RevokeScope::Drag,
            reason: RevokeReason::Cancelled,
        },
        DragState::Ended { accepted: false },
        DragState::Ended { accepted: true },
    ] {
        let mut r = router(source, target, Os::MacOS);
        for peer in &members {
            r.test_connect(*peer, 1);
        }
        // All but target have no geometry; one is connected for control only.
        let control_only = DeviceId([22; 32]);
        r.peers.get_mut(&control_only).unwrap().sharing = false;
        r.on_dnd_msg(control_only, DndMsg::Caps { enabled: false });
        let disconnected = DeviceId([23; 32]);
        r.test_connect(disconnected, 1);
        r.test_disconnect(disconnected, 2);
        assert!(!r.layout.contains_device(control_only));
        let id = notice(source, target, source).key.offer.clip;
        r.out.clear();
        r.dnd_source_learned(id, source, group_metadata());
        let announced = sent_notices(&r);
        assert_eq!(announced.iter().map(|(to, _)| *to).collect::<HashSet<_>>(), members);
        assert_eq!(announced.len(), members.len());
        for (_, n) in &announced {
            assert_eq!(n.state, DragState::Announced);
            assert_eq!(n.preview, PreviewAvailability::Unknown);
            assert_eq!(n.items, group_metadata()[..1]);
        }
        r.out.clear();
        let offer = Router::dnd_offer(id);
        let s = r.dnd.sources.get_mut(&offer).unwrap();
        s.holder = holder;
        s.advance();
        r.dnd_publish_state(offer, state);
        let ended = sent_notices(&r);
        assert_eq!(ended.iter().map(|(to, _)| *to).collect::<HashSet<_>>(), members);
        assert!(
            ended
                .iter()
                .all(|(_, n)| n.state == state && n.items.is_empty() && n.revision > announced[0].1.revision)
        );
    }
}

#[test]
fn all_members_never_entered_non_neighbor_retires_without_begin_preview_or_content_authority() {
    let source = DeviceId([1; 32]);
    let target = DeviceId([2; 32]);
    let member = DeviceId([3; 32]);
    let holder = Holder {
        device: target,
        epoch: 2,
    };
    for os in [Os::MacOS, Os::Windows] {
        for state in [
            DragState::Released {
                holder,
                mode: SourceMode::Held,
            },
            DragState::Revoked {
                scope: RevokeScope::Preparation,
                reason: RevokeReason::Superseded,
            },
            DragState::Revoked {
                scope: RevokeScope::Holder(holder),
                reason: RevokeReason::Cancelled,
            },
            DragState::Revoked {
                scope: RevokeScope::Drag,
                reason: RevokeReason::Cancelled,
            },
            DragState::Ended { accepted: false },
            DragState::Ended { accepted: true },
        ] {
            let mut r = router(member, target, os);
            r.test_connect(source, 1);
            assert!(!r.layout.contains_device(source));
            r.out.clear();
            let mut early = notice(source, target, source);
            early.state = DragState::Announced;
            early.items = group_metadata()[..1].to_vec();
            early.preview = PreviewAvailability::Unknown;
            r.on_dnd_notice(source, early.clone(), false, 1);
            let preparation = r.dnd.preparations[&source].clone();
            assert!(preparation.current());
            let mut end = early.clone_without_items();
            end.revision += 1;
            end.state = state;
            r.on_dnd_notice(source, end, false, 2);
            assert!(!preparation.current(), "{os:?}: {state:?}");
            assert!(r.dnd.preparations.is_empty());
            // A delayed attachment or replay cannot resurrect this preparation.
            r.on_dnd_notice(source, early, false, 3);
            assert!(r.dnd.preparations.is_empty());
            assert!(r.dnd.pending.is_none() && r.dnd.target.is_none() && r.dnd.deliveries.is_empty());
            assert_eq!(
                r.out
                    .iter()
                    .filter(|o| matches!(o, Output::Dnd(DndCmd::Prepare { .. })))
                    .count(),
                1
            );
            assert!(!r.out.iter().any(|o| matches!(
                o,
                Output::Dnd(DndCmd::Begin { .. })
                    | Output::Send {
                        msg: Message::Dnd(DndMsg::PreviewRequest { .. }),
                        ..
                    }
            )));
        }
    }
}

#[test]
fn all_members_late_attachment_gets_only_current_metadata_without_revising_existing_holder() {
    let source = DeviceId([1; 32]);
    let target = DeviceId([2; 32]);
    let late = DeviceId([3; 32]);
    let mut r = router(source, target, Os::MacOS);
    let id = notice(source, target, source).key.offer.clip;
    r.dnd_source_learned(id, source, group_metadata());
    let offer = Router::dnd_offer(id);
    let s = r.dnd.sources.get_mut(&offer).unwrap();
    s.confirmed = true;
    s.phase = source::Phase::Held;
    s.holder = Holder {
        device: target,
        epoch: 2,
    };
    r.dnd_publish_offer(offer);
    let revision = r.dnd.sources[&offer].revision;
    r.out.clear();
    assert!(r.attach_input(
        oink_foundation::session::Boot {
            device_id: late,
            nonce: [3; 16]
        },
        [3; 16],
        false,
        false,
        Default::default(),
        2
    ));
    let notices = sent_notices(&r);
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].0, late);
    assert_eq!(notices[0].1.state, DragState::Announced);
    assert_eq!(notices[0].1.items, group_metadata()[..1]);
    assert_eq!(r.dnd.sources[&offer].revision, revision);
    r.detach_input(late);
    r.dnd.sources.get_mut(&offer).unwrap().phase = source::Phase::Released;
    r.out.clear();
    assert!(r.attach_input(
        oink_foundation::session::Boot {
            device_id: late,
            nonce: [4; 16]
        },
        [4; 16],
        false,
        false,
        Default::default(),
        3
    ));
    assert!(
        sent_notices(&r).is_empty(),
        "a released source cannot prepare new members"
    );
}

#[test]
fn all_members_handoff_grant_supersedes_announcement_and_ignores_its_late_refusal() {
    let source = DeviceId([1; 32]);
    let target = DeviceId([2; 32]);
    for os in [Os::MacOS, Os::Windows] {
        let mut s = router(source, target, os);
        let id = notice(source, target, source).key.offer.clip;
        s.dnd_source_learned(id, source, group_metadata());
        let offer = Router::dnd_offer(id);
        let authority = s.dnd.sources.get_mut(&offer).unwrap();
        authority.confirmed = true;
        let previous = authority.holder;
        let next = Holder {
            device: target,
            epoch: previous.epoch + 1,
        };
        s.out.clear();
        s.dnd_source_action(source, offer, previous, SourceAction::Handoff { next }, None);
        let notices = sent_notices(&s);
        let early = &notices
            .iter()
            .find(|(to, n)| *to == target && n.state == DragState::Announced)
            .unwrap()
            .1;
        let offered = &notices
            .iter()
            .find(|(to, n)| *to == target && matches!(n.state, DragState::Offered { .. }))
            .unwrap()
            .1;
        assert!(early.revision < offered.revision);
        let mut t = router(target, source, os);
        t.on_dnd_notice(source, early.clone(), false, 1);
        t.on_dnd_msg(source, DndMsg::Enter { id, epoch: next.epoch });
        t.holder = target;
        t.epoch = next.epoch;
        t.dnd_focus_entered(source, next.epoch, Point::new(4., 5.));
        t.on_dnd_notice(source, offered.clone(), false, 2);
        assert!(t.out.iter().any(|o| matches!(o, Output::Dnd(DndCmd::Begin { .. }))));
        s.out.clear();
        s.on_dnd_attachment_refused(target, early.key, early.revision);
        assert_eq!(s.dnd.sources[&offer].phase, source::Phase::Held);
        assert!(
            sent_notices(&s).is_empty(),
            "old metadata refusal cannot revoke the later grant"
        );
        s.on_dnd_attachment_refused(target, offered.key, offered.revision);
        assert_eq!(s.dnd.sources[&offer].phase, source::Phase::Cancelling);
    }
}
