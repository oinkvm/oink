//! Standalone codec peer for independently archived compatibility baselines.
//! stdin/stdout use bounded JSON lines; this is test tooling, not a network API.
use oink_protocol::v1::{self, frame, schema::WireDto};
use oink_test_support::protocol as support;
use serde_json::{Value, json};
use std::io::{self, BufRead, Read, Write};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args().nth(1).as_deref() == Some("--schema") {
        println!("{}", support::schema_json());
        return Ok(());
    }
    if std::env::args().nth(1).as_deref() == Some("--identity") {
        println!(
            "{}",
            json!({"harness":1,"revision":option_env!("OINK_BASELINE_REVISION").unwrap_or("development"),"frozen":option_env!("OINK_BASELINE_FROZEN")==Some("1"),"schema_sha256":support::schema_hash()})
        );
        return Ok(());
    }
    let input = io::stdin();
    let mut input = input.lock();
    let mut output = io::stdout().lock();
    loop {
        // Bound the local harness protocol before allocating an unbounded line.
        let mut line = String::new();
        let bytes = input
            .by_ref()
            .take((2 * v1::limits::LOW_FRAME + 4096) as u64)
            .read_line(&mut line)?;
        if bytes == 0 {
            break;
        }
        if !line.ends_with('\n') {
            return Err("bounded request line required".into());
        }
        let request: Value = serde_json::from_str(&line)?;
        let result = handle(&request);
        let reply = match result {
            Ok(mut value) => {
                value["ok"] = json!(true);
                value
            }
            Err(error) => json!({"ok":false,"code":error.code.number(),"field_path":error.field_path}),
        };
        writeln!(output, "{reply}")?;
        output.flush()?;
    }
    Ok(())
}
fn handle(request: &Value) -> v1::Result<Value> {
    if request["kind"] == "offer" {
        return offer(request);
    }
    let Some(hex) = request["hex"].as_str() else {
        return Err(v1::ErrorCode::MalformedFrame.into());
    };
    if hex.len() % 2 != 0 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(v1::ErrorCode::MalformedFrame.into());
    }
    let bytes = support::unhex(hex);
    let encoded = match request["kind"].as_str() {
        Some("negotiate") => {
            let remote = request["remote_hex"].as_str().ok_or(v1::ErrorCode::MalformedFrame)?;
            if remote.len() % 2 != 0 || !remote.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(v1::ErrorCode::MalformedFrame.into());
            }
            let selected =
                v1::admission::negotiate(&bytes, &support::unhex(remote), v1::registry::CAPABILITIES, false)?;
            return Ok(json!({
                "hex": support::hex(&selected.encode(v1::limits::RELIABLE_FRAME)?),
                "major": selected.major()?, "minor": selected.minor()?,
                "enabled": selected.enabled()?.0.into_iter().map(|cap| cap.0).collect::<Vec<_>>()
            }));
        }
        Some("frame") => frame::encode(
            &frame::OpaqueFrame::parse_exact(&bytes, v1::limits::LOW_FRAME)?.decode()?,
            v1::limits::LOW_FRAME,
        )?,
        Some("dto") | Some("enum") | Some("open") => {
            let raw = v1::cbor::decode(&bytes, v1::limits::LOW_FRAME)?;
            let name = request["schema"].as_str().ok_or(v1::ErrorCode::MalformedFrame)?;
            if request["kind"] == "dto" {
                v1::registry::validate_schema(name, raw.as_map()?)?;
            } else if request["kind"] == "open" {
                v1::registry::validate_open(name, raw.as_map()?)?;
            } else {
                v1::registry::validate_enum(name, &raw)?;
            }
            raw.encode(v1::limits::LOW_FRAME)?
        }
        Some("stream") => {
            let (header, len) = frame::StreamHeader::parse(&bytes)?.ok_or(v1::ErrorCode::MalformedFrame)?;
            if len != bytes.len() {
                return Err(v1::ErrorCode::MalformedFrame.into());
            }
            header.encode()?.to_vec()
        }
        Some("discovery") => frame::encode_discovery(&frame::discovery(&bytes)?)?,
        Some("datagram") => {
            let frame::Datagram::Active { flow, frame } = frame::datagram(&bytes, |_| true)? else {
                return Err(v1::ErrorCode::MalformedFrame.into());
            };
            frame::encode_datagram(flow, &frame.decode()?)?
        }
        Some("register-edit") => {
            let register = v1::dto::Register::from_raw(v1::cbor::decode_map(&bytes, v1::limits::CONFIG_STATE)?)?;
            let edited = v1::retained::edit_register(
                &register,
                &register.version()?,
                register.version()?.writer()?,
                |value| v1::retained::patch_path(value, &[4, 10], Some(v1::RawValue::Bool(true))),
            )?;
            edited.encode(v1::limits::CONFIG_STATE)?
        }
        _ => return Err(v1::ErrorCode::UnsupportedValue.into()),
    };
    Ok(json!({"hex": support::hex(&encoded)}))
}

// Generate endpoint offers in the independently built peer, including opaque
// future allocations. Negotiation must use exact frame bytes from both peers.
fn offer(request: &Value) -> v1::Result<Value> {
    use v1::{RawValue as V, dto::ConnectionOffer};
    let mut raw = support::sample_named("ConnectionOffer");
    if request["responder"].as_bool() == Some(true) {
        let mut boot = support::sample_named("Boot");
        let mut id = support::sample_named("DeviceId");
        id.insert(2, V::Bytes(vec![1; 32]));
        boot.insert(1, V::Map(id));
        raw.insert(1, V::Map(boot));
        raw.insert(8, V::Bytes(vec![1; 32]));
    }
    let major = request["major"].as_u64().unwrap_or(1);
    let minor = request["minor"].as_u64().unwrap_or(0);
    let mut protocols = vec![V::Map([(1, V::Uint(major)), (2, V::Uint(minor))].into())];
    if request["also_major2"].as_bool() == Some(true) {
        protocols.push(V::Map([(1, V::Uint(2)), (2, V::Uint(0))].into()));
    }
    raw.insert(3, V::Array(protocols));
    let mut capabilities = v1::registry::BASELINE.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    for cap in request["capabilities"].as_array().into_iter().flatten() {
        capabilities.push(cap.as_str().ok_or(v1::ErrorCode::MalformedFrame)?.to_string());
    }
    if let Some(missing) = request["omit"].as_str() {
        capabilities.retain(|s| s != missing);
    }
    capabilities.sort();
    capabilities.dedup();
    raw.insert(4, V::Array(capabilities.into_iter().map(V::Text).collect()));
    if request["future"].as_bool() == Some(true) {
        raw.insert(900, support::synthetic_unknown());
    }
    let message = v1::Message::ConnectionOffer(ConnectionOffer::from_raw(raw)?);
    Ok(json!({"hex":support::hex(&frame::encode(&message, v1::limits::RELIABLE_FRAME)?)}))
}
