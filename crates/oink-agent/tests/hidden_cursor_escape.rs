#![cfg(debug_assertions)]
//! Real capture-owner/driver plumbing, with an in-memory native desktop.
use oink_agent::{
    api::{AgentApi, Request, ScreenKey, dispatch},
    node::{Node, NodeOptions},
};
use oink_display::DisplayInfo;
use oink_foundation::{Point, Rect};
use oink_input::{CaptureMode, CapturedEvent, Disposition, InputEvent, SourceKind};
use oink_test_support::input::FakeInput;
use std::{sync::Arc, time::Duration};

async fn request(node: &Node, request: Request) {
    let node = node.clone();
    tokio::task::spawn_blocking(move || dispatch(&node, request))
        .await
        .unwrap()
        .unwrap();
}
async fn wait(what: &str, mut ready: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !ready() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timeout waiting for {what}"));
}
fn motion(dx: f64, at: Point) -> CapturedEvent {
    CapturedEvent {
        physical: true,
        event: InputEvent::PointerDelta { dx, dy: 0. },
        source: SourceKind::Mouse,
        time_us: 0,
        cursor: Some(at),
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hidden_native_movement_passes_without_injection_on_windows_and_macos() {
    for os in ["windows", "macos"] {
        let dir = tempfile::tempdir().unwrap();
        let mut net = oink_test_support::network_config(dir.path().into(), "hidden-escape", 0);
        net.os = os.into();
        let input = Arc::new(FakeInput::new());
        input.set_cursor(Point::new(500., 300.));
        let displays = ["visible", "hidden"]
            .into_iter()
            .enumerate()
            .map(|(i, key)| DisplayInfo {
                key: key.into(),
                name: key.into(),
                bounds: Rect::new(i as f64 * 1000., 0., 1000., 600.),
                pixel_width: 1000,
                pixel_height: 600,
                reference_extent: None,
                rotation_deg: 0,
                is_primary: i == 0,
                is_builtin: false,
                is_virtual: false,
            })
            .collect();
        let node = Node::start_with_fake_platform(NodeOptions {
            net,
            input: input.clone(),
            displays,
            display_backend: None,
            clipboard: None,
            mode_switch_in_callback: true,
            capture_reports_key_repeat: false,
            cursor_trail: false,
        })
        .await
        .unwrap();
        wait("editable layout", || {
            node.layout_profile_command(oink_agent::layout_profiles::types::Command::Query)
                .is_ok_and(|s| s["readiness"] == "ready")
        })
        .await;
        request(
            &node,
            Request::SetGameMode {
                mode: oink_routing::GameMode::Off,
            },
        )
        .await;
        request(
            &node,
            Request::SetScreenHidden {
                screen: ScreenKey {
                    device: node.id().into(),
                    display: "hidden".into(),
                },
                hidden: true,
            },
        )
        .await;
        wait("hidden layout", || {
            AgentApi::layout(&node)
                .unwrap()
                .geometry
                .screens
                .iter()
                .any(|s| s.key.display == "hidden" && s.hidden)
        })
        .await;

        wait("local capture", || {
            input.capture_sessions() > 0 && input.mode() == Some(CaptureMode::Local)
        })
        .await;
        input.take_injected();

        // There is no remote destination at this native seam. The OS owns
        // movement into, within and back out of the hidden active desktop.
        // Returning to a visible sibling must not reinstate any confinement.
        for (dx, x) in [(600., 1100.), (100., 1200.), (-300., 900.), (300., 1200.)] {
            let at = Point::new(x, 300.);
            input.set_cursor(at);
            assert_eq!(input.feed(&motion(dx, at)), Disposition::Pass, "{os}: native x={x}");
            assert_eq!(input.mode(), Some(CaptureMode::Local));
            assert!(input.take_injected().is_empty(), "{os}: native movement was reinjected");
        }

        // An external native warp into a hidden screen also needs no Oink
        // correction. The next physical move can return to a visible sibling.
        input.set_cursor(Point::new(1500., 300.));
        for (dx, x) in [(-10., 1490.), (-590., 900.)] {
            let at = Point::new(x, 300.);
            input.set_cursor(at);
            assert_eq!(
                input.feed(&motion(dx, at)),
                Disposition::Pass,
                "{os}: return after native warp"
            );
            assert_eq!(input.mode(), Some(CaptureMode::Local));
            assert!(
                input.take_injected().is_empty(),
                "{os}: native warp/return was corrected"
            );
        }
        node.shutdown().await;
    }
}
