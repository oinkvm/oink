use oink_protocol::v1::{
    self, RawMap, RawValue, registry,
    schema::{Schema, WireDto, WireValue},
};
use serde_json::{Value, json};

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
pub fn unhex(text: &str) -> Vec<u8> {
    assert_eq!(text.len() % 2, 0);
    text.as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| u8::from_str_radix(std::str::from_utf8(b).unwrap(), 16).unwrap())
        .collect()
}
pub fn describe(value: &RawValue) -> Value {
    match value {
        RawValue::Uint(n) => json!({"uint":n.to_string()}),
        RawValue::Negative(n) => json!({"negative_argument":n.to_string()}),
        RawValue::Bytes(b) => json!({"bytes":hex(b)}),
        RawValue::Text(s) => json!({"text":s}),
        RawValue::Bool(b) => json!({"bool":b}),
        RawValue::Null => Value::Null,
        RawValue::F32(n) => json!({"f32_bits":format!("{:08x}",n.to_bits())}),
        RawValue::F64(n) => json!({"f64_bits":format!("{:016x}",n.to_bits())}),
        RawValue::Array(a) => json!({"list":a.iter().map(describe).collect::<Vec<_>>()}),
        RawValue::Map(m) => {
            json!({"map":m.iter().map(|(k,v)|(k.to_string(),describe(v))).collect::<serde_json::Map<_,_>>()})
        }
    }
}
pub fn schema_json() -> Value {
    serde_json::to_value(registry::schema_registry()).unwrap()
}
pub fn schema_hash() -> String {
    hex(&v1::retained::sha256(&serde_json::to_vec(&schema_json()).unwrap()).0)
}
pub fn synthetic_unknown() -> RawValue {
    RawValue::Map(
        [
            (1, RawValue::Uint(u64::MAX)),
            (2, RawValue::F32(1.25)),
            (3, RawValue::F64(-2.5)),
            (
                4,
                RawValue::Array(vec![
                    RawValue::Null,
                    RawValue::Bytes(vec![0, 255]),
                    RawValue::Negative(u64::MAX),
                ]),
            ),
        ]
        .into(),
    )
}
pub fn schema(name: &str) -> Schema {
    *registry::SCHEMAS
        .iter()
        .find(|s| s.name == name)
        .unwrap_or_else(|| panic!("missing schema {name}"))
}
pub fn sample(schema: Schema) -> RawMap {
    let mut raw: RawMap = schema
        .fields
        .iter()
        .filter(|f| f.absence == "required")
        .map(|f| (f.key, sample_type(f.shape)))
        .collect();
    use RawValue as V;
    match schema.name {
        "InputTouchpadContacts" => {
            raw.insert(3, V::Uint(1));
        }
        "ContactSurface" => {
            raw.insert(1, V::Uint(1000));
            raw.insert(2, V::Uint(1000));
        }
        "TrailContinuation" => {
            raw.insert(4, V::F64(0.0));
        }
        "DiagnosticEvent" => {
            raw.insert(1, V::from("future-event/1"));
        }

        "AimState" => {
            raw.insert(7, V::F32(7.0 / 686.0));
        }
        "AttachmentItems" | "AttachmentResult" => {
            raw.insert(4, V::Map(sample_named("OfferId")));
            raw.insert(5, V::Uint(0));
        }
        "DragImage" => {
            let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
            png.extend_from_slice(&1_u32.to_be_bytes());
            png.extend_from_slice(&1_u32.to_be_bytes());
            raw.insert(1, V::Bytes(png));
        }
        "LowOptions" => {
            raw.insert(3, V::Uint(2));
        }
        "CapsTapHold" => {
            raw.insert(1, V::Uint(0x0007_00e1));
            raw.insert(2, V::Uint(0x0007_0039));
        }
        "KeyboardMapping" => {
            raw.insert(1, V::Uint(2));
            raw.insert(3, V::Map(sample_named("CapsTapHold")));
        }
        "ScreenPreferencesValue" => {
            raw.insert(2, V::F64(1.0));
        }
        "ControlSetFocusLock" => {
            raw.insert(2, V::Bool(true));
            raw.insert(3, V::Map(sample_named("DeviceId")));
        }
        "ResourceFacts" => {
            raw.insert(7, V::Map(sample_named("GameObservation")));
        }
        "ObservedState" => {
            raw.insert(6, V::Array(vec![V::Map(sample_named("UserFacingProblem"))]));
        }
        "ResourceFault" => {
            raw.insert(3, V::Map(sample_named("UserFacingProblem")));
        }
        "AudioState" => {
            raw.insert(15, V::Map(sample_named("UserFacingProblem")));
        }

        "TopologySettings" | "EffectiveSettings" => {
            raw.extend([
                (6, V::Bool(false)),
                (7, V::F64(5.0 / 686.0)),
                (8, V::F64(10.0 / 686.0)),
                (9, V::F64(8.0 / 686.0)),
                (10, V::Bool(true)),
                (12, V::F64(0.0)),
            ]);
        }
        "ExitVolumeState" => {
            raw.insert(4, V::Uint(2));
        }
        "DeviceId" => {
            raw.insert(2, V::Bytes(vec![0; 32]));
        }
        "Digest" => {
            raw.insert(1, V::from(v1::dto::CONTENT_DIGEST));
            raw.insert(2, V::Bytes(vec![0; 32]));
        }
        "Signature" => {
            raw.insert(2, V::Bytes(vec![0; 64]));
        }
        "BootPair" => {
            let mut follower = sample_named("Boot");
            let mut id = sample_named("DeviceId");
            id.insert(2, V::Bytes(vec![1; 32]));
            follower.insert(1, V::Map(id));
            raw.insert(2, V::Map(follower));
        }
        "ProtocolOffer" | "ProtocolVersion" => {
            raw.insert(1, V::Uint(1));
            raw.insert(2, V::Uint(0));
        }
        "Limits" => return v1::dto::Limits::baseline().into_raw(),
        "ConnectionOffer" => {
            raw.insert(3, V::Array(vec![V::Map(sample_named("ProtocolOffer"))]));
            let mut caps = registry::BASELINE.to_vec();
            caps.sort();
            raw.insert(4, V::Array(caps.into_iter().map(V::from).collect()));
            raw.insert(7, V::Map(sample_named("CryptoOffers")));
        }
        "CryptoOffers" => {
            raw.insert(1, V::Array(vec![V::from(v1::dto::PAIRING_SUITE)]));
            raw.insert(2, V::Array(vec![V::from(v1::dto::ADMISSION_SUITE)]));
            raw.insert(3, V::Array(vec![V::from(v1::dto::ED25519)]));
        }
        "Negotiated" => {
            raw.insert(6, V::from(v1::dto::ADMISSION_SUITE));
        }
        "SignedRecord" => {
            raw.insert(1, V::Map(sample_named("RecordBody")));
        }
        "Request" => {
            raw.insert(6, V::Map(sample_named("Roster")));
        }
        "RecordBody" => {
            raw.insert(4, V::Uint(0));
        }
        "PairEnded" => {
            raw.insert(1, V::Map(sample_named("PairId")));
        }
        "PairContext" => {
            raw.insert(10, V::from(v1::dto::PAIRING_SUITE));
        }
        "PairPake" => {
            raw.insert(2, V::from(v1::dto::PAIRING_SUITE));
        }
        "EndpointFact" => {
            raw.insert(2, V::Bytes(vec![0; 4]));
        }
        "ScreenRect" => {
            raw.insert(3, V::Map(sample_named("Rect")));
        }
        "Register" => {
            raw.insert(1, V::Uint(2));
            raw.insert(4, V::Map(sample_named("LayoutValue")));
            raw.insert(5, V::Bool(false));
            raw.insert(6, v1::retained::register_digest(&raw).unwrap().write());
        }
        "Advert" => {
            raw.insert(9, V::from("unknown"));
            raw.insert(7, V::Array(vec![V::Map(sample_named("ProtocolOffer"))]));
        }
        _ => {}
    }
    raw
}
pub fn sample_named(name: &str) -> RawMap {
    sample(schema(name))
}
pub fn sample_type(shape: &str) -> RawValue {
    use RawValue as V;
    let shape = shape.replace(' ', "");
    let s = shape.as_str();
    if s.starts_with("Option<") {
        return V::Null;
    }
    if s.starts_with("List<") {
        return V::Array(vec![]);
    }
    if s.starts_with("Bytes<") {
        return V::Bytes(vec![]);
    }
    if s.starts_with("Text<") {
        return V::from("synthetic");
    }
    if s.starts_with("Ascii<") {
        return V::from("synthetic");
    }
    match s {
        "RawValue" | "crate::v1::RawValue" | "u8" | "u16" | "u32" | "u64" | "i32" | "i64" => V::Uint(1),
        "bool" => V::Bool(false),
        "f32" => V::F32(1.0),
        "f64" => V::F64(1.0),
        "Id16" => V::Bytes(vec![0; 16]),
        "Hash32" => V::Bytes(vec![0; 32]),
        "AlgorithmId" => V::from(v1::dto::ED25519),
        "CapabilityId" => V::from("oink.core/1"),
        "CapabilityList" => V::Array(vec![]),
        "RawMap" => V::Map(RawMap::new()),
        "FormatId" => V::from("mime:text/plain;charset=utf-8"),
        "PathReference" => V::Map([(1, V::Uint(0)), (2, V::Map(sample_named("RequestedPath")))].into()),
        "PreviewAvailability" | "RevokeScope" | "DragState" | "ContentSize" | "Demand" => {
            V::Map([(1, V::Uint(0))].into())
        }
        "AttachmentHeader" => V::Map([(1, V::Uint(0)), (2, V::Map(sample_named("OfferAttachment")))].into()),
        "SourceAction" => V::Map([(1, V::Uint(0)), (2, V::Map(sample_named("SourceHandoff")))].into()),
        "RepresentationData" => V::Map([(1, V::Uint(0)), (2, V::Map(sample_named("RepresentationReader")))].into()),
        "TypedResult" => V::Map([(1, V::Uint(0))].into()),
        "ReplyState" => V::Map([(1, V::Uint(0)), (2, V::Map(sample_named("ReplySame")))].into()),
        "PairStep" => V::Map([(1, V::Uint(0)), (2, V::Map(sample_named("PakeStart")))].into()),
        "Decision" => V::Map([(1, V::Uint(0))].into()),
        "ObjectContext" => V::Map([(1, V::Uint(0)), (2, V::Map(sample_named("ObjectPairContext")))].into()),
        _ => {
            if let Some((_, variants)) = registry::ENUMS.iter().find(|(name, _)| *name == s) {
                V::Uint(u64::from(variants[0].0))
            } else if let Some((_, _, variants)) = registry::OPEN_ENUMS.iter().find(|(name, _, _)| *name == s) {
                let (id, body) = variants[0];
                let mut raw = v1::RawMap::from([(1, V::Uint(id.into()))]);
                if let Some(body) = body {
                    raw.insert(2, V::Map(sample_named(body)));
                }
                V::Map(raw)
            } else {
                V::Map(sample_named(s))
            }
        }
    }
}

pub fn future_resources() -> RawValue {
    use RawValue as V;
    let faults = (1..=5)
        .map(|key| {
            (
                key,
                V::Map([(1, V::Uint(if key == 5 { 400 } else { 0 })), (900, synthetic_unknown())].into()),
            )
        })
        .collect();
    let revisions = [1, 2, 3, 4, 5, 400]
        .into_iter()
        .map(|option| V::Map([(1, V::Uint(option)), (2, V::Uint(7)), (900, synthetic_unknown())].into()))
        .collect();
    V::Map(
        [
            (1, V::Uint(0)),
            (2, V::Uint(400)),
            (3, V::Uint(1)),
            (4, V::Map(faults)),
            (
                5,
                V::Map(
                    [
                        (1, V::Map(sample_named("AudioSessionContext"))),
                        (2, V::Uint(3)),
                        (3, V::Uint(0)),
                        (4, V::Bool(false)),
                        (5, V::Map(sample_named("ReceiverOutputBinding"))),
                        (6, V::Uint(16)),
                        (7, V::Bool(true)),
                        (8, V::Map(sample_named("DeviceId"))),
                        (900, synthetic_unknown()),
                    ]
                    .into(),
                ),
            ),
            (6, V::Array(revisions)),
            (7, V::Map(sample_named("GameObservation"))),
            (900, synthetic_unknown()),
        ]
        .into(),
    )
}
pub fn future_exit_volume() -> RawValue {
    use RawValue as V;
    let mut version = sample_named("ExitVolumeVersion");
    version.insert(3, V::from("synthetic-output"));
    version.insert(900, synthetic_unknown());
    V::Map(
        [
            (1, V::Map(version)),
            (2, V::Uint(0)),
            (3, V::Bool(false)),
            (4, V::Uint(0)),
            (900, synthetic_unknown()),
        ]
        .into(),
    )
}

pub fn future_preferences() -> RawValue {
    use RawValue as V;
    let items = (1..=8)
        .chain(10..=16)
        .chain([18, 19, 20, 400])
        .map(|option| {
            let value = match option {
                1 | 2 | 8 => V::Uint(0),
                3..=6 | 10 | 19 => V::Bool(true),
                7 => {
                    let mut screen = sample_named("ScreenPreference");
                    screen.insert(13, synthetic_unknown());
                    V::Map([(1, V::Array(vec![V::Map(screen)])), (900, synthetic_unknown())].into())
                }
                11 => V::Uint(270),
                12 => V::F64(1.0),
                13 => V::Map(sample_named("InputTransformValue")),
                14 => V::Map(sample_named("SourceInputPolicyValue")),
                15 => V::Map([(1, V::Uint(100)), (2, V::Bool(false))].into()),
                16 => V::from("synthetic-device"),
                18 => V::Uint(0),
                20 => V::Map(sample_named("AudioExitClaim")),
                _ => synthetic_unknown(),
            };
            V::Map(
                [
                    (1, V::Uint(option)),
                    (2, V::Uint(u64::MAX)),
                    (3, value),
                    (900, synthetic_unknown()),
                ]
                .into(),
            )
        })
        .collect();
    V::Array(items)
}

pub fn future_pause() -> RawValue {
    use RawValue as V;
    V::Map(
        [
            (1, V::Bool(true)),
            (2, V::Uint(3)),
            (3, V::Uint(7)),
            (4, V::Map(sample_named("DeviceId"))),
            (900, synthetic_unknown()),
        ]
        .into(),
    )
}
