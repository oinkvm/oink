use oink_protocol::v1::{
    self, ErrorCode, Message, RawMap, RawValue as V, admission,
    dto::*,
    frame, registry,
    schema::{WireDto, WireValue},
};
use oink_test_support::protocol as support;
fn offers() -> (RawMap, RawMap) {
    let a = support::sample_named("ConnectionOffer");
    let mut b = a.clone();
    let mut boot = support::sample_named("Boot");
    let mut id = support::sample_named("DeviceId");
    id.insert(2, V::Bytes(vec![1; 32]));
    boot.insert(1, V::Map(id));
    b.insert(1, V::Map(boot));
    b.insert(8, V::Bytes(vec![1; 32]));
    (a, b)
}
fn bytes(raw: RawMap) -> Vec<u8> {
    frame::encode(
        &Message::ConnectionOffer(ConnectionOffer::from_raw(raw).unwrap()),
        16384,
    )
    .unwrap()
}
fn add_cap(raw: &mut RawMap, cap: &str, key: u32) {
    let V::Array(caps) = raw.get_mut(&key).unwrap() else {
        panic!()
    };
    caps.push(V::from(cap));
    caps.sort_by_key(|v| match v {
        V::Text(t) => t.clone(),
        _ => panic!(),
    });
}
#[test]
fn baseline_intersection_ignores_future_offers_and_minor_mismatch() {
    let (mut a, mut b) = offers();
    add_cap(&mut a, "oink.layout.projection/1", 4);
    add_cap(&mut a, "vendor.future/1", 4);
    add_cap(&mut b, "vendor.future/1", 4);
    a.insert(3, V::Array(vec![V::Map([(1, V::Uint(1)), (2, V::Uint(3))].into())]));
    a.insert(900, support::synthetic_unknown());
    let ab = bytes(a);
    let bb = bytes(b);
    let selected = admission::negotiate(&ab, &bb, registry::CAPABILITIES, false).unwrap();
    assert_eq!(selected.major().unwrap(), 1);
    assert_eq!(selected.minor().unwrap(), 0);
    assert_eq!(selected.enabled().unwrap().0.len(), 5);
    assert_eq!(selected.pairing_suite().unwrap(), None);
    assert_eq!(selected.offer_hash().unwrap(), admission::offer_hash(&ab, &bb).unwrap());
    let encoded = frame::encode(&Message::Negotiated(selected), 16384).unwrap();
    assert!(
        frame::OpaqueFrame::parse_exact(&encoded, 16384)
            .unwrap()
            .decode()
            .is_ok()
    );
}
#[test]
fn missing_required_features_unsupported_major_and_connection_mismatch_fail_typed() {
    let (mut a, b) = offers();
    add_cap(&mut a, "vendor.future/1", 5);
    assert_eq!(
        admission::negotiate(&bytes(a), &bytes(b), registry::CAPABILITIES, false)
            .unwrap_err()
            .code,
        ErrorCode::MissingCapability
    );
    let (a, mut b) = offers();
    b.insert(3, V::Array(vec![V::Map([(1, V::Uint(2)), (2, V::Uint(0))].into())]));
    assert_eq!(
        admission::negotiate(&bytes(a), &bytes(b), registry::CAPABILITIES, false)
            .unwrap_err()
            .code,
        ErrorCode::UnsupportedMajor
    );
    let (a, mut b) = offers();
    let V::Map(id) = b.get_mut(&2).unwrap() else { panic!() };
    id.insert(2, V::Bytes(vec![3; 16]));
    assert_eq!(
        admission::negotiate(&bytes(a), &bytes(b), registry::CAPABILITIES, false)
            .unwrap_err()
            .code,
        ErrorCode::NegotiationMismatch
    );
    assert!(admission::Agreement::new(["vendor.future/1".into()]).is_err());
}
#[test]
fn exact_transcript_binds_unknown_fields_nonce_capabilities_and_identity() {
    let (a, b) = offers();
    let baseline = admission::offer_hash(&bytes(a.clone()), &bytes(b.clone())).unwrap();
    for key in [8, 900] {
        let mut changed = a.clone();
        changed.insert(
            key,
            if key == 8 {
                V::Bytes(vec![9; 32])
            } else {
                support::synthetic_unknown()
            },
        );
        assert_ne!(
            baseline,
            admission::offer_hash(&bytes(changed), &bytes(b.clone())).unwrap()
        );
    }
    let mut changed = a.clone();
    add_cap(&mut changed, "oink.aim/1", 4);
    assert_ne!(
        baseline,
        admission::offer_hash(&bytes(changed), &bytes(b.clone())).unwrap()
    );
    assert_ne!(baseline, admission::offer_hash(&bytes(b), &bytes(a)).unwrap());
}
#[test]
fn relay_does_not_intersect_endpoint_features_and_pairing_suite_is_explicit() {
    let (mut a, mut b) = offers();
    for offer in [&mut a, &mut b] {
        add_cap(offer, "oink.layout.projection/1", 4);
        add_cap(offer, "oink.pairing/1", 4);
    }
    let mut relay = support::sample_named("RelayContext");
    relay.insert(
        4,
        match &b[&1] {
            V::Map(m) => V::Map(m.clone()),
            _ => panic!(),
        },
    );
    a.insert(12, V::Map(relay.clone()));
    b.insert(12, V::Map(relay));
    let selected = admission::negotiate(&bytes(a.clone()), &bytes(b.clone()), registry::CAPABILITIES, true).unwrap();
    assert!(
        selected
            .enabled()
            .unwrap()
            .0
            .iter()
            .any(|c| c.0 == "oink.layout.projection/1")
    );
    assert_eq!(selected.pairing_suite().unwrap().unwrap().0, PAIRING_SUITE);
    let V::Map(crypto) = b.get_mut(&7).unwrap() else {
        panic!()
    };
    crypto.insert(1, V::Array(vec![V::from("future-suite/2")]));
    assert_eq!(
        admission::negotiate(&bytes(a), &bytes(b), registry::CAPABILITIES, true)
            .unwrap_err()
            .code,
        ErrorCode::UnsupportedValue
    );
}
#[test]
fn maximum_audio_packet_with_full_width_counters_stays_under_datagram_ceiling() {
    let mut raw = support::sample_named("AudioPacket");
    for key in 1..=4 {
        raw.insert(key, V::Uint(u64::MAX));
    }
    raw.insert(6, V::Bytes(vec![0; 960]));
    let packet = AudioPacket::from_raw(raw).unwrap();
    let datagram = frame::encode_datagram(u32::MAX, &Message::AudioPacket(packet)).unwrap();
    assert!(datagram.len() <= 1023, "{}", datagram.len());
    assert_eq!(v1::limits::DATAGRAM, 1100);
}
#[test]
fn pair_objects_and_human_wait_contract_have_no_ttl_fields() {
    let mut object = support::sample_named("ObjectRef");
    object.insert(2, V::Uint(1_048_577));
    assert!(ObjectRef::from_raw(object).is_err());
    let mut pake = support::sample_named("PairPake");
    pake.insert(4, V::Uint(4));
    assert!(PairPake::from_raw(pake).is_err());
    for schema in registry::SCHEMAS.iter().filter(|s| s.name.starts_with("Pair")) {
        assert!(
            !schema
                .fields
                .iter()
                .any(|f| f.name.contains("expires") || f.name == "ttl")
        );
    }
    assert_eq!(PairCode::TooManyPendingRequests.number(), 46);
    assert_eq!(v1::limits::PENDING_PAIRS, 3);
    let problem = PairProblem::from_raw(
        [
            (1, PairCode::TooManyPendingRequests.write()),
            (2, PairStage::Request.write()),
            (6, 4u32.write()),
            (7, 3u32.write()),
            (9, PairLimitKind::PendingRequests.write()),
        ]
        .into(),
    )
    .unwrap();
    assert_eq!(problem.limit().unwrap(), Some(3));
}

#[test]
fn notification_capability_is_optional_and_only_new_peers_enable_it() {
    for new_peer in [false, true] {
        let (mut a, mut b) = offers();
        add_cap(&mut a, "oink.notify/1", 4);
        if new_peer {
            add_cap(&mut b, "oink.notify/1", 4);
        }
        let negotiated = admission::negotiate(&bytes(a), &bytes(b), registry::CAPABILITIES, false).unwrap();
        let enabled = negotiated
            .enabled()
            .unwrap()
            .0
            .into_iter()
            .map(|v| v.0)
            .collect::<Vec<_>>();
        let agreement = admission::Agreement::new(enabled).unwrap();
        assert_eq!(agreement.contains("oink.notify/1"), new_peer);
        // Released offers still negotiate baseline v1 successfully. Admission
        // excludes the new message unless the optional feature was selected.
        let context = admission::Context {
            kind: 2,
            transport: registry::Transport::Reliable,
            phase: admission::Phase::Member,
            agreement: &agreement,
            direct: true,
            old_member_lane: false,
        };
        let message = Message::NotifyDismiss(NotifyDismiss::from_raw([(1, 1u64.write())].into()).unwrap());
        let encoded = frame::encode(&message, 16384).unwrap();
        let result = context.decode(frame::OpaqueFrame::parse_exact(&encoded, 16384).unwrap());
        assert_eq!(result.is_ok(), new_peer);
    }
    assert!(!registry::BASELINE.contains(&"oink.notify/1"));
}

#[test]
fn arrangement_preview_is_selected_per_peer_and_admitted_only_when_negotiated() {
    for both in [false, true] {
        let (mut a, mut b) = offers();
        for offer in [&mut a, &mut b] {
            add_cap(offer, "oink.layout.projection/1", 4);
        }
        add_cap(&mut a, "oink.layout.preview/1", 4);
        if both {
            add_cap(&mut b, "oink.layout.preview/1", 4);
        }
        let selected = admission::negotiate(&bytes(a), &bytes(b), registry::CAPABILITIES, false).unwrap();
        let agreement = admission::Agreement::new(selected.enabled().unwrap().0.into_iter().map(|s| s.0)).unwrap();
        assert_eq!(agreement.contains("oink.layout.preview/1"), both);
        let m = Message::ArrangementPreview(
            ArrangementPreview::from_raw(support::sample_named("ArrangementPreview")).unwrap(),
        );
        let bytes = frame::encode(&m, v1::limits::RELIABLE_FRAME).unwrap();
        let context = admission::Context {
            kind: 12,
            transport: registry::Transport::Reliable,
            phase: admission::Phase::Member,
            agreement: &agreement,
            direct: false,
            old_member_lane: false,
        };
        let result = context.decode(frame::OpaqueFrame::parse_exact(&bytes, v1::limits::RELIABLE_FRAME).unwrap());
        if both {
            assert!(result.is_ok());
        } else {
            assert_eq!(result.unwrap_err().code, ErrorCode::UnnegotiatedCapability);
        }
    }
    let mut raw = support::sample_named("ArrangementPreview");
    raw.insert(1, V::Bool(false));
    assert!(ArrangementPreview::from_raw(raw.clone()).is_err());
    raw.insert(1, V::Bool(true));
    raw.insert(3, V::Uint(2001));
    assert!(ArrangementPreview::from_raw(raw).is_err());
}

#[test]
fn profile_v2_catalog_is_gated_and_v1_keeps_its_eight_profile_bound() {
    let mut catalog = support::sample_named("LayoutProfileCatalogV2");
    let profile = V::Map(support::sample_named("LayoutProfile"));
    catalog.insert(5, V::Array(vec![profile.clone(); 16]));
    assert!(LayoutProfileCatalogV2::from_raw(catalog.clone()).is_ok());
    assert!(LayoutProfileCatalog::from_raw(catalog.clone()).is_err());
    catalog.insert(5, V::Array(vec![profile.clone(); 8]));
    assert!(LayoutProfileCatalog::from_raw(catalog.clone()).is_ok());
    catalog.insert(5, V::Array(vec![profile; 17]));
    assert!(LayoutProfileCatalogV2::from_raw(catalog).is_err());
    for both in [false, true] {
        let (mut a, mut b) = offers();
        add_cap(&mut a, "oink.layout.profiles/2", 4);
        add_cap(&mut b, "oink.layout.profiles/1", 4);
        if both {
            add_cap(&mut b, "oink.layout.profiles/2", 4);
        }
        let selected = admission::negotiate(&bytes(a), &bytes(b), registry::CAPABILITIES, false).unwrap();
        let agreement = admission::Agreement::new(selected.enabled().unwrap().0.into_iter().map(|s| s.0)).unwrap();
        assert_eq!(agreement.contains("oink.layout.profiles/2"), both);
        let message = Message::LayoutProfilesStateV2(
            LayoutProfilesStateV2::from_raw(support::sample_named("LayoutProfilesStateV2")).unwrap(),
        );
        let encoded = frame::encode(&message, v1::limits::LOW_FRAME).unwrap();
        let context = admission::Context {
            kind: 14,
            transport: registry::Transport::Low,
            phase: admission::Phase::Member,
            agreement: &agreement,
            direct: false,
            old_member_lane: false,
        };
        assert_eq!(
            context
                .decode(frame::OpaqueFrame::parse_exact(&encoded, v1::limits::LOW_FRAME).unwrap())
                .is_ok(),
            both
        );
    }
}

#[test]
fn acceptor_profile_v3_messages_require_v3_on_both_endpoints() {
    for both in [false, true] {
        let (mut a, mut b) = offers();
        add_cap(&mut a, "oink.layout.profiles/3", 4);
        add_cap(&mut b, "oink.layout.profiles/2", 4);
        if both {
            add_cap(&mut b, "oink.layout.profiles/3", 4);
        }
        let selected = admission::negotiate(&bytes(a), &bytes(b), registry::CAPABILITIES, false).unwrap();
        let agreement = admission::Agreement::new(selected.enabled().unwrap().0.into_iter().map(|s| s.0)).unwrap();
        assert_eq!(agreement.contains("oink.layout.profiles/3"), both);
        assert!(!agreement.contains("oink.layout.profiles/2"));
        for message in [
            Message::LayoutProfilesStateV3(
                LayoutProfilesStateV3::from_raw(support::sample_named("LayoutProfilesStateV3")).unwrap(),
            ),
            Message::LayoutProfilesJoin(
                LayoutProfilesJoin::from_raw(support::sample_named("LayoutProfilesJoin")).unwrap(),
            ),
            Message::LayoutProfilesReceiptV3(
                LayoutProfilesReceipt::from_raw(support::sample(LayoutProfilesReceipt::SCHEMA)).unwrap(),
            ),
        ] {
            let encoded = frame::encode(&message, v1::limits::LOW_FRAME).unwrap();
            let context = admission::Context {
                kind: 15,
                transport: registry::Transport::Low,
                phase: admission::Phase::Member,
                agreement: &agreement,
                direct: false,
                old_member_lane: false,
            };
            assert_eq!(
                context
                    .decode(frame::OpaqueFrame::parse_exact(&encoded, v1::limits::LOW_FRAME).unwrap())
                    .is_ok(),
                both
            );
        }
    }
}

#[test]
fn pointer_move_is_optional_and_old_peers_keep_baseline_focus() {
    for new_peer in [false, true] {
        let (mut a, mut b) = offers();
        add_cap(&mut a, "oink.pointer.move/1", 4);
        if new_peer {
            add_cap(&mut b, "oink.pointer.move/1", 4);
        }
        let negotiated = admission::negotiate(&bytes(a), &bytes(b), registry::CAPABILITIES, false).unwrap();
        let agreement = admission::Agreement::new(negotiated.enabled().unwrap().0.into_iter().map(|v| v.0)).unwrap();
        let context = admission::Context {
            kind: 3,
            transport: registry::Transport::Reliable,
            phase: admission::Phase::Member,
            agreement: &agreement,
            direct: true,
            old_member_lane: false,
        };
        let message = Message::ControlMovePointer(
            ControlMovePointer::from_raw(support::sample_named("ControlMovePointer")).unwrap(),
        );
        let encoded = frame::encode(&message, 16384).unwrap();
        assert_eq!(
            context
                .decode(frame::OpaqueFrame::parse_exact(&encoded, 16384).unwrap())
                .is_ok(),
            new_peer
        );
        let baseline = Message::ControlFocusEnter(
            ControlFocusEnter::from_raw(support::sample_named("ControlFocusEnter")).unwrap(),
        );
        let encoded = frame::encode(&baseline, 16384).unwrap();
        assert!(
            context
                .decode(frame::OpaqueFrame::parse_exact(&encoded, 16384).unwrap())
                .is_ok()
        );
    }
    assert!(!registry::BASELINE.contains(&"oink.pointer.move/1"));
}

#[test]
fn lock_escape_is_optional_and_requires_both_peers() {
    for new_peer in [false, true] {
        let (mut a, mut b) = offers();
        add_cap(&mut a, "oink.pointer.lock-escape/1", 4);
        if new_peer {
            add_cap(&mut b, "oink.pointer.lock-escape/1", 4);
        }
        let negotiated = admission::negotiate(&bytes(a), &bytes(b), registry::CAPABILITIES, false).unwrap();
        let agreement = admission::Agreement::new(negotiated.enabled().unwrap().0.into_iter().map(|v| v.0)).unwrap();
        let context = admission::Context {
            kind: 3,
            transport: registry::Transport::Reliable,
            phase: admission::Phase::Member,
            agreement: &agreement,
            direct: true,
            old_member_lane: false,
        };
        let message = Message::ControlLockEscape(
            ControlLockEscape::from_raw(support::sample_named("ControlLockEscape")).unwrap(),
        );
        let encoded = frame::encode(&message, 16384).unwrap();
        assert_eq!(
            context
                .decode(frame::OpaqueFrame::parse_exact(&encoded, 16384).unwrap())
                .is_ok(),
            new_peer
        );
    }
    assert!(!registry::BASELINE.contains(&"oink.pointer.lock-escape/1"));
}

#[test]
fn keymap_none_capability_is_optional_and_requires_both_peers() {
    for modern in [false, true] {
        let (mut a, mut b) = offers();
        add_cap(&mut a, "oink.input.keymap-none/1", 4);
        if modern {
            add_cap(&mut b, "oink.input.keymap-none/1", 4);
        }
        let negotiated = admission::negotiate(&bytes(a), &bytes(b), registry::CAPABILITIES, false).unwrap();
        let agreement = admission::Agreement::new(negotiated.enabled().unwrap().0.into_iter().map(|c| c.0)).unwrap();
        assert_eq!(agreement.contains("oink.input.keymap-none/1"), modern);
        assert!(agreement.contains("oink.core/1"));
    }
    assert!(!registry::BASELINE.contains(&"oink.input.keymap-none/1"));
}

#[test]
fn entry_protection_is_optional_and_requires_both_peers() {
    for new_peer in [false, true] {
        let (mut a, mut b) = offers();
        add_cap(&mut a, "oink.pointer.entry-protection/1", 4);
        if new_peer {
            add_cap(&mut b, "oink.pointer.entry-protection/1", 4);
        }
        let negotiated = admission::negotiate(&bytes(a), &bytes(b), registry::CAPABILITIES, false).unwrap();
        let agreement = admission::Agreement::new(negotiated.enabled().unwrap().0.into_iter().map(|v| v.0)).unwrap();
        let context = admission::Context {
            kind: 3,
            transport: registry::Transport::Reliable,
            phase: admission::Phase::Member,
            agreement: &agreement,
            direct: true,
            old_member_lane: false,
        };
        let message = Message::ControlEntryProtection(
            ControlEntryProtection::from_raw(support::sample_named("ControlEntryProtection")).unwrap(),
        );
        let encoded = frame::encode(&message, 16384).unwrap();
        assert_eq!(
            context
                .decode(frame::OpaqueFrame::parse_exact(&encoded, 16384).unwrap())
                .is_ok(),
            new_peer
        );
    }
    assert!(!registry::BASELINE.contains(&"oink.pointer.entry-protection/1"));
}

#[test]
fn power_is_optional_and_never_admitted_by_released_peer() {
    for supported in [false, true] {
        let (mut a, mut b) = offers();
        add_cap(&mut a, "oink.power/1", 4);
        if supported {
            add_cap(&mut b, "oink.power/1", 4);
        }
        let negotiated = admission::negotiate(&bytes(a), &bytes(b), registry::CAPABILITIES, false).unwrap();
        let agreement = admission::Agreement::new(negotiated.enabled().unwrap().0.into_iter().map(|v| v.0)).unwrap();
        let context = admission::Context {
            kind: 2,
            transport: registry::Transport::Reliable,
            phase: admission::Phase::Member,
            agreement: &agreement,
            direct: true,
            old_member_lane: false,
        };
        let raw = [
            (1, true.write()),
            (2, false.write()),
            (3, true.write()),
            (4, false.write()),
        ]
        .into();
        let message = Message::ControlPowerState(ControlPowerState::from_raw(raw).unwrap());
        let encoded = frame::encode(&message, 16384).unwrap();
        assert_eq!(
            context
                .decode(frame::OpaqueFrame::parse_exact(&encoded, 16384).unwrap())
                .is_ok(),
            supported
        );
    }
    assert!(!registry::BASELINE.contains(&"oink.power/1"));
    assert!(
        ControlPowerState::from_raw(
            [
                (1, false.write()),
                (2, true.write()),
                (3, false.write()),
                (4, false.write())
            ]
            .into()
        )
        .is_err()
    );
}

#[test]
fn microphone_notifications_negotiate_independently_and_preserve_legacy_payloads() {
    let capability = "oink.notify.microphone/1";
    assert!(!registry::BASELINE.contains(&capability));
    for supported in [false, true] {
        let (mut a, mut b) = offers();
        for offer in [&mut a, &mut b] {
            add_cap(offer, "oink.notify/1", 4);
        }
        add_cap(&mut a, capability, 4);
        if supported {
            add_cap(&mut b, capability, 4);
        }
        let selected = admission::negotiate(&bytes(a), &bytes(b), registry::CAPABILITIES, false).unwrap();
        assert_eq!(
            selected.enabled().unwrap().0.iter().any(|cap| cap.0 == capability),
            supported
        );
    }
    let legacy = support::sample_named("NotifyShow");
    assert_eq!(
        NotifyShow::from_raw(legacy.clone())
            .unwrap()
            .microphone_muted()
            .unwrap(),
        None
    );
    for muted in [true, false] {
        let mut raw = legacy.clone();
        raw.insert(9, muted.write());
        let notice = NotifyShow::from_raw(raw).unwrap();
        let decoded = NotifyShow::decode(&notice.encode(16384).unwrap(), 16384).unwrap();
        assert_eq!(decoded.microphone_muted().unwrap(), Some(muted));
        assert_eq!(decoded.kind().unwrap(), notice.kind().unwrap());
    }
}
