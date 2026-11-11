//! Bounded file queue with a reusable drain barrier (including from a panic hook).
use crossbeam_channel::{Sender, TrySendError, bounded};
use std::{
    io::{self, Write},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tracing_subscriber::fmt::MakeWriter;

enum Message {
    Line(Vec<u8>),
    Flush(Sender<io::Result<()>>),
}

const QUEUE_CAPACITY: usize = 1024;
const BATCH_RECORDS: usize = 256;
const BATCH_BYTES: usize = 64 * 1024;

/// Hands formatted records to one background thread that appends them.
#[derive(Clone, Debug)]
pub struct FileWriter {
    send: Sender<Message>,
    /// Lossy writers count records refused by a full queue instead of waiting.
    dropped: Option<Arc<AtomicU64>>,
}
impl FileWriter {
    /// A full queue applies backpressure to the logging thread; no produced
    /// record is discarded.
    pub fn new(file: impl Write + Send + 'static) -> io::Result<Self> {
        Self::spawn(file, None)
    }

    /// Never waits on the file: a full queue drops the record and a later
    /// record reports how many were dropped, and `flush` does not wait either.
    /// For processes whose logging threads must never stall on a filesystem.
    pub fn lossy(file: impl Write + Send + 'static) -> io::Result<Self> {
        Self::spawn(file, Some(Arc::default()))
    }

    fn spawn(mut file: impl Write + Send + 'static, dropped: Option<Arc<AtomicU64>>) -> io::Result<Self> {
        let (send, receive) = bounded(QUEUE_CAPACITY);
        std::thread::Builder::new()
            .name("oink-log-writer".into())
            .spawn(move || {
                let mut batch = Vec::with_capacity(BATCH_BYTES);
                while let Ok(mut message) = receive.recv() {
                    batch.clear();
                    let mut flush = None;
                    for record in 0..BATCH_RECORDS {
                        match message {
                            Message::Line(bytes) => batch.extend_from_slice(&bytes),
                            Message::Flush(done) => {
                                flush = Some(done);
                                break;
                            }
                        }
                        if batch.len() >= BATCH_BYTES || record + 1 == BATCH_RECORDS {
                            break;
                        }
                        // Batch only records already queued; an idle tail must
                        // reach the file without a timer or a later log event.
                        // Never receive past the bound: that would remove one
                        // record from the channel without adding it to a batch.
                        if let Ok(next) = receive.try_recv() {
                            message = next;
                        } else {
                            break;
                        }
                    }
                    // The rolling writer locks/prunes/opens once per write.
                    // Amortize that work so a full queue can drain within the
                    // panic hook's unchanged deadline. A barrier ends the batch
                    // and is acknowledged only after its preceding bytes flush.
                    if !batch.is_empty()
                        && let Err(error) = file.write_all(&batch)
                    {
                        eprintln!("Oink log write failed: {error}");
                    }
                    if let Some(done) = flush {
                        let _ = done.send(file.flush());
                    }
                }
            })?;
        Ok(Self { send, dropped })
    }

    /// Wait until every record queued before this call has been appended.
    pub fn drain(&self) -> io::Result<()> {
        let (send, receive) = bounded(1);
        self.send.send(Message::Flush(send)).map_err(|_| disconnected())?;
        receive.recv().map_err(|_| disconnected())?
    }

    /// [`drain`](Self::drain), giving up at `deadline`. Returns whether the
    /// queued records reached the file in time.
    pub fn drain_until(&self, deadline: Instant) -> bool {
        let (send, receive) = bounded(1);
        self.send.send_deadline(Message::Flush(send), deadline).is_ok()
            && matches!(receive.recv_deadline(deadline), Ok(Ok(())))
    }

    // A panic must not wait forever on a broken filesystem. The normal entry
    // boundary also drains on unwind; abort profiles have only this hook.
    pub fn record_panic(&self, message: String) {
        let deadline = Instant::now() + Duration::from_secs(2);
        if self
            .send
            .send_deadline(Message::Line(message.into_bytes()), deadline)
            .is_err()
        {
            return;
        }
        let _ = self.drain_until(deadline);
    }
}
fn disconnected() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "log writer stopped")
}
impl Write for FileWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let Some(dropped) = &self.dropped else {
            // Backpressure instead of silently discarding a produced record when
            // the queue fills. No filesystem work runs on the logging thread.
            self.send
                .send(Message::Line(bytes.to_vec()))
                .map_err(|_| disconnected())?;
            return Ok(bytes.len());
        };
        match self.send.try_send(Message::Line(bytes.to_vec())) {
            Ok(()) => {
                let lost = dropped.swap(0, Ordering::Relaxed);
                if lost > 0 {
                    let notice = format!("WARN oink_log: {lost} earlier log records dropped: log queue full\n");
                    if self.send.try_send(Message::Line(notice.into_bytes())).is_err() {
                        dropped.fetch_add(lost, Ordering::Relaxed);
                    }
                }
            }
            Err(TrySendError::Full(_)) => {
                dropped.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Disconnected(_)) => return Err(disconnected()),
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        if self.dropped.is_some() {
            return Ok(());
        }
        self.drain()
    }
}
impl<'a> MakeWriter<'a> for FileWriter {
    type Writer = Self;
    fn make_writer(&'a self) -> Self {
        self.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::{Receiver, unbounded};

    #[derive(Debug, PartialEq)]
    enum Operation {
        Write(Vec<u8>),
        Flush,
    }

    struct ObservedWriter {
        operations: Sender<Operation>,
        first_write: Option<Receiver<()>>,
    }
    impl Write for ObservedWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.operations.send(Operation::Write(bytes.to_vec())).unwrap();
            if let Some(release) = self.first_write.take() {
                release.recv().unwrap();
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            self.operations.send(Operation::Flush).unwrap();
            Ok(())
        }
    }

    fn queued_burst(record_bytes: usize) {
        let (operations, observed) = unbounded();
        let (release, wait) = bounded(1);
        let mut writer = FileWriter::new(ObservedWriter {
            operations,
            first_write: Some(wait),
        })
        .unwrap();
        writer.write_all(b"first").unwrap();
        assert_eq!(observed.recv().unwrap(), Operation::Write(b"first".to_vec()));

        // Fill the real bounded queue while the first append is in flight.
        // Two barriers separate distinct byte patterns; batching must neither
        // cross a barrier nor lose the record at a batch's size/count boundary.
        let records = (QUEUE_CAPACITY - 2) / 2;
        let mut acknowledgements = Vec::new();
        for &byte in b"ab" {
            for _ in 0..records {
                writer.write_all(&vec![byte; record_bytes]).unwrap();
            }
            let (done, acknowledgement) = bounded(1);
            writer.send.send(Message::Flush(done)).unwrap();
            acknowledgements.push(acknowledgement);
        }
        release.send(()).unwrap();
        let mut writes = 1;
        for (byte, acknowledgement) in b"ab".iter().copied().zip(acknowledgements) {
            let mut bytes = Vec::new();
            while let Operation::Write(batch) = observed.recv_timeout(Duration::from_secs(2)).unwrap() {
                assert!(batch.len() <= BATCH_BYTES);
                bytes.extend_from_slice(&batch);
                writes += 1;
            }
            acknowledgement.recv_timeout(Duration::from_secs(2)).unwrap().unwrap();
            assert_eq!(bytes, vec![byte; records * record_bytes]);
        }
        let batch_records = BATCH_RECORDS.min(BATCH_BYTES / record_bytes);
        assert!(
            writes <= 1 + 2 * records.div_ceil(batch_records),
            "{writes} appends for a queued burst"
        );
    }

    #[test]
    fn queued_records_share_appends_without_crossing_flush_barriers() {
        queued_burst(4);
    }

    #[test]
    fn large_records_bound_each_batch_without_losing_the_next_record() {
        queued_burst(8 * 1024);
    }

    #[test]
    fn idle_tail_reaches_the_file_without_another_record_or_barrier() {
        let (operations, observed) = unbounded();
        let mut writer = FileWriter::new(ObservedWriter {
            operations,
            first_write: None,
        })
        .unwrap();
        writer.write_all(b"last record").unwrap();
        assert_eq!(
            observed.recv_timeout(Duration::from_secs(2)).unwrap(),
            Operation::Write(b"last record".to_vec())
        );
        writer.drain().unwrap();
        assert_eq!(observed.recv().unwrap(), Operation::Flush);
    }

    #[test]
    fn lossy_writer_never_waits_on_a_stalled_file_and_reports_the_loss() {
        let (operations, observed) = unbounded();
        let (release, wait) = bounded(1);
        let mut writer = FileWriter::lossy(ObservedWriter {
            operations,
            first_write: Some(wait),
        })
        .unwrap();
        writer.write_all(b"first").unwrap();
        assert_eq!(observed.recv().unwrap(), Operation::Write(b"first".to_vec()));
        // The file is stalled inside the first append: overfill the queue.
        let started = Instant::now();
        for _ in 0..QUEUE_CAPACITY + 10 {
            writer.write_all(b"x").unwrap();
        }
        writer.flush().unwrap();
        assert!(!writer.drain_until(Instant::now() + Duration::from_millis(50)));
        assert!(started.elapsed() < Duration::from_secs(2), "lossy writes must not wait");
        release.send(()).unwrap();
        // The stale barrier above is still queued; wait for a fresh one.
        assert!(writer.drain_until(Instant::now() + Duration::from_secs(2)));
        writer.write_all(b"after").unwrap();
        assert!(writer.drain_until(Instant::now() + Duration::from_secs(2)));
        let mut bytes = Vec::new();
        while let Ok(operation) = observed.try_recv() {
            if let Operation::Write(batch) = operation {
                bytes.extend_from_slice(&batch);
            }
        }
        let text = String::from_utf8(bytes).unwrap();
        assert_eq!(text.matches('x').count(), QUEUE_CAPACITY);
        assert!(text.contains("after"));
        assert!(text.contains("WARN oink_log: 10 earlier log records dropped"), "{text}");
    }
}
