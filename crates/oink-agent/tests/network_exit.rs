#![cfg(debug_assertions)]
//! TERM timing on disposable loopback peers and FakeInput, never the installed service.
#![cfg(unix)]
use oink_agent as test_agent;
mod common;
use oink_agent::{
    host::AgentHost,
    node::{Node, NodeOptions},
};
use oink_clipboard::{ClipData, Sensitivity};
use oink_test_support::clipboard::FakeClipboard;
use oink_test_support::input::FakeInput;
use std::io::{BufRead, Read, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};
fn options(path: std::path::PathBuf, name: &str, clip: Arc<FakeClipboard>) -> NodeOptions {
    NodeOptions {
        net: oink_test_support::network_config(path, name, 0),
        input: Arc::new(FakeInput::new()),
        displays: vec![],
        display_backend: None,
        clipboard: Some(clip),
        mode_switch_in_callback: true,
        capture_reports_key_repeat: true,
        cursor_trail: false,
    }
}
#[test]
fn term_child() {
    let _scenario = common::watchdog::Scenario::new(module_path!(), "term_child");
    let Ok(case) = std::env::var("OINK_NETWORK_EXIT_CASE") else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let source = Arc::new(FakeClipboard::default());
    let target = Arc::new(FakeClipboard::default());
    let host =
        AgentHost::start_with_fake_platform(options(dir.path().join("host"), "host", source.clone()), false).unwrap();
    let mut peer = if case == "idle" {
        None
    } else {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let node = rt
            .block_on(Node::start_with_fake_platform(options(
                dir.path().join("peer"),
                "peer",
                target.clone(),
            )))
            .unwrap();
        host.block_on(common::pair(host.node(), &node));
        Some((rt, node))
    };
    let mut blackhole = None;
    if case == "connecting" {
        let (rt, node) = peer.take().unwrap();
        let address = node.links().unwrap().local_addr();
        let peer_id = node.id();
        rt.block_on(node.shutdown());
        drop(node);
        rt.shutdown_timeout(Duration::from_secs(1));
        blackhole = Some(host.block_on(async {
            let socket = tokio::net::UdpSocket::bind(address).await.unwrap();
            let node = host.node().clone();
            tokio::spawn(async move {
                let _ = node
                    .links()
                    .unwrap()
                    .dial(peer_id, vec![address], oink_network::link::DialMode::PreferDirect);
            });
            tokio::time::timeout(Duration::from_secs(15), async {
                let mut packet = [0; 2048];
                loop {
                    let (len, _) = socket.recv_from(&mut packet).await.unwrap();
                    if len >= 1200 && packet[0] & 0xf0 == 0xc0 && packet[1..5] == [0, 0, 0, 1] {
                        break;
                    }
                }
            })
            .await
            .unwrap();
            socket
        }));
    } else if case == "bulk" {
        host.block_on(async {
            // First establish completed payload delivery with a stable offer.
            // Replacing every offer while waiting for that completion cancels
            // its fetch and measures supersession, not shutdown during traffic.
            source.copy(vec![ClipData::Png(vec![7; 2 * 1024 * 1024])], Sensitivity::Normal);
            common::until("first complete Low clipboard payload", || !target.items().is_empty()).await;
            tokio::spawn(async move {
                for n in 0..1000u32 {
                    let mut image = vec![7; 2 * 1024 * 1024];
                    image[..4].copy_from_slice(&n.to_le_bytes());
                    source.copy(vec![ClipData::Png(image)], Sensitivity::Normal);
                    // Clipboard waits 300 ms for a stable native offer. Keep
                    // each offer long enough to initiate an actual Low fetch.
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
            });
            let receiver = &peer.as_ref().unwrap().1;
            common::until("Low payload is in flight at TERM", || {
                // The clipboard receiver initiates the fetch. Source-side
                // outgoing operations used to be diagnostics replication and
                // were never evidence that this clipboard payload was active.
                receiver
                    .low_status(host.node().id())
                    .is_some_and(|status| status.active_connections > 0)
            })
            .await;
        });
    } else {
        assert!(matches!(case.as_str(), "idle" | "connected"));
    }
    let _blackhole = blackhole;
    println!("network-exit-ready:{case}");
    std::io::stdout().flush().unwrap();
    host.block_on(host.node().wait_for_stop());
    let marker = host.node().data_dir().join("Logs/Diagnostics/running");
    host.shutdown();
    assert!(!marker.exists(), "TERM bypassed recorder clean shutdown");
    // The peer is another application owned by this fixture. Stop its readers
    // before dropping its runtime, just as the signalled host stops its own.
    if let Some((rt, node)) = peer {
        rt.block_on(node.shutdown());
        rt.shutdown_timeout(Duration::from_secs(1));
    }
}

#[test]
fn term_stays_bounded_idle_connecting_connected_and_bulk() {
    let _scenario =
        common::watchdog::Scenario::new(module_path!(), "term_stays_bounded_idle_connecting_connected_and_bulk");
    for case in ["idle", "connecting", "connected", "bulk"] {
        for run in 0..5 {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["term_child", "--exact", "--nocapture"])
                .env("OINK_NETWORK_EXIT_CASE", case)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            let stdout = child.stdout.take().unwrap();
            let mut stderr = child.stderr.take().unwrap();
            let errors = std::thread::spawn(move || {
                let mut text = String::new();
                stderr.read_to_string(&mut text).unwrap();
                text
            });
            let (ready_tx, ready_rx) = std::sync::mpsc::channel();
            let reader = std::thread::spawn(move || {
                for line in std::io::BufReader::new(stdout).lines() {
                    let Ok(line) = line else {
                        break;
                    };
                    if line == format!("network-exit-ready:{case}") {
                        let _ = ready_tx.send(());
                    }
                }
            });
            if ready_rx.recv_timeout(Duration::from_secs(30)).is_err() {
                let _ = child.kill();
                let _ = child.wait();
                reader.join().unwrap();
                let errors = errors.join().unwrap();
                eprintln!("{errors}");
                panic!("{case} child did not reach its network readiness barrier");
            }
            let started = Instant::now();
            // SAFETY: signal only this test's owned live child process.
            assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0);
            let status = loop {
                if let Some(status) = child.try_wait().unwrap() {
                    break status;
                }
                if started.elapsed() >= Duration::from_secs(5) {
                    let _ = child.kill();
                    let _ = child.wait();
                    reader.join().unwrap();
                    let errors = errors.join().unwrap();
                    eprintln!("{errors}");
                    panic!("{case}: TERM exceeded the existing host cleanup + runtime shutdown budget");
                }
                std::thread::sleep(Duration::from_millis(2));
            };
            let elapsed = started.elapsed();
            reader.join().unwrap();
            let errors = errors.join().unwrap();
            assert!(status.success(), "{case} child failed: {status}\n{errors}");
            assert!(
                !errors.contains("panicked"),
                "{case}: shutdown emitted a task panic: {errors}"
            );
            eprintln!("TERM_EXIT case={case} run={run} elapsed_us={}", elapsed.as_micros());
        }
    }
}
