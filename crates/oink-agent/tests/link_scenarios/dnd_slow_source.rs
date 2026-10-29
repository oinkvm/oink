//! A blocked one-shot renderer behind the real Low grant service. The gate
//! models FileContents latency; fake native backends never touch the desktop.
use super::*;
use oink_clipboard::content::namespace::{Local, Namespace};
use oink_clipboard::model::*;
use std::io;

struct SlowVirtual {
    files: Arc<Local>,
    gate: Mutex<Option<BoundaryGate>>,
    failure: Mutex<Option<Unavailable>>,
}
impl Namespace for SlowVirtual {
    fn prepare(&self) -> io::Result<()> {
        stop_at_boundary(&self.gate);
        match *self.failure.lock().unwrap() {
            Some(reason) => Err(oink_clipboard::content::unavailable(reason)),
            None => Ok(()),
        }
    }
    fn one_shot_roots(&self) -> Vec<NodeId> {
        self.files.roots().iter().map(|node| node.id).collect()
    }
    fn roots(&self) -> Vec<FileNode> {
        self.files.roots()
    }
    fn source_roots(&self) -> io::Result<Vec<oink_clipboard::content::grants::SourceRoot>> {
        self.files.source_roots()
    }
    fn list(&self, node: NodeId, cursor: Option<DirectoryCursor>) -> io::Result<DirectoryPage> {
        self.files.list(node, cursor)
    }
    fn select(&self, node: NodeId) -> io::Result<FileNode> {
        self.files.select(node)
    }
    fn open(&self, node: NodeId, version: u64, offset: u64) -> io::Result<oink_clipboard::FileReader> {
        self.files.open(node, version, offset)
    }
}

#[test]
fn slow_virtual_grant_does_not_fence_either_input_owner_or_revive_cancelled_begin() {
    common::watchdog::run(module_path!(), "slow virtual source input isolation", Some(12), async {
        for outcome in 0..3 {
            let cancel = outcome == 1;
            let timeout = outcome == 2;
            let root = tempfile::tempdir().unwrap();
            let (a, b, da, db) = drag_pair(root.path()).await;
            let path = a.dir.join("virtual.txt");
            std::fs::write(&path, b"one-shot virtual content").unwrap();
            let files = Arc::new(SlowVirtual {
                files: Local::new(vec![(NodeId(1), path)]).unwrap(),
                gate: Mutex::default(),
                failure: Mutex::default(),
            });
            let (entered, release) = boundary(&files.gate);
            let inventory = Inventory {
                id: OfferId {
                    clip: oink_foundation::content::ClipId {
                        origin: a.node.id(),
                        seq: 0,
                    },
                    generation: 0,
                },
                platform: oink_foundation::clipboard::ClipPlatform::Windows,
                roots: files.roots(),
                items: Vec::new(),
            };
            *da.offer.lock().unwrap() = Some(oink_clipboard::content::items(&inventory));
            *da.export_provider.lock().unwrap() = Some(Arc::new(ExportFiles { inventory, files }));
            button(&a.input, true);
            cross(&a, &b, 1.).await;
            // The native backend does not request content. Preparation must
            // acquire it before Begin, including on Windows where GetData would
            // otherwise block the OLE STA and the receiving app during hover.
            let prepared_first = tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    if entered.try_recv().is_ok() {
                        return true;
                    }
                    if db.began.lock().unwrap().is_some() {
                        return false;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap_or(false);
            if !prepared_first {
                let _ = release.send(());
                a.node.shutdown().await;
                b.node.shutdown().await;
                panic!("native Begin ran before background namespace preparation");
            }
            // Reaching the gate proves the production grant path invoked the
            // virtual source, after metadata was ready. It stays blocked until
            // both the source routing owner and target injector process keys.
            b.input.take_injected();
            key(&a.input, 0x04, true);
            key(&a.input, 0x04, false);
            let input_ready = tokio::time::timeout(Duration::from_secs(2), async {
                while keys(&b.input) != [(0x04, true), (0x04, false)] {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .is_ok();
            let native_unstarted = db.began.lock().unwrap().is_none();
            let began_calls = db.calls.lock().unwrap().contains(&"begin");
            if cancel && input_ready {
                button(&a.input, false);
                common::watchdog::until("Up retires preparation while source remains blocked", || {
                    b.node.input_status().unwrap().dnd_host_pending.is_none()
                })
                .await;
                // Exercise input delivery in the reverse direction too, while
                // the original source renderer is still held at the gate.
                cross(&b, &a, -1.).await;
                type_and_check(&b, &a, 2, "reverse input during cancelled source render").await;
            }
            if timeout {
                let start = Instant::now();
                tokio::time::timeout(Duration::from_secs(4), async {
                    while b.node.input_status().unwrap().dnd_host_pending.is_some() {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("3s preparation timeout must retire the target before source rendering finishes");
                assert!(start.elapsed() < Duration::from_secs(4));
                assert!(db.began.lock().unwrap().is_none());
                button(&a.input, false);
            }
            release.send(()).unwrap();
            assert!(
                input_ready,
                "slow FileContents grant fenced source routing or target injection"
            );
            assert!(
                native_unstarted && !began_calls,
                "native Begin started before the content grant was ready"
            );
            assert!(!a.node.input_status().unwrap().paused);
            assert!(!b.node.input_status().unwrap().paused);
            if cancel || timeout {
                // The retired target must stay inactive when source work returns.
                if cancel {
                    type_and_check(&b, &a, 2, "reverse input after late grant completion").await;
                }
                assert!(db.began.lock().unwrap().is_none());
                assert!(!db.calls.lock().unwrap().contains(&"begin"));
            } else {
                common::watchdog::until("ready grant starts native drag", || db.began.lock().unwrap().is_some()).await;
                drop_drag(&a, &b, &db, &da).await;
            }
            a.node.shutdown().await;
            b.node.shutdown().await;
        }
    });
}

#[test]
fn rejected_virtual_export_logs_reason_retires_target_and_next_crossing_starts() {
    use tracing_subscriber::prelude::*;
    #[derive(Clone)]
    struct Events(Arc<Mutex<Vec<String>>>);
    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Events {
        fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
            if !matches!(event.metadata().target(), "oink_file_diagnostics" | "oink_agent::dnd") {
                return;
            }
            struct Fields(String);
            impl tracing::field::Visit for Fields {
                fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                    use std::fmt::Write;
                    let _ = write!(&mut self.0, "{}={value:?} ", field.name());
                }
            }
            let mut fields = Fields(String::new());
            event.record(&mut fields);
            eprintln!("{}", fields.0);
            self.0.lock().unwrap().push(fields.0);
        }
    }
    let events = Events(Arc::default());
    // Integration nodes spawn their own workers: a thread-local subscriber would
    // miss the actual source send / target receive / target retirement events.
    tracing::subscriber::set_global_default(
        tracing_subscriber::registry().with(
            events
                .clone()
                .with_filter(tracing_subscriber::filter::LevelFilter::WARN),
        ),
    )
    .unwrap();
    common::watchdog::run(module_path!(), "definitive virtual source rejection", Some(8), async {
        let root = tempfile::tempdir().unwrap();
        let (a, b, da, db) = drag_pair(root.path()).await;
        let path = a.dir.join("virtual.png");
        std::fs::write(&path, b"one-shot content").unwrap();
        let files = Arc::new(SlowVirtual {
            files: Local::new(vec![(NodeId(1), path)]).unwrap(),
            gate: Mutex::default(),
            failure: Mutex::new(Some(Unavailable::Permission)),
        });
        let (entered, release) = boundary(&files.gate);
        let inventory = Inventory {
            id: OfferId {
                clip: oink_foundation::content::ClipId {
                    origin: a.node.id(),
                    seq: 0,
                },
                generation: 0,
            },
            platform: oink_foundation::clipboard::ClipPlatform::Mac,
            roots: files.roots(),
            items: Vec::new(),
        };
        let items = oink_clipboard::content::items(&inventory);
        *da.offer.lock().unwrap() = Some(items.clone());
        let provider = Arc::new(ExportFiles {
            inventory,
            files: files.clone(),
        });
        // Both native backends report source completion asynchronously. Keep
        // that callback pending across pointer restoration so the old content
        // retirement cannot accidentally finish before the readiness check.
        da.defer_source_end.store(true, std::sync::atomic::Ordering::Release);
        *da.export_provider.lock().unwrap() = Some(provider.clone());
        button(&a.input, true);
        cross(&a, &b, 1.).await;
        reached(entered).await;
        let start = Instant::now();
        release.send(()).unwrap();
        common::watchdog::until("specific rejection returned from real Low service", || {
            events.0.lock().unwrap().iter().any(|event| {
                event.contains("cross-device drag preparation failed") && event.contains("reason=Permission")
            })
        })
        .await;
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "definitive rejection waited for the timeout"
        );
        common::watchdog::until("failed target is retired", || {
            b.node.input_status().unwrap().dnd_host_pending.is_none()
        })
        .await;
        assert!(!db.calls.lock().unwrap().contains(&"begin"));
        {
            let observed = events.0.lock().unwrap();
            assert!(
                observed
                    .iter()
                    .any(|event| event.contains("file_stage=42") && event.contains("file_reason=Permission"))
            );
            assert!(
                observed
                    .iter()
                    .any(|event| event.contains("file_stage=43") && event.contains("file_reason=Permission"))
            );
            assert!(
                observed.iter().any(|event| event.contains("dnd_preparation_stage=3")
                    && event.contains("dnd_preparation_timeout=false"))
            );
        }
        button(&a.input, false);
        if holder(&a.node) == Some(b.node.id()) {
            cross(&b, &a, -1.).await;
        }
        common::watchdog::until("source pointer ready after rejection", || local_pointer_ready(&a)).await;
        common::watchdog::until("native source cancellation awaits its end callback", || {
            da.pending_source_end.lock().unwrap().is_some()
        })
        .await;
        let ended = da.pending_source_end.lock().unwrap().take().unwrap();
        da.sink.lock().unwrap().as_ref().unwrap()(ended);
        // Target retirement and local pointer mode do not acknowledge source
        // ReleaseData. The fixture has one provider slot; wait for its old
        // cleanup before installing the next native offer into that slot.
        common::watchdog::until("rejected source content is retired", || {
            da.calls.lock().unwrap().contains(&"forget")
        })
        .await;
        da.defer_source_end.store(false, std::sync::atomic::Ordering::Release);
        *files.failure.lock().unwrap() = None;
        *da.export_provider.lock().unwrap() = Some(provider);
        *da.offer.lock().unwrap() = Some(items);
        button(&a.input, true);
        cross(&a, &b, 1.).await;
        common::watchdog::until("next crossing starts after rejection", || {
            db.began.lock().unwrap().is_some()
        })
        .await;
        drop_drag(&a, &b, &db, &da).await;
        a.node.shutdown().await;
        b.node.shutdown().await;
    });
}
