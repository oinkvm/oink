use super::*;
use crate::node::NodeOptions;
use crate::threads::inject::{InjectItem, start_with_backend};
use oink_clipboard::{
    ClipboardError,
    dnd::{DndSink, Injector},
};
use oink_foundation::{
    Point,
    input::{InjectEvent, KeyCode},
};
use std::sync::mpsc;

struct CancelGate {
    phase: mpsc::Sender<&'static str>,
    release: Mutex<mpsc::Receiver<()>>,
    ended: Mutex<mpsc::Receiver<()>>,
}
impl DndBackend for CancelGate {
    fn start(&self, _: DndSink, _: Injector) -> Result<(), ClipboardError> {
        Ok(())
    }
    fn probe(&self, _: Point) {}
    fn unprobe(&self) {}
    fn begin(&self, _: DragSpec) -> Result<(), ClipboardError> {
        Ok(())
    }
    fn release(&self) -> bool {
        true
    }
    fn cancel(&self) -> bool {
        true
    }
    fn cancel_physical(&self, release: &dyn Fn() -> Result<(), String>) -> bool {
        self.phase.send("recognizing").unwrap();
        self.release.lock().unwrap().recv().unwrap();
        let accepted = release().is_ok();
        self.phase.send("up").unwrap();
        // AppKit's ended callback may arrive long after the accepted UP. Keep
        // this real ordinary command outstanding until the test permits it.
        self.ended.lock().unwrap().recv().unwrap();
        accepted
    }
}

#[test]
fn hosted_cancel_releases_input_after_up_before_native_end() {
    crate::test_watchdog::run(
        module_path!(),
        "hosted_cancel_releases_input_after_up_before_native_end",
        Some(2),
        scenario(),
    );
}

async fn scenario() {
    for physical in [true, false] {
        for leave in [false, true] {
            cancellation(physical, leave).await;
        }
    }
}

async fn cancellation(physical: bool, leave: bool) {
    let root = tempfile::tempdir().unwrap();
    let native = Arc::new(oink_test_support::input::FakeInput::new());
    let node = Node::start(NodeOptions {
        net: oink_test_support::network_config(root.path().into(), "cancel-release", 0),
        input: native.clone(),
        displays: vec![],
        display_backend: None,
        clipboard: None,
        mode_switch_in_callback: true,
        capture_reports_key_repeat: false,
        cursor_trail: false,
    })
    .await
    .unwrap();
    let (phase, phases) = mpsc::channel();
    let (release, release_wait) = mpsc::channel();
    let (ended, ended_wait) = mpsc::channel();
    node.start_dnd_backend(Box::new(CancelGate {
        phase,
        release: Mutex::new(release_wait),
        ended: Mutex::new(ended_wait),
    }))
    .unwrap();
    let glue = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(glue) = node.inner.dnd.get().unwrap().glue.lock().unwrap().clone() {
                break glue;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let operation = DragOperation::new();
    let id = ClipId {
        origin: node.id(),
        seq: 1,
    };
    let grant = test_target(id, 1);
    assert!(grant.claim_down());
    grant.down_submitted(true);
    *glue.active_operation.lock().unwrap() = Some((operation, id, 1));
    *glue.active_target.lock().unwrap() = Some((operation, grant));
    let (drained, drain) = mpsc::channel();
    let injector = start_with_backend(
        native.clone(),
        Box::new(move |_| {
            let _ = drained.send(());
        }),
    )
    .unwrap();
    injector.push(InjectItem::Dnd {
        command: if leave {
            DndCmd::LeaveHost { id, epoch: 1, physical }
        } else {
            DndCmd::Cancel { id, epoch: 1, physical }
        },
        main: node.main_sender_for_test(),
    });
    assert_eq!(phases.recv_timeout(Duration::from_secs(2)).unwrap(), "recognizing");
    let motion = InjectEvent::MoveTo {
        pos: Point::new(80., 40.),
    };
    let key = InjectEvent::Key {
        code: KeyCode::keyboard(4),
        pressed: true,
    };
    for event in [&motion, &key] {
        injector.push(InjectItem::Inject {
            event: event.clone(),
            from: None,
        });
    }
    injector.push(InjectItem::ReleaseAll {
        peer: DeviceId([7; 32]),
    });
    let before_up = drain.recv_timeout(Duration::from_millis(50)).is_ok();
    release.send(()).unwrap();
    assert_eq!(phases.recv_timeout(Duration::from_secs(2)).unwrap(), "up");
    let after_up = drain.recv_timeout(Duration::from_millis(200)).is_ok();
    let injected_before_end = native.injected();
    // Always let the ordinary worker finish before asserting or tearing down.
    ended.send(()).unwrap();
    if !after_up {
        drain.recv_timeout(Duration::from_secs(2)).unwrap();
    }
    drop(injector);
    node.shutdown().await;
    assert!(!before_up, "the reject-window/UP section must retain its input fence");
    assert!(
        after_up,
        "accepted cancellation UP must release input before native ended acknowledgement"
    );
    assert!(injected_before_end.contains(&motion));
    assert!(injected_before_end.contains(&key));
    let up = InjectEvent::Button {
        button: oink_foundation::input::MouseButton::Left,
        pressed: false,
    };
    assert!(
        injected_before_end.iter().position(|event| *event == up).unwrap()
            < injected_before_end.iter().position(|event| *event == motion).unwrap(),
        "queued motion cannot overtake the accepted cancellation UP"
    );
}
