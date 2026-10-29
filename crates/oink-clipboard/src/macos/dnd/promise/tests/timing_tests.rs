use super::*;
use std::collections::BTreeMap;
use std::io::Write;
use std::sync::mpsc::{self, Receiver, Sender};
use tracing_subscriber::{Layer, layer::SubscriberExt};

type Row = BTreeMap<String, String>;

#[derive(Clone, Default)]
struct Records(Arc<Mutex<Vec<Row>>>, Option<Sender<Row>>);
impl<S: tracing::Subscriber> Layer<S> for Records {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        if event.fields().any(|field| field.name() == "file_promise_event") {
            let mut fields = BTreeMap::new();
            event.record(&mut Fields(&mut fields));
            self.0.lock().unwrap().push(fields.clone());
            if let Some(events) = &self.1 {
                let _ = events.send(fields);
            }
        }
    }
}

fn observed(events: &Receiver<Row>, code: &str, size: Option<&str>) -> Row {
    let row = events.recv().expect("observer stopped without an event");
    assert_eq!(row["file_promise_event"], code, "{row:?}");
    if let Some(size) = size {
        assert_eq!(row["file_size"], size, "{row:?}");
    }
    row
}
struct Fields<'a>(&'a mut BTreeMap<String, String>);
impl tracing::field::Visit for Fields<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.insert(field.name().into(), format!("{value:?}"));
    }
}

fn snapshot(root: &Path, source: &content::Deferred) -> Arc<dyn Provider> {
    let batch = Batch::create(root, StoreKind::SourceSnapshot, BatchId::random().unwrap()).unwrap();
    content::Snapshot::from_parts(source.inventory(), batch, source.translator()).unwrap()
}

#[test]
fn promise_perform_logs_return_before_deferred_main_heartbeat() {
    let records = Records::default();
    let mut queued = None;
    tracing::subscriber::with_default(tracing_subscriber::registry().with(records.clone()), || {
        let mut perform = timing::perform(77).unwrap();
        perform.bind(42);
        // Substitute only the enqueue operation. No native event loop or input
        // is claimed here; the production guard uses the always-deferred queue.
        perform.finish(|job| queued = Some(job));
        assert_eq!(
            records
                .0
                .lock()
                .unwrap()
                .iter()
                .map(|row| row["file_promise_event"].as_str())
                .collect::<Vec<_>>(),
            ["1", "2"]
        );
    });
    queued.unwrap()();
    let rows = records.0.lock().unwrap();
    assert_eq!(
        rows.iter()
            .map(|row| row["file_promise_event"].as_str())
            .collect::<Vec<_>>(),
        ["1", "2", "5"]
    );
    assert!(rows.iter().all(|row| row["promise_native_sequence"] == "77"));
    assert_eq!(rows[1]["file_request"], "42");
    assert_eq!(rows[2]["file_request"], "42");
}

#[test]
fn promise_native_receipt_logs_submission_return_and_reader_without_paths() {
    objc2::rc::autoreleasepool(|_| {
        let root = tempfile::tempdir().unwrap();
        let backend = MacDnd::new();
        let source = deferred_promise();
        let operation = DragOperation::new();
        let export = Export::prepare(root.path(), operation, source.clone(), vec![], &backend.shared).unwrap();
        let records = Records::default();
        tracing::subscriber::with_default(tracing_subscriber::registry().with(records.clone()), || {
            // SAFETY: callback-only test double; no AppKit session or desktop input.
            let receiver: Retained<MissingFileReceiver> = unsafe { msg_send![MissingFileReceiver::alloc(), init] };
            assert!(export.receive_snapshot(&[receiver.into_super()], snapshot(root.path(), &source)));
        });
        let rows = records.0.lock().unwrap();
        let position = |code: &str| {
            rows.iter()
                .position(|row| row.get("file_promise_event").is_some_and(|value| value == code))
                .unwrap_or_else(|| panic!("missing native timing event {code}: {rows:?}"))
        };
        // This receiver calls back synchronously. Logging must allow that order,
        // rather than pretending native submission returns before every reader.
        assert!(position("3") < position("6"));
        assert!(position("6") < position("4"));
        for code in ["3", "4", "6"] {
            let row = &rows[position(code)];
            assert_eq!(row.get("file_request"), Some(&operation.sequence.to_string()));
            assert!(row["promise_at_us"].parse::<u64>().is_ok());
            for (field, value) in row.iter().filter(|(field, _)| field.as_str() != "message") {
                assert!(value.parse::<i128>().is_ok(), "non-numeric {field}={value}");
            }
        }
        drop(rows);
        export.batch.external_writers_finished().unwrap();
    });
}

#[test]
fn promise_incoming_observer_logs_creation_and_growth_without_names() {
    // Exercise both startup orders: the initial enumeration must also find a
    // file created before the directory's kqueue registration.
    for present_before_start in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let incoming = root.path().join("must-not-appear-in-diagnostics.bin");
        let writer = present_before_start.then(|| fs::File::create(&incoming).unwrap());
        let (send, events) = mpsc::channel();
        let mut records = Records(Arc::default(), Some(send));
        let trace = tracing::subscriber::with_default(tracing_subscriber::registry().with(records.clone()), || {
            timing::Trace::new(42, Instant::now())
        });
        // Only the captured dispatch owns a sender, so a worker panic also
        // disconnects the event channel instead of leaving the test waiting.
        drop(records.1.take());
        // The subscriber scope has ended before this worker starts. Trace must
        // retain and restore its dispatch for every event on the other thread.
        std::thread::scope(|scope| {
            let (stop, stopped) = mpsc::channel::<()>();
            let path = root.path();
            let observer = scope.spawn(move || {
                let result = timing::observe_inner(
                    path,
                    &trace,
                    || !matches!(stopped.try_recv(), Err(mpsc::TryRecvError::Empty)),
                    None,
                );
                // Surface early exit/errors to the waiting test as an event.
                trace.event(
                    timing::Event::ObserverStopped,
                    0,
                    Duration::ZERO,
                    i64::from(result.is_err()),
                );
                result
            });
            observed(&events, "14", None);
            let mut writer = writer.unwrap_or_else(|| fs::File::create(&incoming).unwrap());
            // fs::write is create/truncate followed by write, not an atomic
            // publication. First observation can legitimately report size 0;
            // acknowledging it before writing makes that interleaving explicit.
            let first = observed(&events, "7", Some("0"));
            writer.write_all(b"one").unwrap();
            let grown = observed(&events, "8", Some("3"));
            writer.write_all(b" more bytes").unwrap();
            let grown_again = observed(&events, "8", Some("14"));
            assert_eq!(first["file_index"], grown["file_index"]);
            assert_eq!(first["file_index"], grown_again["file_index"]);
            let rows = format!("{:?}", records.0.lock().unwrap());
            assert!(!rows.contains("must-not-appear"));
            assert!(!rows.contains(&root.path().to_string_lossy().to_string()));
            // Dropping the sender also cancels the worker if an assertion
            // unwinds; the scope always joins it before the directory is freed.
            drop(stop);
            observer.join().unwrap().unwrap();
        });
    }
}
