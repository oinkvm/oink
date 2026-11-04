//! Bounded file queue with a reusable drain barrier (including from a panic hook).
use crossbeam_channel::{Sender, bounded};
use std::{
    io::{self, Write},
    time::{Duration, Instant},
};
use tracing_subscriber::fmt::MakeWriter;

enum Message {
    Line(Vec<u8>),
    Flush(Sender<io::Result<()>>),
}

#[derive(Clone)]
pub(super) struct FileWriter(Sender<Message>);
impl FileWriter {
    pub(super) fn new(mut file: impl Write + Send + 'static) -> io::Result<Self> {
        let (send, receive) = bounded(1024);
        std::thread::Builder::new()
            .name("oink-log-writer".into())
            .spawn(move || {
                while let Ok(message) = receive.recv() {
                    match message {
                        Message::Line(bytes) => {
                            if let Err(error) = file.write_all(&bytes) {
                                eprintln!("Oink log write failed: {error}");
                            }
                        }
                        Message::Flush(done) => {
                            let _ = done.send(file.flush());
                        }
                    }
                }
            })?;
        Ok(Self(send))
    }

    pub(super) fn drain(&self) -> io::Result<()> {
        let (send, receive) = bounded(1);
        self.0.send(Message::Flush(send)).map_err(|_| disconnected())?;
        receive.recv().map_err(|_| disconnected())?
    }

    // A panic must not wait forever on a broken filesystem. The normal entry
    // boundary also drains on unwind; abort profiles have only this hook.
    pub(super) fn record_panic(&self, message: String) {
        let deadline = Instant::now() + Duration::from_secs(2);
        if self
            .0
            .send_deadline(Message::Line(message.into_bytes()), deadline)
            .is_err()
        {
            return;
        }
        let (send, receive) = bounded(1);
        if self.0.send_deadline(Message::Flush(send), deadline).is_ok() {
            let _ = receive.recv_deadline(deadline);
        }
    }
}
fn disconnected() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "log writer stopped")
}
impl Write for FileWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        // Backpressure instead of silently discarding a produced record when
        // the queue fills. No filesystem work runs on the logging thread.
        self.0.send(Message::Line(bytes.to_vec())).map_err(|_| disconnected())?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.drain()
    }
}
impl<'a> MakeWriter<'a> for FileWriter {
    type Writer = Self;
    fn make_writer(&'a self) -> Self {
        self.clone()
    }
}
