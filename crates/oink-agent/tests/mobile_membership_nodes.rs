#![cfg(debug_assertions)]
//! A phone's group-member obligations against real desktop owners: merge
//! adoption, revocation, permit release, decision recovery, supersede and the
//! phone's pairing-page admission. Real nodes and QUIC/TLS on loopback; fake OS
//! backends, no device validation claim.
use oink_agent as test_agent;
mod common;
use oink_agent::{
    api::{AgentApi, pairing},
    device_cards::CardConnection,
    node::{Node, NodeOptions},
};
use oink_foundation::{DeviceId, Point, Rect, Size, display::DisplayInfo, session::Boot};
use oink_mobile_core::{MobileNetwork, MobileNetworkConfig};
use oink_network::{group_state::State, identity::Identity, pairing as core};
use oink_protocol::v2::{self, RawValue, dto, limits, schema::WireDto};
use oink_test_support::input::FakeInput;
use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
};

fn options(root: &Path, name: &str) -> NodeOptions {
    oink_agent::logs::init();
    let input = FakeInput::new();
    input.set_cursor(Point::new(400., 300.));
    NodeOptions {
        net: oink_test_support::network_config(root.join(name), name, 0),
        input: Arc::new(input),
        displays: vec![DisplayInfo {
            key: "main".into(),
            name: "main".into(),
            bounds: Rect::new(0., 0., 800., 600.),
            pixel_width: 800,
            pixel_height: 600,
            reference_extent: Some(Size::new(300., 200.)),
            rotation_deg: 0,
            is_primary: true,
            is_builtin: false,
            is_virtual: false,
        }],
        display_backend: None,
        clipboard: None,
        mode_switch_in_callback: true,
        capture_reports_key_repeat: true,
        cursor_trail: false,
    }
}

/// A phone's durable files. Restarting keeps identity, group state and journal.
struct Phone {
    net: MobileNetwork,
    dir: PathBuf,
    seed: [u8; 32],
    nonce: u8,
}

impl Phone {
    async fn start(dir: PathBuf, seed: [u8; 32]) -> Self {
        let identity = Identity::from_seed(seed);
        let state = State::create(&identity, "test phone", "android", 1).unwrap();
        Self::boot(dir, seed, Some(state.wire().encode(limits::CONFIG_STATE).unwrap()), 1).await
    }
    async fn boot(dir: PathBuf, seed: [u8; 32], state: Option<Vec<u8>>, nonce: u8) -> Self {
        let identity = Identity::from_seed(seed);
        let net = MobileNetwork::start(MobileNetworkConfig {
            identity_seed: seed,
            boot: Boot {
                device_id: identity.id(),
                nonce: [nonce; 16],
            },
            bind: "127.0.0.1:0".parse().unwrap(),
            group_state: state,
            group_state_path: dir.join("group-state.v2"),
            pairing_journal_path: dir.join("pairing-recovery.cbor"),
            device_name: "test phone".into(),
            os: "android".into(),
            form_factor: None,
            trusted_peers: Vec::new(),
            trust_path: None,
            process: oink_mobile_core::MobileProcess::App,
        })
        .await
        .unwrap();
        Self { net, dir, seed, nonce }
    }
    async fn restart(self) -> Self {
        self.net.stop().await;
        Self::boot(self.dir, self.seed, None, self.nonce + 1).await
    }
    fn id(&self) -> DeviceId {
        Identity::from_seed(self.seed).id()
    }
    fn addr(&self) -> SocketAddr {
        ([127, 0, 0, 1], self.net.local_addr().port()).into()
    }
    fn members(&self) -> Vec<DeviceId> {
        self.net.members().into_iter().map(|m| m.device).collect()
    }
    fn session(&self, pair: &str) -> Option<core::View> {
        self.net
            .pairing_snapshot()
            .unwrap()
            .sessions
            .into_iter()
            .find(|view| core::key(view.id) == pair)
    }
    fn request(&self, pair: &str) -> Option<core::View> {
        self.net
            .pairing_snapshot()
            .unwrap()
            .requests
            .into_iter()
            .find(|view| core::key(view.id) == pair)
    }
    /// The user takes a listed request: code entry follows.
    async fn accept(&self, pair: &str) {
        common::until("the request reaches the phone's request list", || {
            self.request(pair).is_some()
        })
        .await;
        let request = self.request(pair).unwrap();
        self.net.accept_pairing(pair, request.revision).unwrap();
        assert!(
            self.session(pair)
                .is_some_and(|view| view.phase == core::Phase::EnterCode)
        );
    }
    async fn connect(&self, desktop: &Node) {
        let _ = self.net.dial(desktop.id(), vec![addr(desktop)]);
        common::until("phone authenticated the desktop greeting", || {
            self.net.authenticated_advert(desktop.id()).is_some()
        })
        .await;
    }
}

fn addr(node: &Node) -> SocketAddr {
    ([127, 0, 0, 1], node.network_info().unwrap().port).into()
}

/// A phone seed whose device id orders on the requested side of `peer`; the
/// smaller id of a pair coordinates the merge permit.
fn seed_ordered(peer: DeviceId, phone_smaller: bool) -> [u8; 32] {
    (1u8..=255)
        .map(|n| [n; 32])
        .find(|seed| (Identity::from_seed(*seed).id() < peer) == phone_smaller)
        .unwrap()
}

fn online(desktop: &Node, device: DeviceId) -> bool {
    desktop.device_cards().is_ok_and(|cards| {
        cards
            .cards
            .iter()
            .any(|card| card.device.0 == device && matches!(card.connection, CardConnection::Online { .. }))
    })
}

/// The phone starts the pairing and joins the desktop's group (initiator joins).
async fn phone_pairs_desktop(phone: &Phone, desktop: &Node) -> String {
    phone.connect(desktop).await;
    let pair = phone.net.start_pairing(desktop.id()).unwrap();
    common::converge(&[desktop], "desktop receives the phone's request", || {
        desktop.pairings().unwrap().requests.iter().any(|r| r.id == pair)
    })
    .await;
    let request = desktop
        .pairings()
        .unwrap()
        .requests
        .into_iter()
        .find(|r| r.id == pair)
        .unwrap();
    let client = desktop.clone();
    let accepted = tokio::task::spawn_blocking(move || client.accept_pairing(request.id, request.revision))
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(accepted, pairing::PairReply::Value(_)), "{accepted:?}");
    common::converge(&[desktop], "desktop ready for the phone's code", || {
        desktop
            .pairings()
            .unwrap()
            .sessions
            .iter()
            .any(|s| s.id == pair && s.phase == 2)
    })
    .await;
    let code = phone.net.pairing_code(&pair).unwrap();
    let submitted = common::submit(desktop, pair.clone(), 1, code).await;
    assert!(matches!(submitted, pairing::PairReply::Value(_)), "{submitted:?}");
    completed(phone, desktop, &pair).await;
    pair
}

/// The desktop starts the pairing with a phone on its pairing page; the phone
/// enters the code without a separate accept (user rule 2026-11-09).
async fn desktop_requests_phone(desktop: &Node, phone: &Phone) -> String {
    let found = common::query_at(desktop, phone.id(), phone.addr()).await;
    let client = desktop.clone();
    let started = tokio::task::spawn_blocking(move || {
        client.start_pairing(
            format!("00000000-0000-4000-8000-{:012x}", rand_operation()),
            pairing::PairTarget::PreparedQuery {
                query_id: found.query_id,
                generation: found.generation,
            },
        )
    })
    .await
    .unwrap()
    .unwrap();
    let pairing::PairReply::Value(pairing::Started::Started { pair_id }) = started else {
        panic!("pairing start refused: {started:?}")
    };
    pair_id
}

async fn desktop_pairs_phone(desktop: &Node, phone: &Phone) -> String {
    let pair = desktop_requests_phone(desktop, phone).await;
    phone.accept(&pair).await;
    let code = desktop.pairing_code(pair.clone()).unwrap().six_digits;
    let view = phone.session(&pair).unwrap();
    phone.net.submit_pairing_code(&pair, view.revision, 1, &code).unwrap();
    completed(phone, desktop, &pair).await;
    pair
}

fn rand_operation() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0x7000);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

async fn completed(phone: &Phone, desktop: &Node, pair: &str) {
    common::until("phone and desktop complete the pairing", || {
        let view = phone.session(pair);
        assert!(
            !view.as_ref().is_some_and(|v| v.phase == core::Phase::Ended),
            "phone pairing ended: {:?}; desktop: {:?}; phone sessions: {:?}",
            view.as_ref().map(|v| (v.direction, v.phase, &v.terminal)),
            desktop.pairings().unwrap().sessions.iter().find(|s| s.id == pair),
            phone
                .net
                .pairing_snapshot()
                .unwrap()
                .sessions
                .iter()
                .map(|v| (v.direction, v.phase, v.terminal.as_ref().and_then(|t| t.code().ok())))
                .collect::<Vec<_>>()
        );
        view.is_some_and(|v| v.phase == core::Phase::Completed)
            && desktop
                .pairings()
                .unwrap()
                .sessions
                .iter()
                .any(|s| s.id == pair && s.phase == 7)
            && online(desktop, phone.id())
    })
    .await;
}

fn persisted(dir: &Path) -> State {
    let bytes = std::fs::read(dir.join("group-state.v2")).unwrap();
    State::from_wire(dto::GroupState::from_raw(v2::cbor::decode_map(&bytes, limits::CONFIG_STATE).unwrap()).unwrap())
        .unwrap()
}

/// S1: a desktop that already shares its group with a phone pairs with another
/// desktop (initiator joins). The online phone adopts the signed transition.
#[test]
fn online_phone_follows_its_group_into_the_merged_group() {
    common::watchdog::run(module_path!(), "online_phone_follows", Some(4), async {
        let temp = tempfile::tempdir().unwrap();
        let a = Node::start_with_fake_platform(options(temp.path(), "a")).await.unwrap();
        let b = Node::start_with_fake_platform(options(temp.path(), "b")).await.unwrap();
        let phone = Phone::start(temp.path().join("phone"), [81; 32]).await;
        phone_pairs_desktop(&phone, &a).await;
        common::pair(&a, &b).await;
        let target = b.group_id().unwrap();
        common::until("phone adopts the merged group and is re-admitted", || {
            phone.net.group_id() == target
                && phone.members().len() == 3
                && a.group_members().contains(&phone.id())
                && online(&a, phone.id())
        })
        .await;
        assert_eq!(persisted(&phone.dir).roster().anchor().group, target);
        phone.net.stop().await;
        a.shutdown().await;
        b.shutdown().await;
    });
}

/// S1: the same transition while the phone is offline; it looks the proof up
/// when it reconnects to its old member.
#[test]
fn offline_phone_adopts_the_merge_when_it_reconnects() {
    common::watchdog::run(module_path!(), "offline_phone_adopts", Some(4), async {
        let temp = tempfile::tempdir().unwrap();
        let a = Node::start_with_fake_platform(options(temp.path(), "a")).await.unwrap();
        let b = Node::start_with_fake_platform(options(temp.path(), "b")).await.unwrap();
        let phone = Phone::start(temp.path().join("phone"), [82; 32]).await;
        phone_pairs_desktop(&phone, &a).await;
        phone.net.stop().await;
        common::until("desktop sees the phone offline", || !online(&a, phone.id())).await;
        common::pair(&a, &b).await;
        let target = b.group_id().unwrap();
        let phone = phone.restart().await;
        let _ = phone.net.dial(a.id(), vec![addr(&a)]);
        common::until("offline phone adopts the merge on reconnect", || {
            phone.net.group_id() == target && phone.members().len() == 3 && online(&a, phone.id())
        })
        .await;
        phone.net.stop().await;
        a.shutdown().await;
        b.shutdown().await;
    });
}

/// H14 A: removed while offline. The desktop's reply carries the signed
/// removal; the phone keeps it and forms a fresh group, then pairs again.
#[test]
fn offline_removed_phone_forms_a_fresh_group_and_pairs_again() {
    common::watchdog::run(module_path!(), "offline_removed_phone", Some(4), async {
        let temp = tempfile::tempdir().unwrap();
        let a = Node::start_with_fake_platform(options(temp.path(), "a")).await.unwrap();
        let phone = Phone::start(temp.path().join("phone"), [83; 32]).await;
        phone_pairs_desktop(&phone, &a).await;
        let old = phone.net.group_id();
        phone.net.stop().await;
        a.group_remove(phone.id()).await.unwrap();
        let phone = phone.restart().await;
        let _ = phone.net.dial(a.id(), vec![addr(&a)]);
        common::until("removed phone forms a fresh group", || {
            phone.members() == vec![phone.id()] && phone.net.group_id() != old
        })
        .await;
        common::until("old link closed", || phone.net.authenticated_advert(a.id()).is_none()).await;
        phone_pairs_desktop(&phone, &a).await;
        assert!(a.group_members().contains(&phone.id()));
        phone.net.stop().await;
        a.shutdown().await;
    });
}

/// H14 B: the phone removed an offline desktop. On reconnect the phone answers
/// with its signed state, so the desktop learns the removal.
#[test]
fn desktop_removed_by_the_phone_while_offline_learns_it_on_reconnect() {
    common::watchdog::run(module_path!(), "desktop_removed_offline", Some(4), async {
        let temp = tempfile::tempdir().unwrap();
        let a = Node::start_with_fake_platform(options(temp.path(), "a")).await.unwrap();
        let phone = Phone::start(temp.path().join("phone"), [84; 32]).await;
        phone_pairs_desktop(&phone, &a).await;
        let old = a.group_id();
        let desktop = a.id();
        a.shutdown().await;
        drop(a);
        common::until("phone sees the desktop gone", || {
            phone.net.authenticated_advert(desktop).is_none()
        })
        .await;
        phone.net.remove_member(desktop).unwrap();
        // The restarted desktop redials its member from its address book.
        let a = Node::start_with_fake_platform(options(temp.path(), "a")).await.unwrap();
        common::until("desktop leaves the phone's group", || {
            a.group_members() == vec![a.id()] && a.group_id() != old
        })
        .await;
        phone.net.stop().await;
        a.shutdown().await;
    });
}

/// H15: the phone coordinates the permit (smaller id). A completed pairing
/// releases it, so the same two devices can pair again.
#[test]
fn phone_coordinator_pairs_the_same_desktop_a_second_time() {
    common::watchdog::run(module_path!(), "phone_pairs_twice", Some(4), async {
        let temp = tempfile::tempdir().unwrap();
        let a = Node::start_with_fake_platform(options(temp.path(), "a")).await.unwrap();
        let phone = Phone::start(temp.path().join("phone"), seed_ordered(a.id(), true)).await;
        phone_pairs_desktop(&phone, &a).await;
        phone.net.leave_group().unwrap();
        common::until("both alone after leaving", || {
            phone.members().len() == 1 && a.group_members().len() == 1
        })
        .await;
        // A restart keeps the journal: a permit left behind would still block.
        let phone = phone.restart().await;
        phone_pairs_desktop(&phone, &a).await;
        phone.net.stop().await;
        a.shutdown().await;
    });
}

/// M32 C: the phone stopped after its journal committed the merge but before
/// the projection reached group-state.v2. Startup replays the projection.
#[test]
fn phone_restart_replays_a_committed_projection() {
    common::watchdog::run(module_path!(), "phone_replays_projection", Some(4), async {
        let temp = tempfile::tempdir().unwrap();
        let a = Node::start_with_fake_platform(options(temp.path(), "a")).await.unwrap();
        let phone = Phone::start(temp.path().join("phone"), [86; 32]).await;
        let before = std::fs::read(phone.dir.join("group-state.v2")).unwrap();
        phone_pairs_desktop(&phone, &a).await;
        phone.net.stop().await;
        // Rebuild the moment between journal commit and the projection write.
        let journal_path = phone.dir.join("pairing-recovery.cbor");
        let mut raw = v2::cbor::decode_map(&std::fs::read(&journal_path).unwrap(), 64 << 20).unwrap();
        let RawValue::Array(rows) = raw.get(&3).unwrap().clone() else {
            panic!()
        };
        let row = rows
            .iter()
            .map(|row| row.as_map().unwrap())
            .find(|row| row.get(&6) == Some(&RawValue::Uint(1)))
            .unwrap();
        let bundle = dto::MergeTransferBundle::from_raw(row.get(&8).unwrap().as_map().unwrap().clone()).unwrap();
        raw.insert(
            9,
            RawValue::Map(
                [
                    (1, row.get(&1).unwrap().clone()),
                    (2, row.get(&5).unwrap().clone()),
                    (3, RawValue::Map(bundle.target_state().unwrap().into_raw())),
                ]
                .into(),
            ),
        );
        std::fs::write(&journal_path, v2::cbor::encode_map(&raw, 64 << 20).unwrap()).unwrap();
        std::fs::write(phone.dir.join("group-state.v2"), before).unwrap();
        let phone = Phone::boot(phone.dir, phone.seed, None, 9).await;
        assert!(
            phone.members().contains(&a.id()),
            "committed projection replayed at start"
        );
        assert!(persisted(&phone.dir).roster().contains(a.id()));
        phone.net.leave_group().unwrap();
        phone.net.stop().await;
        a.shutdown().await;
    });
}

/// M32 A: the desktop initiated, the phone committed as target, and the
/// desktop lost the Committed reply (its source row is Unconfirmed). On
/// reconnect the desktop's decision query must reach the phone's durable answer.
#[test]
fn phone_target_answers_the_desktop_decision_query() {
    common::watchdog::run(module_path!(), "phone_answers_decision_query", Some(4), async {
        let temp = tempfile::tempdir().unwrap();
        let a = Node::start_with_fake_platform(options(temp.path(), "a")).await.unwrap();
        // The phone coordinates: the desktop source row holds no permit.
        let phone = Phone::start(temp.path().join("phone"), seed_ordered(a.id(), true)).await;
        let dir = a.data_dir().to_path_buf();
        // Snapshot a stopped desktop: a running one holds files open.
        a.shutdown().await;
        drop(a);
        let snapshot = temp.path().join("a-before");
        copy_tree(&dir, &snapshot);
        let a = Node::start_with_fake_platform(options(temp.path(), "a")).await.unwrap();
        desktop_pairs_phone(&a, &phone).await;
        let target = phone.net.group_id();
        a.shutdown().await;
        drop(a);
        let journal = std::fs::read(dir.join(oink_network::pair_journal::FILE)).unwrap();
        let mut raw = v2::cbor::decode_map(&journal, 64 << 20).unwrap();
        let Some(RawValue::Array(rows)) = raw.get_mut(&3) else {
            panic!()
        };
        let row = rows
            .iter_mut()
            .filter_map(|row| match row {
                RawValue::Map(row) => Some(row),
                _ => None,
            })
            .find(|row| row.get(&7) == Some(&RawValue::Uint(0)) && row.get(&6) == Some(&RawValue::Uint(1)))
            .expect("desktop source row");
        // Source sent its Commit (key 13) but never stored the target's bundle.
        row.insert(6, RawValue::Uint(3));
        row.insert(9, RawValue::Bool(false));
        row.remove(&8);
        row.remove(&11);
        let journal = v2::cbor::encode_map(&raw, 64 << 20).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        copy_tree(&snapshot, &dir);
        std::fs::write(dir.join(oink_network::pair_journal::FILE), journal).unwrap();
        let a = Node::start_with_fake_platform(options(temp.path(), "a")).await.unwrap();
        assert_ne!(a.group_id(), Some(target));
        let _ = phone.net.dial(a.id(), vec![addr(&a)]);
        common::until("desktop recovers the committed decision from the phone", || {
            a.group_id() == Some(target) && a.group_members().contains(&phone.id()) && online(&a, phone.id())
        })
        .await;
        phone.net.stop().await;
        a.shutdown().await;
    });
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

/// M34: the phone shows code entry for desktop Y's request, then joins desktop
/// X itself. Y's request targets a group the phone has left: Y must end with
/// GroupChanged instead of a later link loss.
#[test]
fn phone_completion_supersedes_a_request_for_its_old_group() {
    common::watchdog::run(module_path!(), "phone_supersedes", Some(4), async {
        let temp = tempfile::tempdir().unwrap();
        let x = Node::start_with_fake_platform(options(temp.path(), "x")).await.unwrap();
        let y = Node::start_with_fake_platform(options(temp.path(), "y")).await.unwrap();
        let phone = Phone::start(temp.path().join("phone"), [87; 32]).await;
        let from_y = desktop_requests_phone(&y, &phone).await;
        phone.accept(&from_y).await;
        phone_pairs_desktop(&phone, &x).await;
        common::until("Y's request ends with GroupChanged", || {
            y.pairings()
                .unwrap()
                .sessions
                .iter()
                .find(|s| s.id == from_y)
                .is_some_and(|s| {
                    s.phase == 8
                        && s.terminal
                            .as_ref()
                            .is_some_and(|p| p.code == dto::PairCode::GroupChanged.number())
                })
        })
        .await;
        assert!(phone.session(&from_y).is_some_and(|v| v.phase == core::Phase::Ended));
        phone.net.stop().await;
        x.shutdown().await;
        y.shutdown().await;
    });
}

/// L19: an incoming request waiting for its code (no deadline) does not block
/// a membership edit.
#[test]
fn waiting_incoming_request_does_not_block_leaving_the_group() {
    common::watchdog::run(module_path!(), "request_does_not_block_leave", Some(4), async {
        let temp = tempfile::tempdir().unwrap();
        let x = Node::start_with_fake_platform(options(temp.path(), "x")).await.unwrap();
        let y = Node::start_with_fake_platform(options(temp.path(), "y")).await.unwrap();
        let phone = Phone::start(temp.path().join("phone"), [88; 32]).await;
        phone_pairs_desktop(&phone, &x).await;
        let from_y = desktop_requests_phone(&y, &phone).await;
        phone.accept(&from_y).await;
        phone.net.leave_group().unwrap();
        common::until("phone alone", || phone.members() == vec![phone.id()]).await;
        phone.net.stop().await;
        x.shutdown().await;
        y.shutdown().await;
    });
}

/// User rules 2026-11-11: a phone takes requests whenever Oink runs and keeps
/// one waiting in its request list until the user takes or declines it; a
/// second request meanwhile is refused at once with TooManyPendingRequests.
#[test]
fn phone_keeps_one_request_until_the_user_takes_or_declines_it() {
    common::watchdog::run(module_path!(), "phone_keeps_one_request", Some(4), async {
        let temp = tempfile::tempdir().unwrap();
        let x = Node::start_with_fake_platform(options(temp.path(), "x")).await.unwrap();
        let y = Node::start_with_fake_platform(options(temp.path(), "y")).await.unwrap();
        let phone = Phone::start(temp.path().join("phone"), [89; 32]).await;
        let ended_with =
            |node: &Node, pair: &str, code: dto::PairCode| {
                node.pairings().unwrap().sessions.iter().any(|s| {
                    s.id == pair && s.phase == 8 && s.terminal.as_ref().is_some_and(|p| p.code == code.number())
                })
            };

        let from_x = desktop_requests_phone(&x, &phone).await;
        common::until("X's request is listed", || phone.request(&from_x).is_some()).await;
        let refused = desktop_requests_phone(&y, &phone).await;
        common::until("a second request is refused as TooManyPendingRequests", || {
            ended_with(&y, &refused, dto::PairCode::TooManyPendingRequests)
        })
        .await;
        assert!(phone.request(&refused).is_none() && phone.session(&refused).is_none());
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        assert!(phone.request(&from_x).is_some());
        assert!(phone.session(&from_x).is_none(), "nothing is accepted on its own");

        let request = phone.request(&from_x).unwrap();
        phone.net.decline_pairing(&from_x, request.revision).unwrap();
        common::until("declining ends X's request as Declined", || {
            ended_with(&x, &from_x, dto::PairCode::Declined)
        })
        .await;
        assert!(phone.request(&from_x).is_none());

        let from_y = desktop_requests_phone(&y, &phone).await;
        phone.accept(&from_y).await;
        let code = y.pairing_code(from_y.clone()).unwrap().six_digits;
        let view = phone.session(&from_y).unwrap();
        phone.net.submit_pairing_code(&from_y, view.revision, 1, &code).unwrap();
        completed(&phone, &y, &from_y).await;
        phone.net.stop().await;
        x.shutdown().await;
        y.shutdown().await;
    });
}

/// H8: the desktop initiates and joins a fresh phone's group. Nobody in that
/// group speaks oink.layout/2; the desktop's profiles must not wait forever.
#[test]
fn desktop_joining_a_phone_group_keeps_its_layout_profiles_usable() {
    common::watchdog::run(module_path!(), "desktop_joins_phone_profiles", Some(4), async {
        let temp = tempfile::tempdir().unwrap();
        let a = Node::start_with_fake_platform(options(temp.path(), "a")).await.unwrap();
        common::until("desktop profiles ready", || common::profile_layout_ready(&a)).await;
        let phone = Phone::start(temp.path().join("phone"), [90; 32]).await;
        desktop_pairs_phone(&a, &phone).await;
        assert_eq!(a.group_id(), Some(phone.net.group_id()));
        let query = || {
            AgentApi::layout_profile_command(&a, oink_agent::layout_profiles::types::Command::Query)
                .map_err(|error| format!("{error:?}"))
        };
        let mut last = query();
        for _ in 0..300 {
            if common::profile_layout_ready(&a) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            last = query();
        }
        assert!(
            common::profile_layout_ready(&a),
            "profiles in the phone's group: {last:?}"
        );
        phone.net.stop().await;
        a.shutdown().await;
    });
}
