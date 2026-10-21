//! Explicit fixture maintenance, never run by compatibility tests.
//! Compatible optional extensions append vectors and preserve existing bytes.
use oink_protocol::v1::{self, RawValue, frame, registry, schema::WireDto};
use oink_test_support::protocol as support;
use serde_json::json;
fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args().nth(1).as_deref() == Some("--append-compatible") {
        return append_compatible();
    }
    if std::env::args().nth(1).as_deref() != Some("--regenerate-unfrozen") {
        return Err("pass --regenerate-unfrozen before the public freeze only".into());
    }
    let directory =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../crates/oink-protocol/tests/fixtures/v1");
    let manifest_path = directory.join("manifest.json");
    if manifest_path.exists() {
        let previous: serde_json::Value = serde_json::from_slice(&std::fs::read(&manifest_path)?)?;
        if previous["frozen"] == true {
            return Err("frozen fixtures cannot be regenerated".into());
        }
    }
    let mut fixtures = vec![];
    for entry in registry::MESSAGES {
        for unknown in [false, true] {
            let mut raw = support::sample(entry.schema);
            if entry.name == "ConfigInfo" {
                raw.insert(5, RawValue::Map(support::sample_named("Negotiated")));
            }
            if unknown {
                raw.insert(900, support::synthetic_unknown());
                match entry.name {
                    "InputKey" => {
                        raw.insert(
                            3,
                            RawValue::Uint(u64::from(oink_foundation::input::KeyCode::CAPS_LOCK.0)),
                        );
                    }
                    "ConfigFacts" => {
                        raw.insert(16, support::future_resources());
                        raw.insert(17, support::future_exit_volume());
                        raw.insert(18, support::future_preferences());
                        raw.insert(19, support::future_pause());
                        raw.insert(20, RawValue::Map(support::sample_named("AudioExitClaim")));
                        raw.insert(21, RawValue::Map(support::sample_named("AudioExitClaim")));
                    }
                    "ConfigSetLocalOption" => {
                        raw.insert(3, RawValue::Uint(400));
                        raw.insert(4, support::synthetic_unknown());
                    }
                    "ConfigExitVolumeResult" => {
                        raw.insert(5, support::future_exit_volume());
                    }
                    "ConfigExitVolumeChanged" => {
                        raw.insert(1, support::future_exit_volume());
                    }
                    "ConfigEmergencyPauseAck" => {
                        raw.insert(3, RawValue::Uint(400));
                    }
                    "AudioPrepareRefused" => {
                        raw.insert(2, RawValue::Uint(16));
                        raw.insert(3, RawValue::Map(support::sample_named("ReceiverOutputBinding")));
                    }
                    _ => {}
                }
            }
            let message =
                v1::Message::from_payload(entry.id, raw.clone()).map_err(|e| format!("{}: {e}", entry.name))?;
            let bytes = frame::encode(&message, v1::limits::LOW_FRAME)?;
            let suffix = if unknown { "-future" } else { "" };
            let file = format!("{:04x}{suffix}.hex", entry.id);
            std::fs::write(directory.join(&file), format!("{}\n", support::hex(&bytes)))?;
            fixtures.push(json!({"file":file,"kind":"frame","type_id":entry.id,"schema":entry.schema.name,
                "capabilities":entry.capabilities,"routes":entry.routes,"expected":support::describe(&RawValue::Map(raw))}));
        }
    }
    // Carrying DTOs and all allocated unit enum ids have their own stable bytes.
    for schema in registry::SCHEMAS {
        let mut raw = support::sample(*schema);
        raw.insert(900, support::synthetic_unknown());
        if schema.name == "DeviceFacts" {
            raw.insert(16, support::future_resources());
            raw.insert(17, support::future_exit_volume());
            raw.insert(18, support::future_preferences());
            raw.insert(19, support::future_pause());
            raw.insert(20, RawValue::Map(support::sample_named("AudioExitClaim")));
            raw.insert(21, RawValue::Map(support::sample_named("AudioExitClaim")));
        }
        if schema.name == "Register" {
            raw.insert(6, v1::schema::WireValue::write(&v1::retained::register_digest(&raw)?));
        }
        registry::validate_schema(schema.name, &raw)?;
        let value = RawValue::Map(raw);
        let file = format!("dto-{}.hex", schema.name);
        std::fs::write(
            directory.join(&file),
            format!("{}\n", support::hex(&value.encode(v1::limits::LOW_FRAME)?)),
        )?;
        fixtures.push(json!({"file":file,"kind":"dto","schema":schema.name,"expected":support::describe(&value)}));
    }
    // The portable URL contract keeps each source's native descriptor and bytes.
    // These vectors cover FormatId mapping independently of the clipboard model.
    for (platform, native, medium, payload) in [
        ("mac", "mac-uti:public.url", 0, b"https://example.invalid/".to_vec()),
        (
            "windows",
            "win-name:UniformResourceLocatorW",
            1,
            "https://example.invalid/"
                .encode_utf16()
                .chain(Some(0))
                .flat_map(u16::to_le_bytes)
                .collect(),
        ),
    ] {
        let value = RawValue::Map(
            [
                (1, RawValue::Uint(1)),
                (2, RawValue::Text(native.into())),
                (3, RawValue::Uint(medium)),
                (4, RawValue::Uint(1)),
                (5, RawValue::Negative(0)),
                (6, RawValue::Text(v1::dto::URL_FORMAT.into())),
                (
                    7,
                    RawValue::Map(
                        [
                            (1, RawValue::Uint(1)),
                            (2, RawValue::Map([(1, RawValue::Bytes(payload))].into())),
                        ]
                        .into(),
                    ),
                ),
            ]
            .into(),
        );
        registry::validate_schema("Representation", value.as_map().unwrap())?;
        let file = format!("dto-Representation-url-{platform}.hex");
        std::fs::write(
            directory.join(&file),
            format!("{}\n", support::hex(&value.encode(v1::limits::LOW_FRAME)?)),
        )?;
        fixtures.push(json!({"file":file,"kind":"dto","schema":"Representation","expected":support::describe(&value)}));
    }
    for (name, values) in registry::ENUMS {
        for id in values.iter().map(|v| v.0).chain([0x7fff_fffe]) {
            let value = RawValue::Uint(u64::from(id));
            let file = format!("enum-{name}-{id}.hex");
            std::fs::write(directory.join(&file), format!("{}\n", support::hex(&value.encode(9)?)))?;
            fixtures.push(json!({"file":file,"kind":"enum","schema":name,"expected":support::describe(&value)}));
        }
    }
    for (name, _, variants) in registry::OPEN_ENUMS {
        for (id, body_schema) in variants.iter().copied().chain([(0x7fff_fffe, None)]) {
            let mut raw = v1::RawMap::from([(1, RawValue::Uint(id.into())), (900, support::synthetic_unknown())]);
            if let Some(body_schema) = body_schema {
                raw.insert(2, RawValue::Map(support::sample_named(body_schema)));
            } else if id == 0x7fff_fffe {
                raw.insert(2, support::synthetic_unknown());
            }
            registry::validate_open(name, &raw)?;
            let value = RawValue::Map(raw);
            let file = format!("open-{name}-{id}.hex");
            std::fs::write(
                directory.join(&file),
                format!("{}\n", support::hex(&value.encode(v1::limits::LOW_FRAME)?)),
            )?;
            fixtures.push(json!({"file":file,"kind":"open","schema":name,"expected":support::describe(&value)}));
        }
    }
    for kind in registry::DIAGNOSTIC_EVENTS {
        let fields = v1::dto::diagnostic_event_schema(kind).map_or_else(v1::RawMap::new, support::sample);
        let value = RawValue::Map(
            [
                (1, RawValue::from(*kind)),
                (2, RawValue::Map(fields)),
                (900, support::synthetic_unknown()),
            ]
            .into(),
        );
        let file = format!("event-{}.hex", kind.replace('/', "-"));
        std::fs::write(
            directory.join(&file),
            format!("{}\n", support::hex(&value.encode(v1::limits::LOW_FRAME)?)),
        )?;
        fixtures
            .push(json!({"file":file,"kind":"dto","schema":"DiagnosticEvent","expected":support::describe(&value)}));
    }
    let header = frame::StreamHeader::bootstrap().encode()?;
    std::fs::write(directory.join("stream.hex"), format!("{}\n", support::hex(&header)))?;
    fixtures.push(json!({"file":"stream.hex","kind":"stream","expected":{"kind":0,"priority":0,"route_tag":0,"ordinal":1,"flow":0}}));
    let discovery = v1::Message::DiscoveryBeacon(v1::dto::Advert::from_raw(support::sample_named("Advert"))?);
    std::fs::write(
        directory.join("discovery.hex"),
        format!("{}\n", support::hex(&frame::encode_discovery(&discovery)?)),
    )?;
    fixtures.push(json!({"file":"discovery.hex","kind":"discovery","expected":support::describe(&RawValue::Map(discovery.raw().clone()))}));
    let audio = v1::Message::AudioPacket(v1::dto::AudioPacket::from_raw(support::sample_named("AudioPacket"))?);
    std::fs::write(
        directory.join("datagram.hex"),
        format!("{}\n", support::hex(&frame::encode_datagram(7, &audio)?)),
    )?;
    fixtures.push(json!({"file":"datagram.hex","kind":"datagram","flow":7,"expected":support::describe(&RawValue::Map(audio.raw().clone()))}));
    // This tool is already guarded by the explicit unfrozen flag and manifest.
    // Remove stale generated vectors when a pre-freeze allocation is reshaped.
    let generated: std::collections::BTreeSet<_> = fixtures.iter().map(|f| f["file"].as_str().unwrap()).collect();
    for file in std::fs::read_dir(&directory)? {
        let path = file?.path();
        if path.extension().is_some_and(|s| s == "hex")
            && !generated.contains(path.file_name().unwrap().to_str().unwrap())
        {
            std::fs::remove_file(path)?;
        }
    }
    let schema = support::schema_json();
    std::fs::write(
        directory.join("schema.json"),
        format!("{}\n", serde_json::to_string_pretty(&schema)?),
    )?;
    let manifest = json!({"revision":v1::SPEC_REVISION,"frozen":false,"schema_sha256":support::schema_hash(),"limits":{"reliable_frame":16384,"config_frame":131072,"low_frame":16777216,"datagram":1100,"config_state":122880,"low_chunk":16384},"fixtures":fixtures});
    std::fs::write(manifest_path, format!("{}\n", serde_json::to_string_pretty(&manifest)?))?;
    let offer = frame::encode(
        &v1::Message::ConnectionOffer(v1::dto::ConnectionOffer::from_raw(support::sample_named(
            "ConnectionOffer",
        ))?),
        v1::limits::RELIABLE_FRAME,
    )?;
    let record = v1::dto::SignedRecord::from_raw(support::sample_named("SignedRecord"))?;
    let record_bytes = record.encode(v1::limits::CONFIG_STATE)?;
    let signature_input = v1::retained::record_signature_input(&record)?;
    let crypto = json!({
        "offer_frame":support::hex(&offer),
        "offer_hash":support::hex(&v1::admission::offer_hash(&offer, &offer)?.0),
        "signed_record":support::hex(&record_bytes),
        "record_id":support::hex(&v1::retained::record_id(&record)?.0),
        "signature_input":support::hex(&signature_input),
        "signature_input_sha256":support::hex(&v1::retained::sha256(&signature_input).0),
    });
    std::fs::write(
        directory.join("crypto.json"),
        format!("{}\n", serde_json::to_string_pretty(&crypto)?),
    )?;
    println!(
        "{} synthetic vectors; schema {}",
        manifest["fixtures"].as_array().unwrap().len(),
        support::schema_hash()
    );
    Ok(())
}

fn append_compatible() -> Result<(), Box<dyn std::error::Error>> {
    let directory =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../crates/oink-protocol/tests/fixtures/v1");
    let mut manifest: serde_json::Value = serde_json::from_slice(&std::fs::read(directory.join("manifest.json"))?)?;
    let previous: serde_json::Value = serde_json::from_slice(&std::fs::read(directory.join("schema.json"))?)?;
    let schema = support::schema_json();
    // Append new allocations and optional fields while preserving every frozen
    // field/route/capability and every existing hex or cryptographic fixture.
    for (table, key) in [("messages", "id"), ("schemas", "name"), ("streams", "kind")] {
        for entry in previous[table].as_array().ok_or("missing table")? {
            if !schema[table]
                .as_array()
                .ok_or("missing table")?
                .iter()
                .any(|candidate| candidate[key] == entry[key] && compatible_entry(table, entry, candidate))
            {
                return Err(format!("changed existing {table} entry").into());
            }
        }
    }
    for entry in registry::MESSAGES {
        if previous["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|old| old["id"] == entry.id)
        {
            continue;
        }
        if !entry.capabilities.iter().any(|cap| {
            !previous["capabilities"]
                .as_array()
                .unwrap()
                .iter()
                .any(|old| old == cap)
        }) {
            return Err("new message requires new optional capability".into());
        }
        for unknown in [false, true] {
            let mut raw = support::sample(entry.schema);
            if unknown {
                raw.insert(900, support::synthetic_unknown());
            }
            let message = v1::Message::from_payload(entry.id, raw.clone())?;
            let file = format!("{:04x}{}.hex", entry.id, if unknown { "-future" } else { "" });
            let mut output = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(directory.join(&file))?;
            use std::io::Write;
            writeln!(
                output,
                "{}",
                support::hex(&frame::encode(&message, v1::limits::LOW_FRAME)?)
            )?;
            manifest["fixtures"].as_array_mut().unwrap().push(json!({"file":file,"kind":"frame","type_id":entry.id,"schema":entry.schema.name,"capabilities":entry.capabilities,"routes":entry.routes,"expected":support::describe(&RawValue::Map(raw))}));
        }
    }
    for entry in registry::SCHEMAS {
        if previous["schemas"]
            .as_array()
            .unwrap()
            .iter()
            .any(|old| old["name"] == entry.name)
        {
            continue;
        }
        let mut raw = support::sample(*entry);
        raw.insert(900, support::synthetic_unknown());
        registry::validate_schema(entry.name, &raw)?;
        let value = RawValue::Map(raw);
        let file = format!("dto-{}.hex", entry.name);
        let mut output = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(directory.join(&file))?;
        use std::io::Write;
        writeln!(output, "{}", support::hex(&value.encode(v1::limits::LOW_FRAME)?))?;
        manifest["fixtures"]
            .as_array_mut()
            .unwrap()
            .push(json!({"file":file,"kind":"dto","schema":entry.name,"expected":support::describe(&value)}));
    }
    for (name, values) in registry::ENUMS {
        for (id, _) in *values {
            let file = format!("enum-{name}-{id}.hex");
            if directory.join(&file).exists() {
                continue;
            }
            let value = RawValue::Uint(u64::from(*id));
            append_value(&directory, &mut manifest, &file, "enum", name, value)?;
        }
    }
    // Explicit forward-compatibility witness: the released retaining Facts
    // decoder must preserve both the new token and the new owner option.
    let file = "dto-DeviceFacts-layout-profiles.hex";
    if !directory.join(file).exists() {
        let mut facts = support::sample_named("DeviceFacts");
        facts.insert(22, RawValue::Text("synthetic-native-token".into()));
        let mut preference = support::sample_named("DevicePreference");
        preference.insert(1, RawValue::Uint(21));
        preference.insert(3, RawValue::Bool(true));
        facts.insert(18, RawValue::Array(vec![RawValue::Map(preference)]));
        registry::validate_schema("DeviceFacts", &facts)?;
        append_value(
            &directory,
            &mut manifest,
            file,
            "dto",
            "DeviceFacts",
            RawValue::Map(facts),
        )?;
    }
    manifest["schema_sha256"] = support::schema_hash().into();
    std::fs::write(
        directory.join("schema.json"),
        format!("{}\n", serde_json::to_string_pretty(&schema)?),
    )?;
    std::fs::write(
        directory.join("manifest.json"),
        format!("{}\n", serde_json::to_string_pretty(&manifest)?),
    )?;
    Ok(())
}

fn compatible_entry(table: &str, old: &serde_json::Value, new: &serde_json::Value) -> bool {
    let mut projected = new.clone();
    let (before, after) = match table {
        "messages" => (&old["schema"], &mut projected["schema"]),
        "schemas" => (old, &mut projected),
        _ => return old == new,
    };
    let (Some(a), Some(b)) = (before["fields"].as_array(), after["fields"].as_array()) else {
        return false;
    };
    let max = a.iter().filter_map(|f| f["key"].as_u64()).max().unwrap_or(0);
    let mut seen = std::collections::BTreeSet::new();
    for field in b {
        let Some(key) = field["key"].as_u64() else { return false };
        if !seen.insert(key) {
            return false;
        }
        match a.iter().find(|f| f["key"] == key) {
            Some(previous) if previous == field => {}
            None if key > max && field["absence"] != "required" => {}
            _ => return false,
        }
    }
    if a.iter().any(|f| !b.contains(f)) {
        return false;
    }
    after["fields"] = before["fields"].clone();
    &projected == old
}

fn append_value(
    directory: &std::path::Path,
    manifest: &mut serde_json::Value,
    file: &str,
    kind: &str,
    schema: &str,
    value: RawValue,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Write;
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(directory.join(file))?;
    writeln!(output, "{}", support::hex(&value.encode(v1::limits::LOW_FRAME)?))?;
    manifest["fixtures"]
        .as_array_mut()
        .unwrap()
        .push(json!({"file":file,"kind":kind,"schema":schema,"expected":support::describe(&value)}));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn optional_appends_preserve_frozen_fields_and_message_admission() {
        let old = json!({"name":"Example","fields":[{"key":1,"name":"first","shape":"u64","absence":"required"}]});
        let mut next = old.clone();
        next["fields"]
            .as_array_mut()
            .unwrap()
            .push(json!({"key":2,"name":"extra","shape":"Option<u64>","absence":"optional"}));
        assert!(compatible_entry("schemas", &old, &next));
        next["fields"][1]["absence"] = "required".into();
        assert!(!compatible_entry("schemas", &old, &next));
        next = old.clone();
        next["fields"][0]["shape"] = "u32".into();
        assert!(!compatible_entry("schemas", &old, &next));
        let old = json!({"id":1,"schema":old,"capabilities":["original"],"routes":[2]});
        let mut next = old.clone();
        next["routes"] = json!([0]);
        assert!(!compatible_entry("messages", &old, &next));
    }
}
