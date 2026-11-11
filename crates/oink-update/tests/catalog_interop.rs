//! Catalogs produced by the Python publisher (build-support/update/feed_v2.py)
//! with the RFC 8032 test key verify and route here exactly as published.
use base64::{Engine, engine::general_purpose::STANDARD};
use oink_update::{
    MAC_EPOCH, WINDOWS_EPOCH,
    catalog::*,
    feed::{Channel, Trust, Watermark, verify_package},
};
use std::collections::BTreeSet;
const NOW: i64 = 1794355200; // 2026-11-11 UTC, inside the fixtures' validity
fn key() -> [u8; 32] {
    STANDARD
        .decode("11qYAYKxCrfVS/7TyWQHOg7hcvPapiMlrwIaaPcHURo=")
        .unwrap()
        .try_into()
        .unwrap()
}
fn selection(quarantine: &BTreeSet<u64>, installed: u64, channel: Channel, os: OsVersion) -> Selection<'_> {
    Selection {
        installed_build: installed,
        successful_build: installed,
        quarantine,
        channel,
        os,
        skipped_build: None,
        check: Check::Automatic,
    }
}
#[test]
fn published_windows_beta_catalog_verifies_and_routes() {
    let key = key();
    let trust = Trust {
        epoch: WINDOWS_EPOCH,
        key: &key,
    };
    let expected = Expected {
        platform: Platform::Windows,
        architecture: "x64",
        channel: Channel::Beta,
    };
    let bytes = include_bytes!("fixtures/catalog-v2/windows-x64-beta.manifest.json");
    let catalog = verify(bytes, &trust, &expected, &Watermark::default(), NOW).unwrap();
    let quarantine = BTreeSet::new();
    let os = OsVersion([10, 0, 26200]);
    let manual = catalog.plan(&selection(&quarantine, 113, Channel::Beta, os)).unwrap();
    assert_eq!((manual.release.build, manual.kind), (308001, Kind::Manual));
    let critical = catalog
        .plan(&selection(&quarantine, 306002, Channel::Beta, os))
        .unwrap();
    assert_eq!((critical.release.build, critical.kind), (308001, Kind::Critical));
    assert!(critical.notice.is_some());
    let normal = catalog
        .plan(&selection(&quarantine, 307001, Channel::Beta, os))
        .unwrap();
    assert_eq!((normal.release.build, normal.kind), (308001, Kind::Normal));
    assert_eq!(
        catalog.plan(&selection(&quarantine, 308001, Channel::Beta, os)),
        Err(Outcome::UpToDate)
    );
    verify_package(
        include_bytes!("fixtures/catalog-v2/windows-package-308001.bin"),
        &normal.release.package,
        &trust,
    )
    .unwrap();
    let other = Expected {
        channel: Channel::Stable,
        ..expected
    };
    assert!(verify(bytes, &trust, &other, &Watermark::default(), NOW).is_err());
}
#[test]
fn published_mac_stable_catalog_verifies_and_signs_the_dmg_bytes() {
    let key = key();
    let trust = Trust {
        epoch: MAC_EPOCH,
        key: &key,
    };
    let expected = Expected {
        platform: Platform::Mac,
        architecture: "arm64",
        channel: Channel::Stable,
    };
    let catalog = verify(
        include_bytes!("fixtures/catalog-v2/mac-arm64-stable.manifest.json"),
        &trust,
        &expected,
        &Watermark::default(),
        NOW,
    )
    .unwrap();
    let quarantine = BTreeSet::new();
    let plan = catalog
        .plan(&selection(&quarantine, 306500, Channel::Stable, OsVersion([14, 7, 0])))
        .unwrap();
    assert_eq!((plan.release.build, plan.kind), (307001, Kind::Critical));
    verify_package(
        include_bytes!("fixtures/catalog-v2/mac-package-307001.bin"),
        &plan.release.package,
        &trust,
    )
    .unwrap();
    assert_eq!(
        catalog.plan(&selection(&quarantine, 306500, Channel::Stable, OsVersion([14, 1, 0]))),
        Err(Outcome::OsUnsupported)
    );
}
