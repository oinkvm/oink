//! Real Node/QUIC/Low tasks; fake OS devices, no desktop validation claim.
use super::*;
use crate::snapshot_tests::{Displays, pair, start_with_clip, until};
use oink_network::link::DialMode;
use oink_protocol::v1::dto::FactPath;
use std::io::Read;

async fn inspect_link(node: &Node, peer: &Node, relay: Option<&Node>) {
    let expected_path = if relay.is_some() {
        FactPath::Relayed
    } else {
        FactPath::Direct
    };
    let hop = relay.unwrap_or(peer).id();
    until("live link facts and remote OS version", || {
        node.link_config()
            .unwrap()
            .local_facts()
            .links()
            .unwrap()
            .0
            .iter()
            .any(|f| {
                f.peer().unwrap() == oink_network::wire_identity::device(peer.id())
                    && f.path().unwrap() == expected_path
                    && f.next_hop().unwrap() == relay.map(|_| oink_network::wire_identity::device(hop))
                    && f.rtt_us().unwrap().is_some()
                    && f.low_available().unwrap()
            })
            && node
                .device_facts(peer.id())
                .is_some_and(|f| !f.os_version().unwrap().0.is_empty())
    })
    .await;
    // No Settings subscription has existed: the source clock must already have
    // populated history, including the actual end-to-end route.
    until("background connection quality", || {
        AgentApi::connection_quality(node)
            .unwrap()
            .peers
            .iter()
            .any(|p| p.device.0 == peer.id() && !p.samples.is_empty())
    })
    .await;
    let quality = AgentApi::connection_quality(node).unwrap();
    let row = quality.peers.iter().find(|p| p.device.0 == peer.id()).unwrap();
    match (&row.path, relay) {
        (LinkKind::Direct, None) => {}
        (LinkKind::Relay { via, via_name }, Some(r)) => {
            assert_eq!(via.0, r.id());
            assert_eq!(via_name, &node.device_name(r.id()));
        }
        path => panic!("wrong quality route: {path:?}"),
    }
    let before = row.samples.last().unwrap().at_ms;
    let subscription = AgentApi::subscribe(node, std::sync::Arc::new(|_| {}));
    AgentApi::unsubscribe(node, subscription);
    until("connection history continues after page closes", || {
        AgentApi::connection_quality(node)
            .unwrap()
            .peers
            .iter()
            .any(|p| p.device.0 == peer.id() && p.samples.last().is_some_and(|s| s.at_ms > before))
    })
    .await;
    let view = AgentApi::group(node).unwrap();
    let link = view
        .members
        .iter()
        .find(|m| m.device.id.0 == peer.id())
        .unwrap()
        .link
        .as_ref()
        .unwrap();
    match (&link.kind, relay) {
        (LinkKind::Direct, None) => {}
        (LinkKind::Relay { via, .. }, Some(r)) => assert_eq!(via.0, r.id()),
        _ => panic!("wrong actual path kind: {:?}", link.kind),
    }
    assert_eq!(link.bulk.as_deref(), Some("available"));
    assert!(link.rtt_ms.is_none_or(|rtt| rtt >= 0.0));
    assert_eq!(AgentApi::status(node).unwrap().network, oink_network::udp_send_status());
    let recorder = node.inner.diagnostics.get().unwrap().recorder.clone();
    until("recorder recovery", || !recorder.status().recovering).await;
    recorder.set_enabled(true).unwrap();
    // Require ordinary periodic observations before any export samples on demand.
    let observed_after = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_micros() as u64;
    tokio::time::sleep(std::time::Duration::from_millis(2200)).await;
    let alias = recorder.anonymizer().device(peer.id());
    let self_alias = recorder.anonymizer().device(node.id());
    let observed_before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_micros() as u64;
    let bundle = tokio::task::spawn_blocking(move || recorder.save(crate::diagnostics::Reason::Manual))
        .await
        .unwrap()
        .unwrap();
    let mut zip = zip::ZipArchive::new(std::fs::File::open(bundle.path).unwrap()).unwrap();
    let manifest: serde_json::Value = serde_json::from_reader(zip.by_name("manifest.json").unwrap()).unwrap();
    let export_alias = manifest["devices"][&self_alias].as_str().unwrap();
    let mut text = String::new();
    zip.by_name(&format!("devices/{export_alias}/timeline.ndjson"))
        .unwrap()
        .read_to_string(&mut text)
        .unwrap();
    assert!(
        text.lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .any(|row| {
                let link = &row["event"]["Link"];
                row["original_utc_us"]
                    .as_u64()
                    .is_some_and(|t| t >= observed_after && t < observed_before)
                    && link["peer"] == alias
                    && link["online"] == true
                    && link["rtt_us"].is_u64()
                    && link["lost_packets"].is_u64()
            }),
        "exported diagnostic timeline lacks actual link sample"
    );
    assert!(
        text.lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .any(|row| {
                let log = &row["event"]["Log"];
                row["original_utc_us"]
                    .as_u64()
                    .is_some_and(|t| t >= observed_after && t < observed_before)
                    && log["module"] == "oink_scheduler"
                    && log["labels"] == serde_json::json!([["source", "Network"]])
                    && log["fields"].as_array().is_some_and(|fields| {
                        [
                            "readback_present",
                            "native_priority",
                            "tier",
                            "demotions",
                            "late_rounds",
                            "over_budget",
                        ]
                        .iter()
                        .all(|name| fields.iter().any(|field| field[0] == *name && field[1].is_number()))
                    })
            }),
        "exported diagnostic timeline lacks network scheduling status"
    );
}

#[test]
fn simultaneous_dial_keeps_pairing_metadata_link_view_and_diagnostics() {
    crate::test_watchdog::run(
        module_path!(),
        "simultaneous_dial_keeps_pairing_metadata_link_view_and_diagnostics",
        Some(4),
        simultaneous_dial_keeps_pairing_metadata_link_view_and_diagnostics_scenario(),
    );
}

async fn simultaneous_dial_keeps_pairing_metadata_link_view_and_diagnostics_scenario() {
    let dir = tempfile::tempdir().unwrap();
    let configs = ["a", "b"].map(|name| oink_test_support::network_config(dir.path().join(name), name, 0));
    let a = start_with_clip(&configs[0], &Displays::new("a"), None).await;
    let b = start_with_clip(&configs[1], &Displays::new("b"), None).await;
    a.links()
        .unwrap()
        .dial(b.id(), vec![b.links().unwrap().local_addr()], DialMode::DirectOnly)
        .unwrap();
    b.links()
        .unwrap()
        .dial(a.id(), vec![a.links().unwrap().local_addr()], DialMode::DirectOnly)
        .unwrap();
    pair(&a, &b).await;
    a.device_rename("a-after-selection".into()).await.unwrap();
    b.device_rename("b-after-selection".into()).await.unwrap();
    until(
        "bilateral metadata after selected connection and losing dial cleanup",
        || a.device_name(b.id()) == "b-after-selection" && b.device_name(a.id()) == "a-after-selection",
    )
    .await;
    inspect_link(&a, &b, None).await;
    inspect_link(&b, &a, None).await;
    a.shutdown().await;
    b.shutdown().await;
}

#[test]
fn relayed_node_publishes_link_view_and_diagnostic_samples() {
    crate::test_watchdog::run(
        module_path!(),
        "relayed_node_publishes_link_view_and_diagnostic_samples",
        Some(4),
        relayed_node_publishes_link_view_and_diagnostic_samples_scenario(),
    );
}

async fn relayed_node_publishes_link_view_and_diagnostic_samples_scenario() {
    let dir = tempfile::tempdir().unwrap();
    let configs = ["a", "b", "relay"].map(|name| oink_test_support::network_config(dir.path().join(name), name, 0));
    let a = start_with_clip(&configs[0], &Displays::new("a"), None).await;
    let b = start_with_clip(&configs[1], &Displays::new("b"), None).await;
    let r = start_with_clip(&configs[2], &Displays::new("relay"), None).await;
    oink_network::link::testing::set_direct_blocked(&a.links().unwrap(), b.id(), true);
    oink_network::link::testing::set_direct_blocked(&b.links().unwrap(), a.id(), true);
    pair(&r, &a).await;
    pair(&r, &b).await;
    crate::snapshot_tests::active(&a, &b).await;
    inspect_link(&a, &b, Some(&r)).await;
    inspect_link(&b, &a, Some(&r)).await;
    a.shutdown().await;
    b.shutdown().await;
    r.shutdown().await;
}
