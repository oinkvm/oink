//! Upgrade check against real 0.3.x data directories (DECISIONS D4: an upgrade
//! keeps identity and membership and does not migrate the stored formats).
//!
//! The data is never part of the repository. Run with one copied data
//! directory per machine below `OINK_UPGRADE_DATA`:
//!
//! ```text
//! OINK_UPGRADE_DATA=<dir> cargo test -p oink-agent --lib upgrade_data -- --ignored --nocapture
//! ```
//!
//! Each machine directory is first copied again into a temporary OINK_HOME;
//! only that copy is read by the v2 loaders, and both are hashed before and
//! after so a rewrite by any loader fails the test. A second temporary copy
//! then starts the real group module on a loopback-only link layer (no
//! beacon, no mDNS, address hints removed) and must come up with the same
//! group, members, layout version and layout profiles.
use super::*;
use oink_network::{group_state::State, pair_journal::Journal, roster::Anchor};
use oink_protocol::v2::schema::{WireDto as _, WireValue as _};
use std::path::Path;

fn hashes(root: &Path) -> BTreeMap<PathBuf, blake3::Hash> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, blake3::Hash>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(root, &path, out);
            } else {
                let relative = path.strip_prefix(root).unwrap().to_owned();
                out.insert(relative, blake3::hash(&std::fs::read(&path).unwrap()));
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let path = entry.unwrap().path();
        let target = to.join(path.file_name().unwrap());
        if path.is_dir() {
            copy_tree(&path, &target);
        } else {
            std::fs::copy(&path, &target).unwrap();
        }
    }
}

#[test]
#[ignore = "needs OINK_UPGRADE_DATA with copies of real 0.3.x data directories"]
fn upgrade_data_keeps_identity_membership_roster_and_layout() {
    let root = PathBuf::from(std::env::var_os("OINK_UPGRADE_DATA").expect("set OINK_UPGRADE_DATA"));
    let mut machines: Vec<_> = std::fs::read_dir(&root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.join(group_store::STATE_FILE).is_file())
        .collect();
    machines.sort();
    assert!(
        !machines.is_empty(),
        "no data directory with {}",
        group_store::STATE_FILE
    );
    let mut groups = BTreeSet::new();
    let mut failures = Vec::new();
    for source in machines {
        let label = source.file_name().unwrap().to_string_lossy().into_owned();
        let original = hashes(&source);
        let home = tempfile::tempdir().unwrap();
        copy_tree(&source, home.path());
        let before = hashes(home.path());
        assert_eq!(before, original);

        let identity = oink_network::identity::Identity::load_or_create(home.path()).unwrap();
        let device = identity.id();
        let stored = group_store::load(&home.path().join(group_store::STATE_FILE), device)
            .unwrap_or_else(|error| panic!("{label}: 0.3.x group state unreadable by v2: {error}"))
            .unwrap_or_else(|| panic!("{label}: group state missing"));
        let roster = stored.state.roster().unwrap();
        let anchor = Anchor {
            group: stored.group(),
            root: oink_network::roster::root_id(&roster).unwrap(),
        };
        let state = State::verify(stored.to_wire().unwrap(), anchor)
            .unwrap_or_else(|error| panic!("{label}: v2 rejects the stored roster: {error}"));
        // Startup replaces a state without this device by a new one-device
        // group; that would be "掉组" after the upgrade.
        assert!(state.roster().contains(device), "{label}: this device left its group");
        let members: Vec<_> = state.roster().members().collect();
        let os: Vec<_> = members.iter().map(|m| m.os.clone().unwrap_or_default()).collect();
        let register = stored.layout_register().unwrap();
        let layout = v2::layout::register_value(&register).unwrap();
        let layout_devices = layout.devices().unwrap().0;
        let screens: usize = layout_devices
            .iter()
            .map(|group| group.screens().map(|s| s.0.len()).unwrap_or(0))
            .sum();
        // Startup refuses to run without a readable pairing journal.
        let journal_path = home.path().join(oink_network::pair_journal::FILE);
        let statuses = journal_statuses(&journal_path);
        let format1_proofs = format1_proofs(&journal_path);
        let pending = match Journal::load(&journal_path, device) {
            Ok(journal) => journal.pending_projection().unwrap().is_some(),
            Err(error) => {
                failures.push(format!(
                    "{label}: pairing journal unreadable by v2 ({error}); transaction statuses {statuses:?}; bundle checks {:?}", bundle_checks(&journal_path)
                ));
                false
            }
        };
        let profiles = crate::layout_profiles::store::load(home.path(), crate::api::GroupKey(stored.group()))
            .unwrap_or_else(|error| panic!("{label}: layout profiles unreadable: {error:?}"));
        local_records::AddressStore::load(home.path()).unwrap();
        local_records::FactsCache::load(home.path()).unwrap();
        // What the first v2 start would still write (one-time, by design).
        let seeded = home.path().join("layout-visibility-seeded").exists();
        let legacy_layout = stored.has_legacy_layout().unwrap();
        let has_visibility = layout_wire::has_device_visibility_record(&register, device).unwrap();
        println!(
            "{label}: group={} members={} os={os:?} layout_devices={} screens={screens} \
             layout_version={:?} profiles={} pending_pairing_projection={pending} \
             legacy_layout={legacy_layout} visibility_record={has_visibility} seeded_marker={seeded} \
             transaction_statuses={statuses:?} format1_proofs={format1_proofs} \
             device_order={:?}",
            hex(&stored.group().0),
            members.len(),
            layout_devices.len(),
            register.version().and_then(|v| v.counter()).ok(),
            profiles.as_ref().map_or(0, |catalog| catalog.profiles.len()),
            state
                .device_order()
                .map(|order| order.into_values().collect::<Vec<_>>()),
        );
        groups.insert(stored.group());

        assert_eq!(hashes(home.path()), before, "{label}: a v2 loader rewrote stored data");

        if failures.is_empty() {
            let expected = Started {
                group: stored.group(),
                members: members.iter().map(|m| m.device).collect(),
                layout_version: register.version().unwrap(),
                // 0.3.6 stored a custom device order at register 0x1302.
                device_order: state
                    .device_order()
                    .unwrap_or_else(|error| panic!("{label}: stored device order unreadable: {error}")),
            };
            let (started, rewritten) = start_group_module(&source, &label);
            assert_eq!(
                started, expected,
                "{label}: the started group module differs from the stored data"
            );
            println!("{label}: group module started; files written by its first start: {rewritten:?}");
        }
        assert_eq!(hashes(&source), original, "{label}: the source copy changed");
    }
    println!("distinct groups: {}", groups.len());
    assert!(failures.is_empty(), "{failures:#?}");
}

/// M2: 0.3.x committed a merge (journal key 9) and stopped before projecting it.
/// The first 0.4.0 start applies that retained projection and clears key 9
/// instead of failing on every start.
#[test]
fn retained_legacy_projection_is_applied_on_the_first_start() {
    let fixture = oink_network::legacy_merge::test_fixture::fixture();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("device.key"), [2u8; 32]).unwrap();
    assert_eq!(
        oink_network::identity::Identity::load_or_create(home.path())
            .unwrap()
            .id(),
        fixture.target.id()
    );
    let stored = group_store::StoredGroupState::new(fixture.target.id(), fixture.target_base.clone()).unwrap();
    group_store::save(&home.path().join(group_store::STATE_FILE), &stored).unwrap();
    let mut journal = v2::cbor::decode_map(&fixture.journal, usize::MAX).unwrap();
    journal.insert(
        9,
        v2::RawValue::Map(
            [
                (1, oink_network::wire_identity::pair(fixture.pair).write()),
                (2, 1u64.write()),
                (3, fixture.bundle.target_state().unwrap().write()),
            ]
            .into(),
        ),
    );
    let journal_path = home.path().join(oink_network::pair_journal::FILE);
    std::fs::write(&journal_path, v2::cbor::encode_map(&journal, usize::MAX).unwrap()).unwrap();
    assert!(
        Journal::load(&journal_path, fixture.target.id())
            .unwrap()
            .pending_projection()
            .unwrap()
            .is_some()
    );
    let (started, _) = start_group_module(home.path(), "legacy-projection");
    assert!(started.members.contains(&fixture.source.id()), "{started:?}");
    assert!(started.members.contains(&fixture.target.id()));
}

#[derive(Debug, PartialEq)]
struct Started {
    group: GroupId,
    members: BTreeSet<DeviceId>,
    layout_version: dto::ConfigVersion,
    device_order: BTreeMap<DeviceId, i64>,
}

struct Application;
impl ConfigApplication for Application {
    fn apply_state(&self, _state: &group_store::StoredGroupState) -> Result<(), String> {
        Ok(())
    }
}

/// Start the production group module from a fresh copy of `source`. Address
/// hints are removed and the link layer is bound to loopback only, so the
/// owner cannot reach the real peers recorded in the copy.
fn start_group_module(source: &Path, label: &str) -> (Started, Vec<PathBuf>) {
    let home = tempfile::tempdir().unwrap();
    copy_tree(source, home.path());
    for file in [
        local_records::ADDRESSES_FILE,
        local_records::FACTS_FILE,
        crate::config::records::PRESENCE_FILE,
    ] {
        let _ = std::fs::remove_file(home.path().join(file));
    }
    let before = hashes(home.path());
    let group_key = |stored: &group_store::StoredGroupState| crate::api::GroupKey(stored.group());
    let identity = Arc::new(oink_network::identity::Identity::load_or_create(home.path()).unwrap());
    let stored = group_store::load(&home.path().join(group_store::STATE_FILE), identity.id())
        .unwrap()
        .unwrap();
    let profiles = crate::layout_profiles::store::load(home.path(), group_key(&stored)).unwrap();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let started = runtime.block_on(async {
        let network = oink_test_support::network::network_config(home.path().into(), "upgrade-check", 0);
        let (links, port) = LinkLayer::start(oink_network::link::LinkConfig {
            profile: crate::node::Node::offer_profile(),
            identity: identity.clone(),
            boot: Boot {
                device_id: identity.id(),
                nonce: [7; 16],
            },
            bind: (network.bind_ip, network.port).into(),
            tier: oink_foundation::realtime::Tier::Elevated,
        })
        .await
        .unwrap();
        let config = Arc::new(std::sync::Mutex::new(
            crate::config::AgentConfig::load_from_with_device_name(home.path(), "upgrade-check".into()).unwrap(),
        ));
        let group = ConfigModule::start_with_config(
            ConfigDeps {
                data_dir: home.path().into(),
                identity: identity.clone(),
                links: links.clone(),
                port,
                application: Arc::new(Application),
                pairing: true,
                config,
                owner_gate: Arc::new(std::sync::Mutex::new(())),
            },
            network,
        )
        .unwrap_or_else(|error| panic!("{label}: the group module does not start from the 0.3.x data: {error:?}"));
        let applied = group.applied();
        group.stop().await;
        links.stop().await;
        Started {
            group: applied.group,
            members: applied.members.iter().copied().collect(),
            layout_version: applied.version.clone(),
            device_order: applied.device_order.clone(),
        }
    });
    // The journal the module started from is still readable afterwards, and
    // the stored layout profiles are untouched.
    Journal::load(&home.path().join(oink_network::pair_journal::FILE), identity.id())
        .unwrap_or_else(|error| panic!("{label}: pairing journal unreadable after start: {error}"));
    let stored_after = group_store::load(&home.path().join(group_store::STATE_FILE), identity.id())
        .unwrap()
        .unwrap();
    assert_eq!(
        stored_after.group(),
        stored.group(),
        "{label}: the started module left its group"
    );
    assert_eq!(
        crate::layout_profiles::store::load(home.path(), group_key(&stored_after)).unwrap(),
        profiles,
        "{label}: layout profiles changed on start"
    );
    let after = hashes(home.path());
    let rewritten = after
        .iter()
        .filter(|(path, hash)| before.get(*path) != Some(*hash))
        .map(|(path, _)| path.clone())
        .collect();
    (started, rewritten)
}

/// Retained committed rows whose proof carries the 0.3.x format-1 fields.
fn format1_proofs(path: &Path) -> usize {
    let Ok(bytes) = std::fs::read(path) else {
        return 0;
    };
    let Ok(raw) = v2::cbor::decode_map(&bytes, bytes.len()) else {
        return 0;
    };
    let Some(v2::RawValue::Array(rows)) = raw.get(&3) else {
        return 0;
    };
    rows.iter()
        .filter_map(|row| dto::MergeTransferBundle::read(row.as_map().ok()?.get(&8)?).ok())
        .filter(|bundle| bundle.source_roster().is_ok_and(|roster| roster.raw().contains_key(&2)))
        .count()
}

/// Which step of the v2 bundle verification rejects each retained row.
fn bundle_checks(path: &Path) -> Vec<String> {
    use oink_network::group_merge;
    use oink_protocol::v2::schema::WireValue;
    let Ok(bytes) = std::fs::read(path) else {
        return Vec::new();
    };
    let Ok(raw) = v2::cbor::decode_map(&bytes, bytes.len()) else {
        return Vec::new();
    };
    let Some(v2::RawValue::Array(rows)) = raw.get(&3) else {
        return Vec::new();
    };
    rows.iter()
        .filter_map(|row| {
            let row = row.as_map().ok()?;
            let bundle = dto::MergeTransferBundle::read(row.get(&8)?).ok()?;
            let step = (|| -> Result<&'static str, v2::Error> {
                let certificate = bundle.certificate()?;
                let intent = certificate.intent()?;
                let plan = bundle.plan()?;
                if group_merge::digest(&plan)? != certificate.plan_digest()?.0 {
                    return Ok("plan digest differs");
                }
                let anchor = group_merge::anchor(&intent.source()?)?;
                if let Err(error) = oink_network::roster::Roster::verify(bundle.source_roster()?, anchor) {
                    return Ok(if error.code == v2::ErrorCode::AuthenticationFailed {
                        "source roster"
                    } else {
                        "source roster (other)"
                    });
                }
                Ok(match group_merge::verify_bundle(&bundle, anchor) {
                    Ok(_) => "ok",
                    Err(_) => "later step (result state/digest)",
                })
            })();
            Some(format!("{:?}", step.map_err(|e| e.to_string())))
        })
        .collect()
}

/// Transaction status per row (0 prepared, 1 committed, 2 aborted,
/// 3 unconfirmed, 4 historical proof), read without validation.
fn journal_statuses(path: &Path) -> Vec<u64> {
    let Ok(bytes) = std::fs::read(path) else {
        return Vec::new();
    };
    let Ok(raw) = v2::cbor::decode_map(&bytes, bytes.len()) else {
        return Vec::new();
    };
    match raw.get(&3) {
        Some(v2::RawValue::Array(rows)) => rows
            .iter()
            .filter_map(|row| match row.as_map().ok()?.get(&6)? {
                v2::RawValue::Uint(status) => Some(*status),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
