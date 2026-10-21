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
fn release_notes_are_bounded_and_preserved_in_candidates() {
    let manifest = check(&payload(), &Watermark::default(), NOW).unwrap();
    let notes = &manifest.manifest().releases[0].notes;
    assert_eq!(notes.for_language("zh-Hans-CN"), "修复问题。");
    assert_eq!(notes.for_language("zh_CN"), "Fix a problem.");
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
        json!({"zh-Hans":"仅中文"}),
        json!({"zh-Hans":"中文","en":null}),
        json!({"zh-Hans":"中文","en":"English","fr_X":"invalid tag"}),
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
    let notes = ReleaseNotes::from_json(
        &serde_json::to_vec(&json!({
            "zh-Hans":"中".repeat(MAX_NOTES / 3), "en":"x".repeat(MAX_NOTES - 2) + "\n\t"
        }))
        .unwrap(),
    )
    .unwrap();
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
        url: "https://bucket.account.r2.cloudflarestorage.com/Oink.msi?signature=abc".into(),
        length: bytes.len() as u64,
        sha256: hex(&Sha256::digest(bytes)),
        key_id: WINDOWS_EPOCH.into(),
        ed25519: STANDARD.encode(key().sign(bytes).to_bytes()),
    };
    let public = key().verifying_key().to_bytes();
    let t = Trust {
        epoch: WINDOWS_EPOCH,
        key: &public,
    };
    assert_eq!(validate_package_url(&p.url), Ok(()));
    assert_eq!(verify_package(bytes, &p, &t), Ok(()));
    assert_eq!(verify_package(b"zip other bytes", &p, &t), Err(Error::Signature));
    assert_eq!(verify_package(&bytes[..2], &p, &t), Err(Error::Metadata));
    let mut oversize = p.clone();
    oversize.length = MAX_PACKAGE + 1;
    assert_eq!(verify_package(bytes, &oversize, &t), Err(Error::Metadata));
}
#[test]
fn signed_packages_and_redirects_accept_any_https_host_without_credentials() {
    for url in [
        "https://github.com/oinkvm/oink/releases/download/v1/a.zip",
        "https://github.com/another/repository/releases/download/v2/a.msi",
        "https://bucket.account.r2.cloudflarestorage.com/windows/Oink%201.0.msi?X-Amz-Signature=abc",
        "https://downloads.example.test:9443/any/path?token=x#package",
        "HTTPS://cdn.example.test/package.msi",
    ] {
        assert_eq!(validate_package_url(url), Ok(()), "{url}");
        let mut manifest = payload();
        manifest["releases"][0]["package"]["url"] = url.into();
        assert!(check(&manifest, &Watermark::default(), NOW).is_ok(), "{url}");
        for hop in 0..=5 {
            assert_eq!(validate_redirect(url, hop), Ok(()), "{url}: {hop}");
        }
        assert_eq!(validate_redirect(url, 6), Err(Error::Url));
    }
    for url in [
        "http://downloads.example.test/a.msi",
        "https://user@downloads.example.test/a.msi",
        "https://user:password@downloads.example.test/a.msi",
        "https://:password@downloads.example.test/a.msi",
        "https://@downloads.example.test/a.msi",
        "file:///a.msi",
        "https:///",
    ] {
        assert_eq!(validate_package_url(url), Err(Error::Url), "{url}");
        assert_eq!(validate_redirect(url, 1), Err(Error::Url), "{url}");
    }
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

#[test]
fn human_version_cannot_decrease_even_when_build_increases() {
    assert_eq!(verify_version("0.1.0", "0.1.0-beta.test"), Ok(()));
    assert_eq!(verify_version("0.1.0+build.42", "0.1.0+build.41"), Ok(()));
    assert_eq!(verify_version("0.2.0", "0.1.0"), Ok(()));
    assert_eq!(verify_version("0.1.0", "0.2.0"), Err(Error::Replay));
    assert_eq!(verify_version("0.2.0-beta.1", "0.2.0"), Err(Error::Replay));
    assert_eq!(verify_version("invalid", "0.1.0"), Err(Error::Metadata));
}

#[test]
fn compiled_feed_origin_has_no_runtime_override_and_test_assets_stay_loopback() {
    #[cfg(not(feature = "test-feed"))]
    {
        assert_eq!(
            manifest_url(Channel::Stable),
            "https://updates.oink.hachiwii.com/windows/v1/win-2026-01/stable/manifest.json"
        );
        assert!(
            validate_package_url("http://127.0.0.1:8765/oink-update-qualification/packages/candidate.msi").is_err()
        );
    }
    #[cfg(feature = "test-feed")]
    {
        let origin = env!("OINK_WINDOWS_UPDATE_TEST_FEED");
        assert_eq!(
            manifest_url(Channel::Stable),
            format!("{origin}/oink-update-qualification/windows/v1/win-2026-01/stable/manifest.json")
        );
        assert!(validate_package_url(&format!("{origin}/oink-update-qualification/packages/candidate.msi")).is_ok());
        for suffix in ["../evil.msi", "file.msi?x", "file.exe", "file.msi/extra"] {
            assert!(validate_package_url(&format!("{origin}/oink-update-qualification/packages/{suffix}")).is_err());
        }
        assert!(
            validate_package_url("http://127.0.0.2:8765/oink-update-qualification/packages/candidate.msi").is_err()
        );
        assert_eq!(
            oink_update::trust::compiled().unwrap().key,
            base64::Engine::decode(&STANDARD, "11qYAYKxCrfVS/7TyWQHOg7hcvPapiMlrwIaaPcHURo=")
                .unwrap()
                .as_slice()
        );
    }
}

#[test]
fn health_fault_is_compiled_only_into_explicit_test_candidate() {
    assert!(!oink_update::trust::health_ready(false));
    #[cfg(feature = "test-feed")]
    assert_eq!(
        oink_update::trust::health_ready(true),
        env!("OINK_WINDOWS_UPDATE_TEST_FAIL_HEALTH") != "1"
    );
    #[cfg(not(feature = "test-feed"))]
    assert!(oink_update::trust::health_ready(true));
}

#[test]
fn notes_accept_future_languages_and_use_parent_then_english() {
    let mut p = payload();
    p["releases"][0]["notes"] = json!({"en":"English", "zh-Hans":"中文", "ja":"日本語", "ko":"한국어", "es":"Español", "es-MX":"México", "zh":"中文基础"});
    let manifest = check(&p, &Watermark::default(), NOW).unwrap();
    let notes = &manifest.manifest().releases[0].notes;
    for (language, expected) in [
        ("ja-JP", "日本語"),
        ("KO-kr", "한국어"),
        ("es-MX", "México"),
        ("es-AR", "Español"),
        ("zh-Hant-TW", "中文基础"),
        ("de-DE", "English"),
    ] {
        assert_eq!(notes.for_language(language), expected);
    }
    for language in ["x", "en_US", "en-", "en-a", "../ja"] {
        p["releases"][0]["notes"][language] = json!("invalid");
        assert!(check(&p, &Watermark::default(), NOW).is_err());
        p["releases"][0]["notes"].as_object_mut().unwrap().remove(language);
    }
}

#[test]
fn client_requires_english_and_accepts_an_english_only_future_release() {
    assert!(ReleaseNotes::from_json(br#"{"en":"English only"}"#).is_ok());
    assert!(ReleaseNotes::from_json(br#"{"ja":"missing English"}"#).is_err());
    assert!(ReleaseNotes::from_json(br#"{"en":"English","EN":"ambiguous"}"#).is_err());
    let mut notes = json!({"en":"English"});
    for i in 0..32 {
        notes[format!("zz-{i:02}")] = json!("test");
    }
    assert!(ReleaseNotes::from_json(&serde_json::to_vec(&notes).unwrap()).is_err());
}

#[test]
fn release_013_notes_parse_with_the_shipped_012_strict_type() {
    // Frozen shipped 0.1.2 type; future languages intentionally fail this type.
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct ShippedNotes {
        #[serde(rename = "zh-Hans")]
        zh_hans: String,
        en: String,
    }
    let bytes = include_bytes!("fixtures/release-notes-0.1.3.json");
    let old: ShippedNotes = serde_json::from_slice(bytes).unwrap();
    let current = ReleaseNotes::from_json(bytes).unwrap();
    assert_eq!(old.en, current.for_language("en"));
    assert_eq!(old.zh_hans, current.for_language("zh-Hans"));
    let mut later: Value = serde_json::from_slice(bytes).unwrap();
    later["ja"] = json!("future TEST translation");
    let bytes = serde_json::to_vec(&later).unwrap();
    assert!(serde_json::from_slice::<ShippedNotes>(&bytes).is_err());
    assert!(ReleaseNotes::from_json(&bytes).is_ok());
}

#[test]
fn notes_require_english_during_typed_deserialization_too() {
    assert!(serde_json::from_str::<ReleaseNotes>(r#"{"en":"first","en":"second"}"#).is_err());
    assert!(serde_json::from_str::<ReleaseNotes>(r#"{"ja":"missing English"}"#).is_err());
    assert!(serde_json::from_str::<ReleaseNotes>(r#"{"en":"English","EN":"ambiguous"}"#).is_err());
}
