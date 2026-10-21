use oink_protocol::v1::{self, RawMap, RawValue as V, dto::*, schema::WireDto};
use oink_test_support::protocol as support;

#[test]
fn all_registered_diagnostic_bodies_have_recursive_schemas() {
    for kind in v1::registry::DIAGNOSTIC_EVENTS {
        let schema = diagnostic_event_schema(kind).expect(kind);
        let body = support::sample(schema);
        let raw = RawMap::from([(1, V::from(*kind)), (2, V::Map(body))]);
        DiagnosticEvent::from_raw(raw).unwrap_or_else(|e| panic!("{kind}: {e}"));
    }
}

#[test]
fn classified_input_rejects_wrong_known_values_and_retains_unknown_nested_values() {
    let mut key = support::sample_named("DiagKey");
    key.insert(1, V::from("PRIVATE KEY TEXT"));
    assert!(DiagKey::from_raw(key).is_err());
    let mut key = support::sample_named("DiagKey");
    key.insert(900, support::synthetic_unknown());
    let mut observation = support::sample_named("DiagKeyObservation");
    observation.insert(900, support::synthetic_unknown());
    key.insert(4, V::Map(observation));
    let event = DiagnosticEvent::from_raw(RawMap::from([
        (1, V::from("input/1")),
        (
            2,
            V::Map({
                let mut input = support::sample_named("DiagInput");
                input.insert(5, V::Map(RawMap::from([(1, V::Uint(5)), (2, V::Map(key))])));
                input
            }),
        ),
    ]))
    .unwrap();
    let bytes = event.encode(v1::limits::DIAGNOSTIC_SEGMENT).unwrap();
    let raw = v1::cbor::decode(&bytes, v1::limits::DIAGNOSTIC_SEGMENT).unwrap();
    let decoded = DiagnosticEvent::from_raw(raw.as_map().unwrap().clone()).unwrap();
    assert_eq!(decoded, event);
    assert_eq!(decoded.encode(v1::limits::DIAGNOSTIC_SEGMENT).unwrap(), bytes);
}

#[test]
fn unknown_event_and_snapshot_extensions_survive_without_reconstruction() {
    let unknown = RawMap::from([
        (1, V::from("future-observation/7")),
        (2, V::Map(RawMap::from([(900, support::synthetic_unknown())]))),
        (901, V::Bytes(vec![1, 2, 3])),
    ]);
    let event = DiagnosticEvent::from_raw(unknown.clone()).unwrap();
    assert_eq!(event.into_raw(), unknown);
    let mut snapshot = support::sample_named("Snapshot");
    snapshot.insert(901, support::synthetic_unknown());
    let decoded = Snapshot::from_raw(snapshot.clone()).unwrap();
    let bytes = decoded.encode(v1::limits::DIAGNOSTIC_SEGMENT).unwrap();
    assert_eq!(v1::cbor::decode(&bytes, bytes.len()).unwrap(), V::Map(snapshot));
}

#[test]
fn metrics_are_sorted_scalar_entries_and_cannot_hide_a_nested_payload() {
    let scalar = |name: &str, value| V::Map(RawMap::from([(1, V::from(name)), (2, value)]));
    let mut snapshot = support::sample_named("Snapshot");
    for bad in [
        vec![scalar("a", V::Map(RawMap::new()))],
        vec![scalar("b", V::Bool(true)), scalar("a", V::Bool(false))],
        vec![scalar("a", V::Uint(1)), scalar("a", V::Uint(2))],
    ] {
        snapshot.insert(8, V::Array(bad));
        assert!(Snapshot::from_raw(snapshot.clone()).is_err());
    }
    snapshot.insert(8, V::Array(vec![scalar("a", V::Bool(false)), scalar("b", V::Uint(2))]));
    assert!(Snapshot::from_raw(snapshot).is_ok());
}

#[test]
fn known_event_nested_bounds_are_checked_before_materialization() {
    let mut body = support::sample_named("DiagTrail");
    body.insert(
        12,
        V::Array(vec![V::Map(support::sample_named("DiagTrailPoint")); 8193]),
    );
    let raw = RawMap::from([(1, V::from("trail/1")), (2, V::Map(body))]);
    let bytes = v1::cbor::encode_map(&raw, v1::limits::DIAGNOSTIC_SEGMENT).unwrap();
    let mut decoder = minicbor::Decoder::new(&bytes);
    assert_eq!(
        DiagnosticEvent::preflight(&mut decoder, 0).unwrap_err().code,
        v1::ErrorCode::LimitExceeded
    );
    assert!(decoder.position() < bytes.len() / 2, "reject at the array header");
    let mut future = raw;
    future.insert(1, V::from("future-observation/7"));
    let bytes = v1::cbor::encode_map(&future, v1::limits::DIAGNOSTIC_SEGMENT).unwrap();
    DiagnosticEvent::preflight(&mut minicbor::Decoder::new(&bytes), 0).unwrap();
    assert_eq!(DiagnosticEvent::from_raw(future.clone()).unwrap().into_raw(), future);
}

#[test]
fn standalone_decode_checks_budget_complete_input_and_nested_limits() {
    let mut raw = support::sample_named("DiagnosticSegment");
    raw.insert(901, support::synthetic_unknown());
    let segment = DiagnosticSegment::from_raw(raw.clone()).unwrap();
    let bytes = segment.encode(v1::limits::DIAGNOSTIC_SEGMENT).unwrap();
    assert_eq!(DiagnosticSegment::decode(&bytes, bytes.len()).unwrap().into_raw(), raw);
    assert_eq!(
        DiagnosticSegment::decode(&bytes, bytes.len() - 1).unwrap_err().code,
        v1::ErrorCode::LimitExceeded
    );
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert_eq!(
        DiagnosticSegment::decode(&trailing, trailing.len()).unwrap_err().code,
        v1::ErrorCode::MalformedFrame
    );

    let mut trail = support::sample_named("DiagTrail");
    trail.insert(
        12,
        V::Array(vec![V::Map(support::sample_named("DiagTrailPoint")); 8193]),
    );
    let event = RawMap::from([(1, V::from("trail/1")), (2, V::Map(trail))]);
    let bytes = v1::cbor::encode_map(&event, v1::limits::DIAGNOSTIC_SEGMENT).unwrap();
    assert_eq!(
        DiagnosticEvent::decode(&bytes, bytes.len()).unwrap_err().code,
        v1::ErrorCode::LimitExceeded
    );
}

#[test]
fn gesture_foreground_observations_are_optional_and_new_decisions_roundtrip() {
    let mut baseline = support::sample_named("DiagGestureSummary");
    baseline.remove(&18);
    baseline.remove(&19);
    let old = DiagGestureSummary::from_raw(baseline.clone()).unwrap();
    assert_eq!(old.desktop_showing().unwrap(), None);
    assert_eq!(old.start_open().unwrap(), None);
    for decision in [
        DiagGestureDecision::ShowDesktop,
        DiagGestureDecision::RestoreDesktop,
        DiagGestureDecision::DesktopNoop,
        DiagGestureDecision::StartOpen,
        DiagGestureDecision::StartClose,
        DiagGestureDecision::StartNoop,
    ] {
        let mut row = baseline.clone();
        row.insert(15, oink_protocol::v1::schema::WireValue::write(&decision));
        row.insert(18, V::Bool(true));
        row.insert(19, V::Bool(false));
        row.insert(900, support::synthetic_unknown());
        let summary = DiagGestureSummary::from_raw(row.clone()).unwrap();
        assert_eq!(summary.decision().unwrap(), decision);
        assert_eq!(summary.desktop_showing().unwrap(), Some(true));
        assert_eq!(summary.start_open().unwrap(), Some(false));
        let bytes = summary.encode(v1::limits::DIAGNOSTIC_SEGMENT).unwrap();
        assert_eq!(v1::cbor::decode(&bytes, bytes.len()).unwrap(), V::Map(row));
    }
}
