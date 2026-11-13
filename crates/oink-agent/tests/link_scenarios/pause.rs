//! Clipboard restoration has its own completion boundary, independent of input.
use super::*;
use oink_clipboard::{CapturedClipboard, ClipSnapshot, ClipboardBackend, ClipboardError, NativeSpec, content};
use tokio::sync::oneshot;

fn restored(a: &Dev, b: &Dev, round: usize) -> bool {
    let items = a.clip.items();
    (items == vec![ClipData::Text(format!("paused copy {round}"))]
        || items == vec![ClipData::Text(format!("b keeps {round}"))])
        && b.clip.items() == items
}

pub(super) async fn clipboard_restored(a: &Dev, b: &Dev, round: usize) {
    // Resume recaptures the pause owner's clipboard asynchronously. Until that
    // copy and the other peer's paused-period offer converge, another copy is
    // concurrent with restoration and can legitimately lose Lamport ordering.
    // Input readiness and wallpaper delivery do not acknowledge this work.
    wait_for("clipboard state republished on resume", 10.0, || restored(a, b, round)).await;
}

type Gate = (oneshot::Sender<()>, std::sync::mpsc::Receiver<()>);

/// Holds the clipboard actor inside `set_watch_paused(paused)` once armed,
/// and records every `set_watch_paused` call.
#[derive(Default)]
struct ResumeGate {
    armed: Mutex<Option<(bool, Gate)>>,
    calls: Mutex<Vec<bool>>,
}
impl ResumeGate {
    /// Holds the actor when it resumes watching.
    fn arm(&self) -> (oneshot::Receiver<()>, std::sync::mpsc::Sender<()>) {
        self.arm_on(false)
    }
    fn arm_on(&self, paused: bool) -> (oneshot::Receiver<()>, std::sync::mpsc::Sender<()>) {
        let (entered, waiting) = oneshot::channel();
        let (release, gate) = std::sync::mpsc::channel();
        assert!(self.armed.lock().unwrap().replace((paused, (entered, gate))).is_none());
        (waiting, release)
    }
}

struct Clipboard {
    clipboard: Arc<FakeClipboard>,
    gate: Arc<ResumeGate>,
}
impl ClipboardBackend for Clipboard {
    fn change_count(&self) -> u64 {
        self.clipboard.change_count()
    }
    fn read(&self) -> Result<ClipSnapshot, ClipboardError> {
        self.clipboard.read()
    }
    fn capture(&self, data: &Path) -> Result<CapturedClipboard, ClipboardError> {
        self.clipboard.capture(data)
    }
    fn write(&self, items: &[ClipData], sensitivity: Sensitivity) -> Result<u64, ClipboardError> {
        self.clipboard.write(items, sensitivity)
    }
    fn write_native_guarded(&self, spec: NativeSpec, guard: content::ReaderGuard) -> Result<u64, ClipboardError> {
        self.clipboard.write_native_guarded(spec, guard)
    }
    fn watch(&self, handler: oink_clipboard::ChangeHandler) -> Result<(), ClipboardError> {
        self.clipboard.watch(handler)
    }
    fn set_watch_paused(&self, paused: bool) -> Result<(), ClipboardError> {
        self.gate.calls.lock().unwrap().push(paused);
        let armed = {
            let mut armed = self.gate.armed.lock().unwrap();
            if armed.as_ref().is_some_and(|(on, _)| *on == paused) {
                armed.take().map(|(_, gate)| gate)
            } else {
                None
            }
        };
        if let Some((entered, release)) = armed {
            let _ = entered.send(());
            // Dropping the controller also releases the worker during unwinding.
            let _ = release.recv();
        }
        Ok(())
    }
}

#[test]
fn clipboard_resume_waits_for_publication_even_after_input_is_active() {
    common::watchdog::run(
        module_path!(),
        "clipboard_resume_waits_for_publication_even_after_input_is_active",
        Some(4),
        async {
            let root = tempfile::tempdir().unwrap();
            let a = start(root.path(), "alpha").await;
            let gate = Arc::new(ResumeGate::default());
            let held = gate.clone();
            let b = start_config_with_clipboard(
                root.path(),
                "beta",
                oink_test_support::network_config(root.path().join("beta"), "beta", 0),
                move |clipboard| Arc::new(Clipboard { clipboard, gate: held }),
            )
            .await;
            common::pair(&a.node, &b.node).await;
            row(&[&a, &b]).await;
            let link = a.node.links().unwrap().links().by_peer(b.node.id()).unwrap().link;
            api(&b.node, |node| {
                node.set_local_paused("00000000-0000-4000-8000-000000000000".into(), true)
            })
            .await
            .unwrap();
            wait_for("pause settled", 10.0, || {
                b.node.pause_status().paused && !b.node.pause_status().settling
            })
            .await;
            b.clip
                .copy(vec![ClipData::Text("b keeps 0".into())], Sensitivity::Normal);
            a.clip
                .copy(vec![ClipData::Text("paused copy 0".into())], Sensitivity::Normal);
            let (entered, release) = gate.arm();
            api(&b.node, |node| {
                node.set_local_paused("00000000-0000-4000-9000-000000000000".into(), false)
            })
            .await
            .unwrap();
            entered.await.unwrap();
            common::active(&[&a.node, &b.node]).await;
            assert!(!restored(&a, &b, 0), "input can resume before clipboard publication");
            {
                let restored = clipboard_restored(&a, &b, 0);
                tokio::pin!(restored);
                // Explicitly poll the observation before releasing the actor. No
                // sleep or CPU-speed assumption can satisfy the barrier prematurely.
                tokio::select! {
                    biased;
                    _ = &mut restored => panic!("clipboard barrier passed a suspended actor"),
                    _ = std::future::ready(()) => {},
                }
                release.send(()).unwrap();
                restored.await;
            }
            clipboard_reaches(&a, &b, "a to b after restored clipboard").await;
            clipboard_reaches(&b, &a, "b to a after restored clipboard").await;
            assert_eq!(a.node.links().unwrap().links().by_peer(b.node.id()).unwrap().link, link);
            shutdown(vec![a, b]).await;
        },
    );
}

/// Sharing changes (pause, resume, the send switch) made faster than the
/// clipboard actor runs: it applies only the latest state, once. Each older
/// reconfiguration it applied with the newer state cancelled what the
/// previous one started (an accepted offer's write), and the clipboards never
/// converged. Driven with the send switch: the clipboard's pause follows the
/// node's pause and is set again from every config change, so pausing the
/// clipboard behind the node's back is undone mid-test.
#[test]
fn sharing_changes_faster_than_the_clipboard_actor_apply_only_the_latest() {
    common::watchdog::run(
        module_path!(),
        "sharing_changes_faster_than_the_clipboard_actor_apply_only_the_latest",
        Some(4),
        async {
            let root = tempfile::tempdir().unwrap();
            let a = start(root.path(), "alpha").await;
            let gate = Arc::new(ResumeGate::default());
            let held = gate.clone();
            let b = start_config_with_clipboard(
                root.path(),
                "beta",
                oink_test_support::network_config(root.path().join("beta"), "beta", 0),
                move |clipboard| Arc::new(Clipboard { clipboard, gate: held }),
            )
            .await;
            common::pair(&a.node, &b.node).await;
            row(&[&a, &b]).await;
            // Every reconfiguration sets the watch (to unpaused while active).
            let (entered, release) = gate.arm_on(false);
            gate.calls.lock().unwrap().clear();
            let send = |on: bool| {
                api(&b.node, move |node| {
                    let mut settings = node.clipboard_settings().unwrap();
                    settings.send = on;
                    node.set_clipboard_settings(settings.into())
                })
            };
            send(false).await.unwrap();
            // The actor is held applying it; the other changes queue behind it.
            common::phase("clipboard actor holds the first change");
            entered.await.unwrap();
            send(true).await.unwrap();
            send(false).await.unwrap();
            send(true).await.unwrap();
            release.send(()).unwrap();
            common::phase("clipboard actor applies the queued changes");
            b.node.clipboard().unwrap().settle().await;
            let calls = gate.calls.lock().unwrap().clone();
            assert_eq!(calls, vec![false, false], "the first change, then only the latest");
            clipboard_reaches(&a, &b, "a to b after quick changes").await;
            clipboard_reaches(&b, &a, "b to a after quick changes").await;
            shutdown(vec![a, b]).await;
        },
    );
}
