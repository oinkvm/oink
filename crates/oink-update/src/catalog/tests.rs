use super::*;
use crate::WINDOWS_EPOCH;
use base64::{Engine, engine::general_purpose::STANDARD};
use ed25519_dalek::{Signer, SigningKey};
use serde_json::{Value, json};
const NOW: i64 = 1794268800; // 2026-11-10 UTC
// TEST ONLY, derived in memory; never compiled into production trust.
fn key() -> SigningKey {
    SigningKey::from_bytes(&[9; 32])
}
fn release(build: u64, channel: &str) -> Value {
    json!({"build":build,"version":format!("0.3.{}",build % 1000),"tag":format!("v{build}"),"channel":channel,
        "sourceTreeSha256":"a".repeat(64),"minimumOs":"10.0.17763","protocolBaseline":1,"minimumUpdaterSchema":3,
        "notes":{"en":"Fixes","zh-Hans":"修复"},
        "package":{"url":format!("https://github.com/oinkvm/oink/releases/download/v{build}/Oink.msi"),"length":3,
            "sha256":"b".repeat(64),"keyId":WINDOWS_EPOCH,"ed25519":STANDARD.encode([0;64])}})
}
fn payload() -> Value {
    json!({"schema":2,"product":PRODUCT,"platform":"windows","keyEpoch":WINDOWS_EPOCH,"architecture":"x64",
        "channel":"beta","sequence":5,"generatedAt":"2026-11-09T00:00:00Z","expiresAt":"2026-12-09T00:00:00Z",
        "releases":[release(305001,"stable"),release(306002,"stable"),release(307001,"beta")],
        "rules":[{"installed":[0,2147483647],"kind":"normal","target":"latest"}]})
}
fn verify_as(p: &Value, channel: Channel, seen: &Watermark) -> Result<VerifiedCatalog, Error> {
    let bytes = serde_json::to_vec(p).unwrap();
    let envelope = serde_json::to_vec(&json!({"envelopeSchema":1,"payload":STANDARD.encode(&bytes),
        "signatures":[{"keyId":WINDOWS_EPOCH,"ed25519":STANDARD.encode(key().sign(&bytes).to_bytes())}]}))
    .unwrap();
    let expected = Expected {
        platform: Platform::Windows,
        architecture: "x64",
        channel,
    };
    let trust = Trust {
        epoch: WINDOWS_EPOCH,
        key: &key().verifying_key().to_bytes(),
    };
    verify(&envelope, &trust, &expected, seen, NOW)
}
fn check(p: &Value) -> Result<VerifiedCatalog, Error> {
    verify_as(p, Channel::Beta, &Watermark::default())
}
fn selection(quarantine: &BTreeSet<u64>) -> Selection<'_> {
    Selection {
        installed_build: 305001,
        successful_build: 305001,
        quarantine,
        channel: Channel::Beta,
        os: OsVersion([10, 0, 26200]),
        skipped_build: None,
        check: Check::Automatic,
    }
}
fn rules(p: &mut Value, rules: Value) -> &mut Value {
    p["rules"] = rules;
    p
}

#[test]
fn catalog_identity_must_match_the_installation() {
    assert!(check(&payload()).is_ok());
    for (field, value) in [
        ("platform", json!("mac")),
        ("architecture", json!("arm64")),
        ("schema", json!(1)),
        ("product", json!("com.example")),
        ("keyEpoch", json!("win-2099-01")),
        ("sequence", json!(0)),
    ] {
        let mut p = payload();
        p[field] = value;
        assert!(matches!(check(&p), Err(Error::Policy)), "{field}");
    }
    assert!(matches!(
        verify_as(&payload(), Channel::Stable, &Watermark::default()),
        Err(Error::Policy)
    ));
}
#[test]
fn stable_catalogs_cannot_carry_beta_releases() {
    let mut p = payload();
    p["channel"] = json!("stable");
    assert!(matches!(
        verify_as(&p, Channel::Stable, &Watermark::default()),
        Err(Error::Metadata)
    ));
    p["releases"].as_array_mut().unwrap().pop();
    assert!(verify_as(&p, Channel::Stable, &Watermark::default()).is_ok());
}
#[test]
fn malformed_rules_and_releases_are_rejected() {
    let bad_rules = [
        json!([]),
        json!([{"installed":[5,4],"kind":"normal","target":"latest"}]),
        json!([{"installed":[0,9],"kind":"normal"}]),
        json!([{"installed":[0,9],"kind":"none","target":"latest"}]),
        json!([{"installed":[0,9],"kind":"normal","target":999}]),
        json!([{"installed":[0,9],"kind":"normal","target":"newest"}]),
        json!([{"installed":[0,9],"kind":"normal","target":"latest","notice":{"en":"x"}}]),
        json!([{"installed":[0,9],"kind":"critical","target":"latest","notice":{"zh-Hans":"no english"}}]),
        json!([{"installed":[0,9],"kind":"sometimes","target":"latest"}]),
        json!([{"installed":[0,9],"kind":"normal","target":"latest","extra":1}]),
    ];
    for bad in bad_rules {
        let mut p = payload();
        assert!(
            matches!(check(rules(&mut p, bad.clone())), Err(Error::Metadata)),
            "{bad}"
        );
    }
    for (field, value) in [
        ("minimumOs", json!("ten")),
        ("minimumOs", json!("10")),
        ("build", json!(306002)),
        ("sourceTreeSha256", json!("A".repeat(64))),
        ("tag", json!("")),
    ] {
        let mut p = payload();
        p["releases"][0][field] = value;
        assert!(matches!(check(&p), Err(Error::Metadata)), "{field}");
    }
    let mut p = payload();
    p["releases"][0]["package"]["url"] = json!("http://example.com/Oink.msi");
    assert!(matches!(check(&p), Err(Error::Url)));
}
#[test]
fn replayed_or_expired_catalogs_are_rejected() {
    let first = check(&payload()).unwrap();
    let mut older = payload();
    older["sequence"] = json!(4);
    assert!(matches!(
        verify_as(&older, Channel::Beta, first.watermark()),
        Err(Error::Replay)
    ));
    let mut changed = payload();
    changed["releases"].as_array_mut().unwrap().pop();
    assert!(matches!(
        verify_as(&changed, Channel::Beta, first.watermark()),
        Err(Error::Replay)
    ));
    assert!(verify_as(&payload(), Channel::Beta, first.watermark()).is_ok());
    let mut expired = payload();
    expired["generatedAt"] = json!("2026-10-01T00:00:00Z");
    expired["expiresAt"] = json!("2026-10-31T00:00:00Z");
    assert!(matches!(check(&expired), Err(Error::ExpiredFeed)));
}
#[test]
fn the_first_matching_rule_routes_each_installed_build() {
    let mut p = payload();
    rules(
        &mut p,
        json!([
            {"installed":[0,305999],"kind":"manual","target":"latest"},
            {"installed":[306000,306001],"kind":"critical","target":306002,"notice":{"en":"Security fix"}},
            {"installed":[306002,306002],"kind":"none"},
            {"installed":[0,2147483647],"kind":"normal","target":"latest"}
        ]),
    );
    let c = check(&p).unwrap();
    let quarantine = BTreeSet::new();
    let mut s = selection(&quarantine);
    let plan = c.plan(&s).unwrap();
    assert_eq!((plan.release.build, plan.kind), (307001, Kind::Manual));
    s.installed_build = 306001;
    let plan = c.plan(&s).unwrap();
    assert_eq!((plan.release.build, plan.kind), (306002, Kind::Critical));
    assert_eq!(plan.notice.unwrap().for_language("en"), "Security fix");
    s.installed_build = 306002;
    assert_eq!(c.plan(&s), Err(Outcome::UpToDate));
    s.installed_build = 306003;
    let plan = c.plan(&s).unwrap();
    assert_eq!(
        (plan.release.build, plan.kind, plan.notice),
        (307001, Kind::Normal, None)
    );
    // No matching rule offers nothing.
    let mut p = payload();
    rules(&mut p, json!([{"installed":[0,10],"kind":"normal","target":"latest"}]));
    assert_eq!(check(&p).unwrap().plan(&selection(&quarantine)), Err(Outcome::UpToDate));
}
#[test]
fn channel_quarantine_and_success_watermark_limit_the_latest_target() {
    let c = check(&payload()).unwrap();
    let mut quarantine = BTreeSet::new();
    let mut s = selection(&quarantine);
    assert_eq!(c.plan(&s).unwrap().release.build, 307001);
    s.channel = Channel::Stable;
    assert_eq!(c.plan(&s).unwrap().release.build, 306002);
    s.successful_build = 306002;
    assert_eq!(c.plan(&s), Err(Outcome::UpToDate));
    quarantine.insert(307001);
    let mut s = selection(&quarantine);
    assert_eq!(c.plan(&s).unwrap().release.build, 306002);
    s.installed_build = 307001;
    assert_eq!(c.plan(&s), Err(Outcome::UpToDate));
}
#[test]
fn skipping_only_silences_automatic_checks_of_a_normal_update() {
    let mut p = payload();
    rules(
        &mut p,
        json!([
            {"installed":[0,305000],"kind":"critical","target":"latest"},
            {"installed":[0,2147483647],"kind":"normal","target":"latest"}
        ]),
    );
    let c = check(&p).unwrap();
    let quarantine = BTreeSet::new();
    let mut s = selection(&quarantine);
    s.skipped_build = Some(307001);
    assert_eq!(c.plan(&s), Err(Outcome::UpToDate));
    s.check = Check::Manual;
    assert_eq!(c.plan(&s).unwrap().release.build, 307001);
    s.check = Check::Automatic;
    s.installed_build = 305000;
    assert_eq!(c.plan(&s).unwrap().kind, Kind::Critical);
    // A newer build than the skipped one is offered again.
    s.installed_build = 305001;
    s.skipped_build = Some(306002);
    assert_eq!(c.plan(&s).unwrap().release.build, 307001);
}
#[test]
fn manual_targets_ignore_updater_capability_but_not_the_operating_system() {
    let mut p = payload();
    p["releases"][2]["minimumUpdaterSchema"] = json!(UPDATER_CAPABILITY + 1);
    let normal = check(&p).unwrap();
    let quarantine = BTreeSet::new();
    let s = selection(&quarantine);
    assert_eq!(normal.plan(&s).unwrap().release.build, 306002);
    rules(
        &mut p,
        json!([{"installed":[0,2147483647],"kind":"manual","target":"latest"}]),
    );
    let manual = check(&p).unwrap();
    assert_eq!(manual.plan(&s).unwrap().release.build, 307001);
    p["releases"][2]["minimumOs"] = json!("10.0.99999");
    assert_eq!(check(&p).unwrap().plan(&s).unwrap().release.build, 306002);
}
#[test]
fn a_target_needing_a_newer_os_is_reported() {
    let mut p = payload();
    for release in p["releases"].as_array_mut().unwrap() {
        release["minimumOs"] = json!("10.0.30000");
    }
    let c = check(&p).unwrap();
    let quarantine = BTreeSet::new();
    let mut s = selection(&quarantine);
    assert_eq!(c.plan(&s), Err(Outcome::OsUnsupported));
    s.os = OsVersion([10, 0, 30000]);
    assert_eq!(c.plan(&s).unwrap().release.build, 307001);
}
#[test]
fn os_versions_compare_numerically() {
    assert!(OsVersion::parse("14.2").unwrap() < OsVersion::parse("14.10").unwrap());
    assert_eq!(OsVersion::parse("14.2").unwrap(), OsVersion([14, 2, 0]));
    assert!(OsVersion::parse("10.0.17763").unwrap() < OsVersion([10, 0, 26200]));
    for bad in ["", "14", "14.", "1.2.3.4", "a.b", "14.-1", "1234567890.0"] {
        assert!(OsVersion::parse(bad).is_err(), "{bad}");
    }
}
#[test]
fn catalog_paths_name_platform_epoch_architecture_and_channel() {
    assert_eq!(
        path(Platform::Windows, WINDOWS_EPOCH, "x64", Channel::Stable),
        "windows/v2/win-2026-01/x64/stable/manifest.json"
    );
    assert_eq!(
        path(Platform::Mac, crate::MAC_EPOCH, "arm64", Channel::Beta),
        "mac/v2/mac-2026-01/arm64/beta/manifest.json"
    );
}
