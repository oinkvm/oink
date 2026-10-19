use oink_protocol::v1::{self, RawValue, cbor, frame, registry};
use oink_test_support::protocol as support;
use serde_json::Value;
#[test]
fn every_checked_in_vector_decodes_and_reencodes_identically() {
    let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/v1");
    let manifest: Value = serde_json::from_slice(&std::fs::read(directory.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["schema_sha256"], support::schema_hash());
    let listed: std::collections::BTreeSet<_> = manifest["fixtures"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["file"].as_str().unwrap().to_owned())
        .collect();
    let present: std::collections::BTreeSet<_> = std::fs::read_dir(&directory)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|s| s == "hex"))
        .map(|p| p.file_name().unwrap().to_str().unwrap().to_owned())
        .collect();
    assert_eq!(listed, present, "every checked-in hex vector must be exercised");
    let mut messages = std::collections::BTreeSet::new();
    for fixture in manifest["fixtures"].as_array().unwrap() {
        let file = fixture["file"].as_str().unwrap();
        let bytes = support::unhex(std::fs::read_to_string(directory.join(file)).unwrap().trim());
        let (actual, encoded) = match fixture["kind"].as_str().unwrap() {
            "frame" => {
                let parsed = frame::OpaqueFrame::parse_exact(&bytes, v1::limits::LOW_FRAME).unwrap();
                assert_eq!(fixture["type_id"], parsed.header.type_id);
                messages.insert(parsed.header.type_id);
                let message = parsed.decode().unwrap_or_else(|e| panic!("{file}: {e}"));
                (
                    support::describe(&RawValue::Map(message.raw().clone())),
                    frame::encode(&message, v1::limits::LOW_FRAME).unwrap(),
                )
            }
            "dto" | "enum" | "open" => {
                let name = fixture["schema"].as_str().unwrap();
                if fixture["kind"] == "dto" {
                    let mut decoder = minicbor::Decoder::new(&bytes);
                    registry::preflight_schema(name, &mut decoder, 0).unwrap_or_else(|e| panic!("{file}: {e}"));
                    assert_eq!(decoder.position(), bytes.len(), "{file}");
                }
                let raw = cbor::decode(&bytes, v1::limits::LOW_FRAME).unwrap();
                if fixture["kind"] == "dto" {
                    registry::validate_schema(name, raw.as_map().unwrap()).unwrap_or_else(|e| panic!("{file}: {e}"));
                } else if fixture["kind"] == "open" {
                    registry::validate_open(name, raw.as_map().unwrap()).unwrap();
                } else {
                    registry::validate_enum(name, &raw).unwrap();
                }
                (support::describe(&raw), raw.encode(v1::limits::LOW_FRAME).unwrap())
            }
            "stream" => {
                let (header, len) = frame::StreamHeader::parse(&bytes).unwrap().unwrap();
                assert_eq!(len, bytes.len());
                (
                    serde_json::json!({"kind":header.kind,"priority":header.priority,"route_tag":header.route_tag,"ordinal":header.ordinal,"flow":header.datagram_flow}),
                    header.encode().unwrap().to_vec(),
                )
            }
            "discovery" => {
                let m = frame::discovery(&bytes).unwrap();
                (
                    support::describe(&RawValue::Map(m.raw().clone())),
                    frame::encode_discovery(&m).unwrap(),
                )
            }
            "datagram" => {
                let frame::Datagram::Active { flow, frame } = frame::datagram(&bytes, |flow| flow == 7).unwrap() else {
                    panic!("registered flow")
                };
                assert_eq!(fixture["flow"], flow);
                let m = frame.decode().unwrap();
                (
                    support::describe(&RawValue::Map(m.raw().clone())),
                    frame::encode_datagram(flow, &m).unwrap(),
                )
            }
            kind => panic!("unknown fixture kind {kind}"),
        };
        assert_eq!(actual, fixture["expected"], "{file}");
        assert_eq!(encoded, bytes, "{file}");
    }
    assert_eq!(messages, registry::MESSAGES.iter().map(|m| m.id).collect());
}
#[test]
fn schema_export_matches_reviewed_snapshot_and_allocations_are_unique() {
    let saved: Value = serde_json::from_str(include_str!("fixtures/v1/schema.json")).unwrap();
    assert_eq!(saved, support::schema_json());
    let mut ids = std::collections::BTreeSet::new();
    for entry in registry::MESSAGES {
        assert!(ids.insert(entry.id));
        assert!(!registry::TOMBSTONES.contains(&entry.id));
        assert!(!registry::experimental_message(entry.id));
        for cap in entry.capabilities {
            assert!(registry::CAPABILITIES.contains(cap));
        }
    }
    for schema in registry::SCHEMAS {
        assert!(schema.fields.windows(2).all(|w| w[0].key < w[1].key), "{}", schema.name);
        assert!(
            schema
                .fields
                .iter()
                .all(|f| f.key != 0 && f.key != u32::MAX && !f.absence.is_empty())
        );
    }
    for (_, values) in registry::ENUMS {
        let unique: std::collections::BTreeSet<_> = values.iter().map(|v| v.0).collect();
        assert_eq!(unique.len(), values.len());
    }
}

#[test]
fn cryptographic_labels_bind_exact_golden_bytes() {
    use v1::{dto::SignedRecord, retained, schema::WireDto};
    let crypto: Value = serde_json::from_str(include_str!("fixtures/v1/crypto.json")).unwrap();
    let bytes = |key: &str| support::unhex(crypto[key].as_str().unwrap());
    let offer = bytes("offer_frame");
    assert_eq!(
        v1::admission::offer_hash(&offer, &offer).unwrap().0.as_slice(),
        bytes("offer_hash")
    );
    let record =
        SignedRecord::from_raw(cbor::decode_map(&bytes("signed_record"), v1::limits::CONFIG_STATE).unwrap()).unwrap();
    assert_eq!(retained::record_id(&record).unwrap().0.as_slice(), bytes("record_id"));
    let signature_input = retained::record_signature_input(&record).unwrap();
    assert_eq!(signature_input, bytes("signature_input"));
    assert_eq!(
        retained::sha256(&signature_input).0.as_slice(),
        bytes("signature_input_sha256")
    );
}
