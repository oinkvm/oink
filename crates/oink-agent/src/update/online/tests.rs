use super::*;
use base64::{Engine, engine::general_purpose::STANDARD};
use ed25519_dalek::{Signer, SigningKey};
use oink_update::MAC_EPOCH;
use serde_json::{Value, json};
use sha2::Digest;
use std::collections::HashMap;

const NOW: i64 = 1794268800; // 2026-11-10 UTC
const INSTALLED: u64 = 305001;
const PACKAGE: &[u8] = b"genuine notarized disk image bytes";
// TEST ONLY, derived in memory; never compiled into production trust.
fn key() -> SigningKey {
    SigningKey::from_bytes(&[7; 32])
}
fn public() -> [u8; 32] {
    key().verifying_key().to_bytes()
}
fn release(build: u64, channel: &str) -> Value {
    json!({"build":build,"version":format!("0.3.{}",build / 1000 % 100),"tag":format!("v{build}"),"channel":channel,
        "sourceTreeSha256":"a".repeat(64),"minimumOs":"14.2","protocolBaseline":1,"minimumUpdaterSchema":3,
        "notes":{"en":"Fixes","zh-Hans":"修复"},
        "package":{"url":format!("https://github.com/oinkvm/oink/releases/download/v{build}/Oink-macos-arm64.dmg"),
            "length":PACKAGE.len(),"sha256":oink_update::feed::hex(&sha2::Sha256::digest(PACKAGE)),"keyId":MAC_EPOCH,
            "ed25519":STANDARD.encode(key().sign(PACKAGE).to_bytes())}})
}
fn catalog(sequence: u64, rules: Value) -> Value {
    json!({"schema":2,"product":oink_update::PRODUCT,"platform":"mac","keyEpoch":MAC_EPOCH,"architecture":"arm64",
        "channel":"beta","sequence":sequence,"generatedAt":"2026-11-09T00:00:00Z","expiresAt":"2026-12-09T00:00:00Z",
        "releases":[release(306001,"stable"),release(306002,"stable"),release(307001,"beta")],"rules":rules})
}
fn normal() -> Value {
    json!([{"installed":[0,2147483647],"kind":"normal","target":"latest"}])
}
fn signed_by(payload: &Value, key: &SigningKey) -> Vec<u8> {
    let bytes = serde_json::to_vec(payload).unwrap();
    serde_json::to_vec(&json!({"envelopeSchema":1,"payload":STANDARD.encode(&bytes),
        "signatures":[{"keyId":MAC_EPOCH,"ed25519":STANDARD.encode(key.sign(&bytes).to_bytes())}]}))
    .unwrap()
}
fn signed(payload: &Value) -> Vec<u8> {
    signed_by(payload, &key())
}
#[derive(Default)]
struct Fake {
    catalogs: HashMap<String, Result<Vec<u8>, Error>>,
    package: Option<Vec<u8>>,
    asked: Vec<String>,
}
impl Transport for Fake {
    fn catalog(&mut self, url: &str) -> Result<Vec<u8>, Error> {
        self.asked.push(url.into());
        self.catalogs.get(url).cloned().unwrap_or(Err(Error::Network))
    }
    fn package(&mut self, url: &str, _: u64, progress: &mut dyn FnMut(u64, u64) -> bool) -> Result<Vec<u8>, Error> {
        self.asked.push(url.into());
        let bytes = self.package.clone().ok_or(Error::Network)?;
        for received in [bytes.len() as u64 / 2, bytes.len() as u64] {
            if !progress(received, bytes.len() as u64) {
                return Err(Error::Network);
            }
        }
        Ok(bytes)
    }
}
const PATH: &str = "mac/v2/mac-2026-01/arm64/beta/manifest.json";
fn sources() -> Vec<String> {
    vec![
        "https://a.example".into(),
        "https://b.example".into(),
        "https://c.example".into(),
    ]
}
fn run(fake: &mut Fake, skipped: Option<u64>, check: Check) -> Result<(Watermark, Result<Plan, Outcome>), Error> {
    let sources = sources();
    let key = public();
    let feed = Feed {
        sources: &sources,
        trust: Trust {
            epoch: MAC_EPOCH,
            key: &key,
        },
        architecture: "arm64",
        os: OsVersion([26, 1, 0]),
    };
    let seen = Watermark::default();
    let query = Query {
        channel: Channel::Beta,
        seen: &seen,
        now: NOW,
        installed_build: INSTALLED,
        skipped_build: skipped,
        check,
    };
    evaluate(&feed, &query, fake)
}
fn serve(fake: &mut Fake, source: &str, bytes: Result<Vec<u8>, Error>) {
    fake.catalogs.insert(format!("{source}/{PATH}"), bytes);
}
fn controller() -> (tempfile::TempDir, Controller) {
    let dir = tempfile::tempdir().unwrap();
    let mut controller = Controller::load(dir.path());
    controller.status.error = None;
    controller.status.phase = Phase::Idle;
    controller.status.checks_enabled = true;
    controller.online.due = Some(Instant::now());
    (dir, controller)
}
/// Runs one check through the controller with the given catalog rules.
fn offer(controller: &mut Controller, rules: Value) -> bool {
    let generation = controller.begin_check().unwrap().unwrap();
    let mut fake = Fake::default();
    serve(&mut fake, "https://a.example", Ok(signed(&catalog(1, rules))));
    let result = run(&mut fake, None, Check::Manual).map(|(_, plan)| plan);
    controller.finish_check(generation, result, NOW, Instant::now())
}
fn staged() -> local::Staged {
    let directory = tempfile::tempdir().unwrap();
    local::Staged {
        package: oink_update::local::Package {
            product: oink_update::PRODUCT.into(),
            version: "0.3.7".into(),
            build: 307001,
            architecture: "arm64".into(),
            source_tree_sha256: "a".repeat(64),
            minimum_windows_build: 0,
            length: PACKAGE.len() as u64,
            sha256: String::new(),
            notes: None,
        },
        path: directory.path().join("Oink.app"),
        _directory: directory,
    }
}

#[test]
fn sources_are_tried_in_order_until_a_catalog_verifies() {
    let mut fake = Fake::default();
    // a: unreachable; b: signed by another key; c: genuine.
    serve(
        &mut fake,
        "https://b.example",
        Ok(signed_by(&catalog(2, normal()), &SigningKey::from_bytes(&[8; 32]))),
    );
    serve(&mut fake, "https://c.example", Ok(signed(&catalog(1, normal()))));
    let (watermark, plan) = run(&mut fake, None, Check::Automatic).unwrap();
    assert_eq!(
        fake.asked,
        ["https://a.example", "https://b.example", "https://c.example"].map(|s| format!("{s}/{PATH}"))
    );
    assert_eq!(watermark.sequence, 1);
    assert_eq!(plan.unwrap().release.build, 307001);
    // Every source failing: a verification failure says more than the network.
    let mut fake = Fake::default();
    serve(
        &mut fake,
        "https://b.example",
        Ok(signed_by(&catalog(2, normal()), &SigningKey::from_bytes(&[8; 32]))),
    );
    assert_eq!(run(&mut fake, None, Check::Manual).unwrap_err(), Error::Signature);
    assert_eq!(
        run(&mut Fake::default(), None, Check::Manual).unwrap_err(),
        Error::Network
    );
}

#[test]
fn skip_applies_to_automatic_checks_of_normal_updates_only() {
    let mut fake = Fake::default();
    serve(&mut fake, "https://a.example", Ok(signed(&catalog(1, normal()))));
    assert_eq!(
        run(&mut fake, Some(307001), Check::Automatic).unwrap().1,
        Err(Outcome::UpToDate)
    );
    assert_eq!(
        run(&mut fake, Some(307001), Check::Manual)
            .unwrap()
            .1
            .unwrap()
            .release
            .build,
        307001
    );
}

#[test]
fn candidates_carry_kind_and_notice_and_manual_ones_are_never_downloaded() {
    let (_dir, mut controller) = controller();
    let critical = json!([{"installed":[0,2147483647],"kind":"critical","target":306002,
        "notice":{"en":"Security fix","zh-Hans":"安全修复"}}]);
    assert!(offer(&mut controller, critical));
    assert_eq!(controller.status.phase, Phase::Available);
    let candidate = controller.status.candidate.clone().unwrap();
    assert_eq!(
        (candidate.build, candidate.kind, candidate.severity),
        (306002, Kind::Critical, Severity::Critical)
    );
    assert_eq!(candidate.notice.unwrap().for_language("zh-Hans"), "安全修复");
    assert_eq!(candidate.size, PACKAGE.len() as u64);
    assert!(!candidate.local_package);
    assert_eq!(controller.status.checked_at, Some(NOW));

    let manual = json!([{"installed":[0,2147483647],"kind":"manual","target":"latest"}]);
    assert!(offer(&mut controller, manual));
    assert_eq!(controller.status.candidate.as_ref().unwrap().kind, Kind::Manual);
    assert_eq!(controller.begin_download().unwrap_err(), Error::Policy);
    assert_eq!(controller.status.phase, Phase::Available);

    assert!(offer(
        &mut controller,
        json!([{"installed":[0,2147483647],"kind":"none"}])
    ));
    assert_eq!(
        (controller.status.phase, controller.status.candidate.is_none()),
        (Phase::Idle, true)
    );
}

#[test]
fn a_release_needing_a_newer_macos_reports_os_unsupported() {
    let (_dir, mut controller) = controller();
    let mut payload = catalog(1, normal());
    for release in payload["releases"].as_array_mut().unwrap() {
        release["minimumOs"] = json!("27.0");
    }
    let generation = controller.begin_check().unwrap().unwrap();
    let mut fake = Fake::default();
    serve(&mut fake, "https://a.example", Ok(signed(&payload)));
    let result = run(&mut fake, None, Check::Manual).map(|(_, plan)| plan);
    assert!(controller.finish_check(generation, result, NOW, Instant::now()));
    assert_eq!(controller.status.phase, Phase::Failed);
    assert_eq!(controller.status.error, Some(Error::OsUnsupported));
    assert!(controller.status.candidate.is_none());
}

#[test]
fn packages_must_match_the_signed_length_hash_and_signature() {
    let payload = catalog(1, normal());
    let release: Release = serde_json::from_value(payload["releases"][2].clone()).unwrap();
    let key = public();
    let trust = Trust {
        epoch: MAC_EPOCH,
        key: &key,
    };
    let get = |bytes: &[u8]| {
        let mut fake = Fake {
            package: Some(bytes.to_vec()),
            ..Fake::default()
        };
        let result = fetch_package(&release, &trust, &mut fake, &mut |_, _| true);
        assert_eq!(
            fake.asked,
            std::slice::from_ref(&release.package.url),
            "only the signed URL"
        );
        result
    };
    assert_eq!(get(PACKAGE).unwrap(), PACKAGE);
    let mut tampered = PACKAGE.to_vec();
    tampered[0] ^= 1;
    assert_eq!(get(&tampered), Err(Error::Signature));
    assert_eq!(get(&PACKAGE[1..]), Err(Error::Metadata));
    // Same bytes and hash, but the signature is not by the Mac key.
    let mut forged = release.clone();
    forged.package.ed25519 = STANDARD.encode(SigningKey::from_bytes(&[8; 32]).sign(PACKAGE).to_bytes());
    let mut fake = Fake {
        package: Some(PACKAGE.to_vec()),
        ..Fake::default()
    };
    assert_eq!(
        fetch_package(&forged, &trust, &mut fake, &mut |_, _| true),
        Err(Error::Signature)
    );
}

#[test]
fn a_failed_download_marks_its_build_until_another_build_is_offered_or_it_succeeds() {
    let (_dir, mut controller) = controller();
    assert!(offer(&mut controller, normal()));
    let download = controller.begin_download().unwrap().unwrap();
    assert_eq!(controller.status.phase, Phase::Downloading);
    assert!(
        controller.begin_download().unwrap().is_none(),
        "a second click joins the download"
    );
    assert!(controller.download_progress(download.generation, 40));
    assert_eq!(controller.status.progress, Some(40));
    assert!(
        controller
            .finish_download(download.generation, 307001, Err(Error::Signature))
            .0
    );
    assert_eq!(controller.status.phase, Phase::Failed);
    assert_eq!(controller.status.error, Some(Error::Signature));
    assert_eq!(controller.status.failed_build, Some(307001));
    assert_eq!(
        controller.status.candidate.as_ref().unwrap().build,
        307001,
        "candidate kept"
    );
    // Rechecking the same build keeps the failure; a different build clears it.
    assert!(offer(&mut controller, normal()));
    assert_eq!(controller.status.failed_build, Some(307001));
    assert!(offer(
        &mut controller,
        json!([{"installed":[0,2147483647],"kind":"normal","target":306002}])
    ));
    assert_eq!(controller.status.failed_build, None);
    // A retried download of a failed build clears it once it is staged.
    assert!(offer(&mut controller, normal()));
    let download = controller.begin_download().unwrap().unwrap();
    controller.finish_download(download.generation, 307001, Err(Error::Network));
    assert_eq!(controller.status.failed_build, Some(307001));
    let retry = controller.begin_download().unwrap().unwrap();
    assert!(controller.download_verified(retry.generation));
    assert_eq!(controller.status.phase, Phase::Verifying);
    let (applied, superseded) = controller.finish_download(retry.generation, 307001, Ok(staged()));
    assert!(applied && superseded.is_none());
    assert_eq!(
        (controller.status.phase, controller.status.failed_build),
        (Phase::Ready, None)
    );
    assert!(controller.local.is_some() && !controller.local_mode);
}

#[test]
fn cancel_aborts_the_transfer_and_drops_a_late_result() {
    let (_dir, mut controller) = controller();
    assert!(offer(&mut controller, normal()));
    let download = controller.begin_download().unwrap().unwrap();
    controller.cancel_download();
    assert!(download.abort.load(Ordering::Acquire));
    assert_eq!(controller.status.phase, Phase::Available);
    assert_eq!(controller.status.progress, None);
    assert!(!controller.download_progress(download.generation, 90));
    let (applied, superseded) = controller.finish_download(download.generation, 307001, Ok(staged()));
    assert!(!applied && superseded.is_some());
    assert_eq!(
        (controller.status.phase, controller.status.failed_build),
        (Phase::Available, None)
    );
    // An aborted transfer stops at the next progress callback.
    let release: Release = serde_json::from_value(catalog(1, normal())["releases"][2].clone()).unwrap();
    let key = public();
    let mut fake = Fake {
        package: Some(PACKAGE.to_vec()),
        ..Fake::default()
    };
    let trust = Trust {
        epoch: MAC_EPOCH,
        key: &key,
    };
    assert_eq!(
        fetch_package(&release, &trust, &mut fake, &mut |_, _| false),
        Err(Error::Network)
    );
    // Discarding a downloaded package returns to the offer.
    let download = controller.begin_download().unwrap().unwrap();
    controller.finish_download(download.generation, 307001, Ok(staged()));
    controller.cancel_download();
    assert_eq!(controller.status.phase, Phase::Available);
    assert!(controller.local.is_none());
}

#[test]
fn superseding_drops_the_candidate_and_a_running_check() {
    let (_dir, mut controller) = controller();
    assert!(offer(&mut controller, normal()));
    let download = controller.begin_download().unwrap().unwrap();
    controller.supersede();
    assert!(download.abort.load(Ordering::Acquire));
    assert_eq!(
        (controller.status.phase, controller.status.candidate.is_none()),
        (Phase::Idle, true)
    );
    let stale = controller.begin_check().unwrap().unwrap();
    controller.supersede();
    let fresh = controller.begin_check().unwrap().unwrap();
    assert!(!controller.finish_check(stale, Err(Error::Network), NOW, Instant::now()));
    assert_eq!(controller.status.phase, Phase::Checking);
    assert!(controller.finish_check(fresh, Ok(Err(Outcome::UpToDate)), NOW, Instant::now()));
    assert_eq!(controller.status.phase, Phase::Idle);
}

#[test]
fn automatic_checks_run_at_launch_then_a_day_after_success_or_two_hours_after_failure() {
    let (_dir, mut controller) = controller();
    let launch = Instant::now();
    assert_eq!(controller.automatic_wait(launch), Some(Duration::ZERO), "launch check");
    controller.status.checks_enabled = false;
    assert_eq!(controller.automatic_wait(launch), None);
    controller.status.checks_enabled = true;

    let generation = controller.begin_check().unwrap().unwrap();
    assert!(controller.finish_check(generation, Ok(Err(Outcome::UpToDate)), NOW, launch));
    assert_eq!(controller.automatic_wait(launch), Some(AFTER_SUCCESS));
    assert!(controller.status.next_check.is_some());
    let generation = controller.begin_check().unwrap().unwrap();
    assert!(controller.finish_check(generation, Err(Error::Network), NOW, launch));
    assert_eq!(controller.automatic_wait(launch), Some(AFTER_FAILURE));
    assert_eq!(controller.status.error, Some(Error::Network));
    assert_eq!(
        controller.status.checked_at,
        Some(NOW),
        "a failed check keeps the last success"
    );
    // Turning checks on again runs one now; nothing runs while they are off.
    controller.status.checks_enabled = false;
    controller.publish_schedule();
    assert_eq!(controller.status.next_check, None);
    controller.status.checks_enabled = true;
    controller.schedule_now();
    assert_eq!(controller.automatic_wait(Instant::now()), Some(Duration::ZERO));
    // An unconfigured feed never schedules.
    let (_unconfigured_dir, mut unconfigured) = self::controller();
    unconfigured.online.due = None;
    unconfigured.schedule_now();
    assert_eq!(unconfigured.automatic_wait(Instant::now()), None);
}

#[test]
fn watermarks_are_kept_per_channel_in_the_update_state_file() {
    let (dir, mut controller) = controller();
    let mut fake = Fake::default();
    serve(&mut fake, "https://a.example", Ok(signed(&catalog(9, normal()))));
    let (watermark, _) = run(&mut fake, None, Check::Manual).unwrap();
    controller
        .policy
        .watermarks
        .insert(Channel::Beta.as_str().into(), watermark.clone());
    controller.save(dir.path()).unwrap();
    let saved: Value = serde_json::from_slice(&std::fs::read(dir.path().join("updates.json")).unwrap()).unwrap();
    assert_eq!(saved["watermarks"]["beta"]["sequence"], 9);
    let reloaded = Controller::load(dir.path());
    assert_eq!(reloaded.policy.watermarks.get("beta"), Some(&watermark));
    // An older catalog from a lagging mirror is a replay.
    let sources = sources();
    let key = public();
    let feed = Feed {
        sources: &sources,
        trust: Trust {
            epoch: MAC_EPOCH,
            key: &key,
        },
        architecture: "arm64",
        os: OsVersion([26, 1, 0]),
    };
    let mut fake = Fake::default();
    serve(&mut fake, "https://a.example", Ok(signed(&catalog(8, normal()))));
    let query = Query {
        channel: Channel::Beta,
        seen: &watermark,
        now: NOW,
        installed_build: INSTALLED,
        skipped_build: None,
        check: Check::Manual,
    };
    assert_eq!(evaluate(&feed, &query, &mut fake).unwrap_err(), Error::Replay);
}

#[cfg(target_os = "macos")]
#[test]
fn this_mac_reports_a_comparable_os_version() {
    assert!(os_version().unwrap() >= OsVersion([14, 2, 0]));
}

/// Download → verify → stage with a genuine notarized DMG served by an
/// injected transport. Stages into a private directory; never installs.
#[cfg(target_os = "macos")]
#[test]
#[ignore = "device: OINK_FEED_TEST_DMG names a genuine notarized Oink DMG for this architecture"]
fn genuine_dmg_downloads_verifies_and_stages_like_a_dropped_package() {
    let path = std::env::var("OINK_FEED_TEST_DMG").expect("genuine DMG");
    let dmg = std::fs::read(&path).unwrap();
    let build: u64 = std::env::var("OINK_FEED_TEST_BUILD")
        .expect("its CFBundleVersion")
        .parse()
        .unwrap();
    let mut payload = catalog(1, normal());
    let mut entry = release(build, "stable");
    entry["package"]["length"] = json!(dmg.len());
    entry["package"]["sha256"] = json!(oink_update::feed::hex(&sha2::Sha256::digest(&dmg)));
    entry["package"]["ed25519"] = json!(STANDARD.encode(key().sign(&dmg).to_bytes()));
    payload["architecture"] = json!(architecture());
    payload["releases"] = json!([entry]);
    let bytes_len = dmg.len() as u64;
    let mut fake = Fake {
        package: Some(dmg),
        ..Fake::default()
    };
    let sources = sources();
    let key = public();
    let trust = Trust {
        epoch: MAC_EPOCH,
        key: &key,
    };
    fake.catalogs.insert(
        format!(
            "https://a.example/mac/v2/{MAC_EPOCH}/{}/beta/manifest.json",
            architecture()
        ),
        Ok(signed(&payload)),
    );
    let feed = Feed {
        sources: &sources,
        trust: Trust {
            epoch: MAC_EPOCH,
            key: &key,
        },
        architecture: architecture(),
        os: os_version().unwrap(),
    };
    let seen = Watermark::default();
    let query = Query {
        channel: Channel::Beta,
        seen: &seen,
        now: NOW,
        installed_build: 1,
        skipped_build: None,
        check: Check::Automatic,
    };
    let plan = evaluate(&feed, &query, &mut fake).unwrap().1.unwrap();
    let mut seen_progress = vec![];
    let bytes = fetch_package(&plan.release, &trust, &mut fake, &mut |received, expected| {
        seen_progress.push((received, expected));
        true
    })
    .unwrap();
    assert_eq!(seen_progress.last(), Some(&(bytes_len, bytes_len)));
    let parent = tempfile::Builder::new().suffix(".noindex").tempdir().unwrap();
    let started = Instant::now();
    let staged = stage_into(&parent.path().join("Oink.app"), &bytes, &plan.release, "0.0.1", 1).unwrap();
    eprintln!("staged build {} in {:?}", staged.package.build, started.elapsed());
    assert_eq!(staged.package.build, build);
    super::super::local_mac::verify_app(&staged.path).unwrap();
    let mut wrong = plan.release.clone();
    wrong.build += 1;
    assert_eq!(
        stage_into(&parent.path().join("Oink.app"), &bytes, &wrong, "0.0.1", 1).unwrap_err(),
        Error::CorruptPackage
    );
    drop(staged);
    super::super::local_mac::wait_for_detach();
}

/// The production transport and compiled trust against the real sources.
#[cfg(target_os = "macos")]
#[test]
#[ignore = "network: contacts the production update origin"]
fn production_sources_answer_over_https_with_the_compiled_trust() {
    let appcast = Https
        .catalog("https://updates.oink.hachiwii.com/mac/v1/mac-2026-01/stable/appcast.xml")
        .unwrap();
    assert!(appcast.starts_with(b"<?xml"));
    let roots = oink_update::trust::mac_compiled().unwrap();
    let sources = oink_update::trust::sources().unwrap();
    let feed = Feed {
        sources: &sources,
        trust: roots.feed(),
        architecture: architecture(),
        os: os_version().unwrap(),
    };
    let seen = Watermark::default();
    let query = Query {
        channel: Channel::Stable,
        seen: &seen,
        now: now(),
        installed_build: 1,
        skipped_build: None,
        check: Check::Manual,
    };
    // Until the first v2 publication the catalog path is absent (HTTP 404).
    match evaluate(&feed, &query, &mut Https) {
        Ok((watermark, plan)) => eprintln!("v2 catalog sequence {}: {plan:?}", watermark.sequence),
        Err(error) => assert_eq!(error, Error::Network),
    }
}
