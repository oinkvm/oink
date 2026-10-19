use oink_protocol::v1::{
    self, ErrorCode, RawMap, RawValue as V,
    dto::*,
    retained::*,
    schema::{WireDto, WireValue},
};
use oink_test_support::protocol as support;
fn register(kind: u32) -> Register {
    let mut raw = support::sample_named("Register");
    raw.insert(1, V::Uint(kind.into()));
    let mut layout = support::sample_named("LayoutValue");
    let mut screen = support::sample_named("ScreenPlacement");
    screen.insert(900, support::synthetic_unknown());
    let mut group = support::sample_named("DevicePlacement");
    group.insert(4, V::Array(vec![V::Map(screen)]));
    layout.insert(1, V::Array(vec![V::Map(group)]));
    layout.insert(13, support::synthetic_unknown());
    layout.insert(
        4,
        V::Map([(12, V::F64(8.0 / 686.0)), (900, support::synthetic_unknown())].into()),
    );
    raw.insert(4, V::Map(layout));
    raw.insert(900, support::synthetic_unknown());
    raw.insert(6, register_digest(&raw).unwrap().write());
    Register::from_raw(raw).unwrap()
}
#[test]
fn future_register_values_survive_merge_edit_persist_restart_and_send() {
    let old = register(2);
    let unknown_envelope = old.raw()[&900].clone();
    let unknown_value = old.value().unwrap()[&13].clone();
    let array = old.value().unwrap()[&1].clone();
    let writer = DeviceId::from_raw(support::sample_named("DeviceId")).unwrap();
    let edited = edit_register(&old, &old.version().unwrap(), writer, |value| {
        patch_path(value, &[4, 10], Some(V::Bool(false)))
    })
    .unwrap();
    verify_register(&edited).unwrap();
    let merged = merge_register(&old, &edited).unwrap();
    assert_eq!(merged, edited);
    let disk = merged.encode(v1::limits::CONFIG_STATE).unwrap();
    let restarted = Register::from_raw(v1::cbor::decode_map(&disk, v1::limits::CONFIG_STATE).unwrap()).unwrap();
    assert_eq!(restarted.raw()[&900], unknown_envelope);
    assert_eq!(restarted.value().unwrap()[&13], unknown_value);
    assert_eq!(restarted.value().unwrap()[&1], array);
    assert_eq!(
        restarted.value().unwrap()[&4].as_map().unwrap()[&900],
        support::synthetic_unknown()
    );
    assert!(!restarted.version().unwrap().raw().contains_key(&2));
    assert!(!restarted.raw().contains_key(&2));
    assert_eq!(restarted.encode(v1::limits::CONFIG_STATE).unwrap(), disk);
    let future = register(400);
    assert_eq!(merge_register(&future, &future).unwrap(), future);
}
#[test]
fn equal_version_conflict_stale_edits_tombstone_and_counter_exhaustion_are_safe() {
    let old = register(2);
    let mut conflict = old.raw().clone();
    let mut value = old.value().unwrap();
    patch_path(&mut value, &[4, 10], Some(V::Bool(false))).unwrap();
    conflict.insert(4, V::Map(value));
    conflict.insert(6, register_digest(&conflict).unwrap().write());
    assert_eq!(
        merge_register(&old, &Register::from_raw(conflict).unwrap())
            .unwrap_err()
            .code,
        ErrorCode::StateConflict
    );
    let writer = DeviceId::from_raw(support::sample_named("DeviceId")).unwrap();
    let changed = edit_register(&old, &old.version().unwrap(), writer.clone(), |v| {
        patch_path(v, &[4, 10], Some(V::Bool(false)))
    })
    .unwrap();
    assert!(edit_register(&changed, &old.version().unwrap(), writer.clone(), |_| Ok(())).is_err());
    assert!(
        edit_register(&old, &old.version().unwrap(), writer, |v| {
            v.insert(900, V::Bytes(vec![0; v1::limits::CONFIG_STATE]));
            Ok(())
        })
        .is_err()
    );
    let future = register(400);
    let changed_future = edit_register(
        &future,
        &future.version().unwrap(),
        future.version().unwrap().writer().unwrap(),
        |_| Ok(()),
    )
    .unwrap();
    let mut tomb = changed_future.raw().clone();
    tomb.insert(4, V::Map(RawMap::new()));
    tomb.insert(5, V::Bool(true));
    tomb.insert(6, register_digest(&tomb).unwrap().write());
    let tomb = Register::from_raw(tomb).unwrap();
    assert!(merge_register(&future, &tomb).unwrap().deleted().unwrap());
    let mut max = old.raw().clone();
    let V::Map(version) = max.get_mut(&3).unwrap() else {
        panic!()
    };
    version.insert(1, V::Uint(u64::MAX));
    max.insert(6, register_digest(&max).unwrap().write());
    let max = Register::from_raw(max).unwrap();
    assert_eq!(
        edit_register(
            &max,
            &max.version().unwrap(),
            max.version().unwrap().writer().unwrap(),
            |_| Ok(())
        )
        .unwrap_err()
        .code,
        ErrorCode::LimitExceeded
    );
}
#[test]
fn unversioned_extensions_merge_transactionally_and_signed_bytes_are_not_projected() {
    let a: RawMap = [(900, V::Uint(1))].into();
    let b: RawMap = [(901, V::Uint(2))].into();
    assert_eq!(merge_extensions(&a, &b, &[1, 2, 3]).unwrap().len(), 2);
    assert!(merge_extensions(&a, &[(900, V::Uint(2))].into(), &[1, 2, 3]).is_err());
    assert_eq!(a.len(), 1);
    let record = SignedRecord::from_raw(support::sample_named("SignedRecord")).unwrap();
    let before = record_id(&record).unwrap();
    let mut raw = record.into_raw();
    let V::Map(body) = raw.get_mut(&1).unwrap() else {
        panic!()
    };
    body.insert(900, support::synthetic_unknown());
    let retained = SignedRecord::from_raw(raw).unwrap();
    assert_ne!(record_id(&retained).unwrap(), before);
    let signature = record_signature_input(&retained).unwrap();
    assert!(signature.starts_with(b"oink/v1/roster/signature\0"));
    assert!(signature.ends_with(&V::Map(retained.body().unwrap()).encode(122880).unwrap()));
    assert!(!Grant::SCHEMA.fields.iter().any(|f| f.key == 2));
    assert!(!Request::SCHEMA.fields.iter().any(|f| f.key == 3));
    assert!(!Reply::SCHEMA.fields.iter().any(|f| f.key == 3));
}
