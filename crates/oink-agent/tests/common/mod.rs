//! Shared real-node pairing fixture. Callers provide fake OS backends in NodeOptions.
use super::test_agent::api::{AgentApi, ApiEvent, SharingState, pairing};
use super::test_agent::device_cards::DecimalU64;
use super::test_agent::node::{Node, NodeOptions};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};

/// Poll conditions that have no published event (for example fixture counters).
/// The enclosing process watchdog bounds missing progress and synchronous calls.
#[allow(dead_code, reason = "shared fixture also compiled into event-only tests")]
pub async fn until(what: &str, ready: impl FnMut() -> bool) {
    let _watchdog = watchdog::ensure(what);
    watchdog::until(what, ready).await;
}

type EventDiagnostics = Arc<Mutex<std::collections::BTreeMap<&'static str, String>>>;

/// Subscribe before observing so publication cannot race the first query.
/// Notify keeps one permit when an event arrives during an observation; Drop
/// removes subscriptions even when a fixture cancels its pending wait.
pub struct StateChanges<'a> {
    subscriptions: Vec<(&'a Node, super::test_agent::api::SubscriptionId)>,
    changed: Arc<tokio::sync::Notify>,
    _diagnostics: Vec<EventDiagnostics>,
    _watchdog: Option<watchdog::Scenario>,
}
impl<'a> StateChanges<'a> {
    pub fn new(nodes: &[&'a Node], what: &str) -> Self {
        let watchdog = watchdog::ensure(what);
        phase(what);
        let mut changes = Self {
            subscriptions: Vec::new(),
            changed: Arc::new(tokio::sync::Notify::new()),
            _diagnostics: Vec::new(),
            _watchdog: watchdog,
        };
        for node in nodes {
            let diagnostic = Arc::new(Mutex::new(std::collections::BTreeMap::new()));
            let weak = Arc::downgrade(&diagnostic);
            watchdog::on_timeout(format!("{what}: {:?}", node.id()), move || {
                weak.upgrade().map_or_else(
                    || "wait finished".into(),
                    |state| match state.try_lock() {
                        Ok(state) => format!("last published state: {state:#?}"),
                        Err(error) => format!("event diagnostic unavailable: {error}"),
                    },
                )
            });
            let changed = changes.changed.clone();
            let cache = diagnostic.clone();
            let subscription = node.subscribe(Arc::new(move |event| {
                let (kind, state) = match event {
                    ApiEvent::Layout(layout) => ("layout", format!("{layout:?}")),
                    ApiEvent::LayoutProfiles(state) => ("profiles", format!("{state:?}")),
                    ApiEvent::LayoutProfileOperation(state) => ("operation", format!("{state:?}")),
                    ApiEvent::Group(group) => ("group", format!("{group:?}")),
                    ApiEvent::PairingChanged { view, .. } => ("pairing", format!("{view:?}")),
                    ApiEvent::PairingRequestsChanged { requests, .. } => ("requests", format!("{requests:?}")),
                    _ => return,
                };
                cache.lock().unwrap().insert(kind, state);
                changed.notify_one();
            }));
            changes.subscriptions.push((node, subscription));
            changes._diagnostics.push(diagnostic);
        }
        changes
    }

    pub async fn notified(&self) {
        self.changed.notified().await;
    }
}
impl Drop for StateChanges<'_> {
    fn drop(&mut self) {
        for (node, subscription) in self.subscriptions.drain(..) {
            node.unsubscribe(subscription);
        }
    }
}

/// Await the actual multi-node predicate after each relevant state publication.
/// There is no per-phase scheduling budget; the scenario watchdog remains armed.
#[allow(dead_code)]
pub async fn converge(nodes: &[&Node], what: &str, mut ready: impl FnMut() -> bool) {
    let changes = StateChanges::new(nodes, what);
    while !ready() {
        changes.notified().await;
    }
}

/// A manual layout fixture needs a writable target catalog as well as owner facts.
#[allow(dead_code, reason = "shared fixture also compiled into tests without layout edits")]
pub fn profile_layout_ready(node: &Node) -> bool {
    AgentApi::layout_profile_command(node, super::test_agent::layout_profiles::types::Command::Query).is_ok_and(
        |state| state["readiness"] == "ready" && state.get("snapshot").is_some_and(|snapshot| !snapshot.is_null()),
    )
}

/// Screen facts and input admission can precede catalog handoff. Manual edits
/// also need the target catalog and its owner visibility snapshot to be ready.
#[allow(dead_code, reason = "shared fixture also compiled into tests without layout edits")]
pub async fn editable_layouts(nodes: &[&Node], screen_count: usize) {
    converge(nodes, "editable shared layouts", || {
        nodes
            .iter()
            .all(|node| node.layout().0.screens.len() == screen_count && profile_layout_ready(node))
    })
    .await;
}

/// A fresh-default fixture needs the completed catalog handoff before editing,
/// pairing onward, or removing the acceptor. Matching provisional catalogs can
/// precede completion even when the retained default ID does not change. Check
/// durable completion and every profile's member coverage as well as API state.
#[allow(
    dead_code,
    reason = "shared fixture also compiled into tests without profile handoffs"
)]
pub async fn profile_handoff(nodes: &[&Node], expected_default: &str) {
    converge(nodes, "complete profile catalog converged after pairing", || {
        let states = nodes
            .iter()
            .map(|node| {
                AgentApi::layout_profile_command(*node, super::test_agent::layout_profiles::types::Command::Query)
            })
            .collect::<Result<Vec<_>, _>>();
        states.is_ok_and(|states| {
            states.iter().zip(nodes).all(|(state, node)| {
                state["readiness"] == "ready"
                    && state["active_profile_id"] == expected_default
                    && state["catalog_token"] == states[0]["catalog_token"]
                    && node.group_id().is_some_and(|group| {
                        super::test_agent::layout_profiles::store::load(node.data_dir(), group.into()).is_ok_and(
                            |catalog| {
                                catalog.is_some_and(|c| {
                                    !c.completed_joins.is_empty()
                                        && c.profiles.iter().all(|p| {
                                            nodes
                                                .iter()
                                                .all(|n| p.content.placements.iter().any(|p| p.device.0 == n.id()))
                                        })
                                })
                            },
                        )
                    })
            })
        })
    })
    .await;
}

static OPERATIONS: AtomicU64 = AtomicU64::new(1);

/// Resolve an authenticated peer, including peers that are already members.
pub async fn query(initiator: &Node, responder: &Node) -> pairing::FoundPeer {
    query_at(
        initiator,
        responder.id(),
        ([127, 0, 0, 1], responder.network_info().unwrap().port).into(),
    )
    .await
}

pub async fn query_at(
    initiator: &Node,
    expected_device: oink_foundation::DeviceId,
    address: std::net::SocketAddr,
) -> pairing::FoundPeer {
    let _watchdog = watchdog::ensure("pairing address query");
    let generation = DecimalU64(OPERATIONS.fetch_add(1, Ordering::Relaxed));
    let operation = format!("00000000-0000-4000-8000-{:012x}", generation.0);
    phase("pair: prepared address query");
    let (events, mut received) = tokio::sync::mpsc::unbounded_channel();
    let expected = operation.clone();
    let subscription = initiator.subscribe(Arc::new(move |event| {
        if let ApiEvent::AddressQueryFinished {
            operation_id, result, ..
        } = event
            && operation_id == &expected
        {
            let _ = events.send(result.clone());
        }
    }));
    let client = initiator.clone();
    let query = tokio::task::spawn_blocking(move || {
        client.query_pairing_address(operation, generation, address.ip().to_string(), Some(address.port()))
    })
    .await
    .unwrap()
    .unwrap();
    assert!(
        matches!(query, pairing::PairReply::Value(_)),
        "query refused: {query:?}"
    );
    // The owner produces a typed completion at its five-second total deadline.
    // The scenario watchdog also bounds synchronous calls and missing events.
    let found = received.recv().await.expect("query event channel");
    initiator.unsubscribe(subscription);
    let pairing::AddressQueryResult::Found { peer } = found else {
        panic!("address query failed: {found:?}")
    };
    assert_eq!(peer.peer.device.0, expected_device);
    peer
}

/// Start and explicitly accept a request, stopping before code submission.
pub async fn request(initiator: &Node, responder: &Node) -> (pairing::PairId, String) {
    let _watchdog = watchdog::ensure("pairing request");
    let peer = query(initiator, responder).await;
    assert!(peer.prepared, "a new pairing request needs a prepared peer");
    let client = initiator.clone();
    let operation = format!(
        "00000000-0000-4000-8000-{:012x}",
        OPERATIONS.fetch_add(1, Ordering::Relaxed)
    );
    let started = tokio::task::spawn_blocking(move || {
        client.start_pairing(
            operation,
            pairing::PairTarget::PreparedQuery {
                query_id: peer.query_id,
                generation: peer.generation,
            },
        )
    })
    .await
    .unwrap()
    .unwrap();
    let pairing::PairReply::Value(pairing::Started::Started { pair_id }) = started else {
        panic!("pairing start refused: {started:?}")
    };
    phase("explicit incoming pairing request");
    converge(&[initiator, responder], "explicit incoming pairing request", || {
        let outgoing = initiator.pairings().unwrap();
        let incoming = responder.pairings().unwrap();
        assert!(
            !outgoing.sessions.iter().any(|s| s.id == pair_id && s.phase == 8),
            "pairing ended before incoming request: pair={pair_id}; outgoing={outgoing:?}; incoming={incoming:?}; initiator_group={:?}; responder_group={:?}",
            initiator.group(), responder.group(),
        );
        incoming.requests.iter().any(|r| r.id == pair_id)
    })
    .await;
    let request = responder
        .pairings()
        .unwrap()
        .requests
        .into_iter()
        .find(|r| r.id == pair_id)
        .unwrap();
    let before_accept = initiator
        .pairings()
        .unwrap()
        .sessions
        .into_iter()
        .find(|session| session.id == pair_id)
        .unwrap()
        .revision;
    let client = responder.clone();
    let accepted = tokio::task::spawn_blocking(move || client.accept_pairing(request.id, request.revision))
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(accepted, pairing::PairReply::Value(_)),
        "accept refused: {accepted:?}"
    );
    // The initiator already shows its code before acceptance. The accept
    // acknowledgement advances its revision without changing that UI phase;
    // returning earlier races the next command's expected revision.
    converge(&[initiator], "initiator received pairing acceptance", || {
        initiator
            .pairings()
            .unwrap()
            .sessions
            .iter()
            .any(|s| s.id == pair_id && s.phase == 1 && s.revision > before_accept)
    })
    .await;
    let code = initiator.pairing_code(pair_id.clone()).unwrap().six_digits;
    converge(&[responder], "responder ready for code", || {
        responder
            .pairings()
            .unwrap()
            .sessions
            .iter()
            .any(|s| s.id == pair_id && s.phase == 2)
    })
    .await;
    (pair_id, code)
}

/// Submit against the responder's current revision; expose refusals to tests.
pub async fn submit(
    responder: &Node,
    pair_id: pairing::PairId,
    attempt: u8,
    code: String,
) -> pairing::PairReply<pairing::Accepted> {
    let session = responder
        .pairings()
        .unwrap()
        .sessions
        .into_iter()
        .find(|s| s.id == pair_id)
        .unwrap();
    let client = responder.clone();
    tokio::task::spawn_blocking(move || client.submit_pairing_code(pair_id, session.revision, attempt, code))
        .await
        .unwrap()
        .unwrap()
}

pub async fn pair(initiator: &Node, responder: &Node) {
    let _watchdog = watchdog::ensure("pairing fixture");
    let (pair_id, code) = request(initiator, responder).await;
    let submitted = submit(responder, pair_id.clone(), 1, code).await;
    assert!(
        matches!(submitted, pairing::PairReply::Value(_)),
        "code refused: {submitted:?}"
    );
    phase("durable paired membership");
    // Completion includes the owner's bounded prepare/admission/recovery stages,
    // not just the earlier roster projection. Observe its typed terminal state;
    // the enclosing process watchdog still bounds a missing completion.
    converge(&[initiator, responder], "durable paired membership", || {
        let a = initiator.group().unwrap();
        let b = responder.group().unwrap();
        let pairings = [initiator.pairings().unwrap(), responder.pairings().unwrap()];
        assert!(
            pairings
                .iter()
                .all(|p| !p.sessions.iter().any(|s| s.id == pair_id && s.phase == 8)),
            "pairing terminated before completion: {pairings:?}",
        );
        a.id.is_some()
                && a.id == b.id
                && a.members.iter().any(|m| m.device.id.0 == responder.id())
                && b.members.iter().any(|m| m.device.id.0 == initiator.id())
                // Roster adoption and input admission precede the durable
                // receipt exchange. A caller may immediately restart/reset a
                // node or pair again, so it needs actual transaction completion
                // and released endpoint permits on both owners.
                && pairings.iter().all(|p| {
                    p.sessions.iter().any(|s| s.id == pair_id && s.phase == 7)
                })
    })
    .await;
    active(&[initiator, responder]).await;
}

pub async fn active(nodes: &[&Node]) {
    converge(nodes, "all input sharing active", || {
        nodes.iter().all(|a| {
            let group = a.group().unwrap();
            nodes.iter().all(|b| {
                a.id() == b.id()
                    || group
                        .members
                        .iter()
                        .any(|m| m.device.id.0 == b.id() && m.sharing == SharingState::Active)
            })
        })
    })
    .await;
}

#[allow(dead_code)]
pub async fn start(options: impl IntoIterator<Item = NodeOptions>) -> Vec<Node> {
    let _watchdog = watchdog::ensure("start paired nodes");
    let mut nodes = Vec::new();
    for option in options {
        nodes.push(Node::start_with_fake_platform(option).await.unwrap());
    }
    for peer in nodes.iter().skip(1) {
        pair(&nodes[0], peer).await;
    }
    active(&nodes.iter().collect::<Vec<_>>()).await;
    nodes
}

#[allow(dead_code)]
pub fn input_id(node: &Node, peer: oink_foundation::DeviceId) -> Option<oink_network::link::StreamId> {
    node.input_status()
        .ok()?
        .peers
        .iter()
        .find(|p| p.peer == peer && p.attached)
        .and_then(|p| p.outbound)
}
#[allow(dead_code)]
pub fn linked(node: &Node, peer: oink_foundation::DeviceId) -> bool {
    node.links().unwrap().links().by_peer(peer).is_some()
}

pub(crate) mod watchdog;
pub use watchdog::phase;
