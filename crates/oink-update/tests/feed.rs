use base64::{Engine, engine::general_purpose::STANDARD};
use ed25519_dalek::{Signer, SigningKey};
use oink_update::{Error, PRODUCT, WINDOWS_EPOCH, feed::*, policy::Channel};
use serde_json::{Value, json};
use std::collections::BTreeSet;
const NOW: i64 = 1791936000; // 2026-10-14 UTC
// TEST ONLY, derived in memory; never compiled into production trust.
fn key() -> SigningKey {
    SigningKey::from_bytes(&[7; 32])
}
fn payload() -> Value {
    json!({"schema":1,"product":PRODUCT,"keyEpoch":WINDOWS_EPOCH,"channel":"stable","sequence":1,
        "generatedAt":"2026-10-14T00:00:00Z","expiresAt":"2026-11-13T00:00:00Z", "releases":[{
            "build":20,"version":"1.0.1","tag":"v1.0.1","sourceTreeSha256":"a".repeat(64),"architecture":"x86_64",
            "minimumWindowsBuild":10240,"protocolBaseline":1,"minimumUpdaterSchema":1,"severity":"normal","notes":{"zh-Hans":"修复问题。","en":"Fix a problem."},
            "package":{"url":"https://github.com/oinkvm/oink/releases/download/v1.0.1/update.zip","length":3,
            "sha256":"b".repeat(64),"keyId":WINDOWS_EPOCH,"ed25519":STANDARD.encode([0;64])}}]})
}
fn signed_bytes(bytes: &[u8]) -> Vec<u8> {
    serde_json::to_vec(&json!({"envelopeSchema":1,"payload":STANDARD.encode(bytes),"signatures":[{"keyId":WINDOWS_EPOCH,"ed25519":STANDARD.encode(key().sign(bytes).to_bytes())}]})).unwrap()
}
fn envelope(p: &Value) -> Vec<u8> {
    signed_bytes(&serde_json::to_vec(p).unwrap())
}
fn check(p: &Value, seen: &Watermark, now: i64) -> Result<VerifiedManifest, Error> {
    verify_manifest(
        &envelope(p),
        &Trust {
            epoch: WINDOWS_EPOCH,
            key: &key().verifying_key().to_bytes(),
            repository: "oinkvm/oink",
        },
        Channel::Stable,
        seen,
        now,
    )
}
#[test]
fn good_signature_selects_highest_compatible_build_without_downgrade() {
    let m = check(&payload(), &Watermark::default(), NOW).unwrap();
    let quarantine = BTreeSet::new();
    let mut s = Selection {
        channel: Channel::Stable,
        windows_build: 19045,
        architecture: "x86_64",
        installed_build: 19,
        successful_build: 19,
        quarantine: &quarantine,
    };
    assert_eq!(m.select(&s).unwrap().unwrap().build, 20);
    s.installed_build = 20;
    assert!(m.select(&s).unwrap().is_none());
    s.installed_build = 18;
    s.successful_build = 21;
    assert!(m.select(&s).unwrap().is_none());
    s.successful_build = 19;
    s.architecture = "arm64";
    assert!(m.select(&s).unwrap().is_none());
    s.architecture = "x86_64";
    s.windows_build = 1;
    assert!(m.select(&s).unwrap().is_none());
}
#[test]
fn tamper_every_payload_byte_wrong_key_and_invalid_signature() {
    let bytes = serde_json::to_vec(&payload()).unwrap();
    let env: Value = serde_json::from_slice(&signed_bytes(&bytes)).unwrap();
    let public = key().verifying_key().to_bytes();
    let trust = Trust {
        epoch: WINDOWS_EPOCH,
        key: &public,
        repository: "oinkvm/oink",
    };
    for i in 0..bytes.len() {
        let mut changed = bytes.clone();
        changed[i] ^= 1;
        let mut e = env.clone();
        e["payload"] = json!(STANDARD.encode(changed));
        assert_eq!(
            verify_manifest(
                &serde_json::to_vec(&e).unwrap(),
                &trust,
                Channel::Stable,
                &Watermark::default(),
                NOW
            )
            .unwrap_err(),
            Error::Signature
        );
    }
    let wrong = SigningKey::from_bytes(&[8; 32]).verifying_key().to_bytes();
    assert_eq!(
        verify_manifest(
            &envelope(&payload()),
            &Trust { key: &wrong, ..trust },
            Channel::Stable,
            &Watermark::default(),
            NOW
        )
        .unwrap_err(),
        Error::Signature
    );
}
#[test]
fn duplicate_keys_rejected_even_when_signed() {
    let bytes = serde_json::to_string(&payload())
        .unwrap()
        .replacen("\"schema\":1", "\"schema\":1,\"schema\":1", 1);
    assert!(
        verify_manifest(
            &signed_bytes(bytes.as_bytes()),
            &Trust {
                epoch: WINDOWS_EPOCH,
                key: &key().verifying_key().to_bytes(),
                repository: "oinkvm/oink"
            },
            Channel::Stable,
            &Watermark::default(),
            NOW
        )
        .is_err()
    );
    assert!(strict_json::<Value>(br#"{"a":{"x":1,"x":2}}"#).is_err());
}

#[test]
fn bilingual_notes_are_required_bounded_and_preserved_in_candidates() {
    let manifest = check(&payload(), &Watermark::default(), NOW).unwrap();
    let notes = &manifest.manifest().releases[0].notes;
    assert_eq!(notes.for_language("zh-Hans-CN"), "修复问题。");
    assert_eq!(notes.for_language("zh_CN"), "修复问题。");
    for language in ["en-US", "fr", "zh-Hant", "", "unknown"] {
        assert_eq!(notes.for_language(language), "Fix a problem.");
    }
    let candidate = oink_update::policy::Candidate {
        build: 20,
        version: "1.0.1".into(),
        size: 3,
        published_at: None,
        severity: Severity::Normal,
        notes: notes.clone(),
    };
    let value = serde_json::to_value(&candidate).unwrap();
    assert_eq!(value["notes"], payload()["releases"][0]["notes"]);
    assert_eq!(
        serde_json::from_value::<oink_update::policy::Candidate>(value).unwrap(),
        candidate
    );

    for invalid in [
        json!("one language"),
        json!({"en":"English only"}),
        json!({"zh-Hans":"仅中文"}),
        json!({"zh-Hans":"中文","en":null}),
        json!({"zh-Hans":"中文","en":"English","fr":"extra"}),
        json!({"zh-Hans":"中文","en":" "}),
        json!({"zh-Hans":"中\u{0085}文","en":"English"}),
        json!({"zh-Hans":"中文","en":"bad\rtext"}),
        json!({"zh-Hans":"中".repeat(MAX_NOTES / 3 + 1),"en":"English"}),
        json!({"zh-Hans":"中文","en":"x".repeat(MAX_NOTES + 1)}),
    ] {
        let mut changed = payload();
        changed["releases"][0]["notes"] = invalid;
        assert_eq!(
            check(&changed, &Watermark::default(), NOW).unwrap_err(),
            Error::Metadata
        );
    }
    let notes = ReleaseNotes {
        zh_hans: "中".repeat(MAX_NOTES / 3),
        en: "x".repeat(MAX_NOTES - 2) + "\n\t",
    };
    notes.validate().unwrap();
    assert_eq!(
        ReleaseNotes::from_json(&serde_json::to_vec(&notes).unwrap()).unwrap(),
        notes
    );
    assert!(ReleaseNotes::from_json(br#"{"zh-Hans":"a","en":"first","en":"second"}"#).is_err());
}
#[test]
fn sequence_expiration_clock_and_cached_install_revalidation() {
    let m = check(&payload(), &Watermark::default(), NOW).unwrap();
    assert!(check(&payload(), m.watermark(), NOW).is_ok());
    let mut p = payload();
    p["releases"][0]["notes"]["en"] = json!("changed");
    assert_eq!(check(&p, m.watermark(), NOW).unwrap_err(), Error::Replay);
    let mut p = payload();
    p["sequence"] = json!(2);
    let next = check(&p, m.watermark(), NOW + 1).unwrap();
    assert_eq!(check(&payload(), next.watermark(), NOW + 2).unwrap_err(), Error::Replay);
    assert_eq!(
        check(&payload(), &Watermark::default(), NOW + 30 * 86400).unwrap_err(),
        Error::ExpiredFeed
    );
    assert_eq!(check(&payload(), m.watermark(), NOW - 301).unwrap_err(), Error::Clock);
    assert_eq!(m.ensure_fresh(NOW + 30 * 86400), Err(Error::ExpiredFeed));
}
#[test]
fn product_epoch_channel_schema_and_fractional_build_rejected() {
    for (field, value) in [
        ("product", json!("other")),
        ("keyEpoch", json!("other")),
        ("channel", json!("beta")),
        ("schema", json!(2)),
    ] {
        let mut p = payload();
        p[field] = value;
        assert!(check(&p, &Watermark::default(), NOW).is_err());
    }
    let mut p = payload();
    p["releases"][0]["build"] = json!(20.5);
    assert!(check(&p, &Watermark::default(), NOW).is_err());
}
#[test]
fn stable_rejects_beta_beta_can_select_stable_and_returning_stable_waits() {
    let mut p = payload();
    p["releases"][0]["channel"] = json!("beta");
    assert!(check(&p, &Watermark::default(), NOW).is_err());
    p["channel"] = json!("beta");
    let mut stable = p["releases"][0].clone();
    stable["channel"] = json!("stable");
    stable["build"] = json!(21);
    p["releases"].as_array_mut().unwrap().push(stable);
    let public = key().verifying_key().to_bytes();
    let m = verify_manifest(
        &envelope(&p),
        &Trust {
            epoch: WINDOWS_EPOCH,
            key: &public,
            repository: "oinkvm/oink",
        },
        Channel::Beta,
        &Watermark::default(),
        NOW,
    )
    .unwrap();
    let q = BTreeSet::new();
    let s = Selection {
        channel: Channel::Beta,
        windows_build: 19045,
        architecture: "x86_64",
        installed_build: 19,
        successful_build: 19,
        quarantine: &q,
    };
    assert_eq!(m.select(&s).unwrap().unwrap().build, 21);
    let stable = check(&payload(), &Watermark::default(), NOW).unwrap();
    assert!(
        stable
            .select(&Selection {
                channel: Channel::Stable,
                installed_build: 21,
                ..s
            })
            .unwrap()
            .is_none()
    );
}
#[test]
fn package_signatures_and_lengths_bind_final_bytes() {
    use sha2::{Digest, Sha256};
    let bytes = b"zip final bytes";
    let p = Package {
        url: String::new(),
        length: bytes.len() as u64,
        sha256: hex(&Sha256::digest(bytes)),
        key_id: WINDOWS_EPOCH.into(),
        ed25519: STANDARD.encode(key().sign(bytes).to_bytes()),
    };
    let public = key().verifying_key().to_bytes();
    let t = Trust {
        epoch: WINDOWS_EPOCH,
        key: &public,
        repository: "oinkvm/oink",
    };
    assert!(verify_package(bytes, &p, &t).is_ok());
    assert!(verify_package(b"zip other bytes", &p, &t).is_err());
    assert!(verify_package(&bytes[..2], &p, &t).is_err());
}
#[test]
fn urls_and_redirects_are_closed_to_other_origins() {
    assert!(
        validate_asset_url(
            "https://github.com/oinkvm/oink/releases/download/v1/a.zip",
            "oinkvm/oink",
            "v1"
        )
        .is_ok()
    );
    for s in [
        "http://github.com/oinkvm/oink/releases/download/v1/a.zip",
        "https://github.com/evil/oink/releases/download/v1/a.zip",
        "https://u@github.com/oinkvm/oink/releases/download/v1/a.zip",
        "https://github.com/oinkvm/oink/releases/download/v1/a.zip?q=1",
        "file:///a.zip",
        "https://github.com/oinkvm/oink/releases/download/v1/%2e%2e/a.zip",
    ] {
        assert!(validate_asset_url(s, "oinkvm/oink", "v1").is_err());
    }
    assert!(validate_redirect("https://release-assets.githubusercontent.com/a?token=x", 5).is_ok());
    assert!(validate_redirect("https://release-assets.githubusercontent.com/a", 6).is_err());
    assert!(validate_redirect("https://github.com/evil/a", 1).is_err());
}
#[test]
fn rfc8032_empty_message_vector() {
    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }
    let public: [u8; 32] = unhex("d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a")
        .try_into()
        .unwrap();
    let sig = unhex(
        "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b",
    );
    verify_signature(b"", &STANDARD.encode(sig), &public).unwrap();
}
