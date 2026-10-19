//! A single pipe owner, with one outstanding request per lane. An ordinary
//! round trip never owns the caller handoff or stops input/ACK progress.
use crate::wire::{Command, Reply, Request};
use crossbeam_queue::ArrayQueue;
use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};
pub const CAPACITY: usize = 64;
pub type Wake = Arc<dyn Fn() + Send + Sync>;
pub trait Transport: Send + 'static {
    fn now(&self) -> Instant {
        Instant::now()
    }
    /// Accept without waiting for an ordinary write. True means already written;
    /// false reports completion later through written(), while ACK reads progress.
    fn send(&mut self, request: Request) -> io::Result<bool>;
    fn written(&mut self) -> io::Result<Option<u32>> {
        Ok(None)
    }
    fn receive(&mut self) -> io::Result<Option<Reply>>;
    fn wait(&mut self, timeout: Duration) -> io::Result<()>;
    fn connected(&self) -> bool;
    fn disconnect(&mut self);
}
struct Work {
    command: Command,
    only_connected: bool,
    reply: Option<Arc<Completion>>,
}
struct Completion {
    value: ArrayQueue<io::Result<Option<Reply>>>,
    waiter: thread::Thread,
}
impl Completion {
    fn finish(&self, result: io::Result<Option<Reply>>) {
        // A private, single-use reply slot: waking an ordinary caller must not
        // acquire that caller's channel mutex on the realtime pipe owner.
        assert!(self.value.push(result).is_ok(), "duplicate helper ACK");
        self.waiter.unpark();
    }
    fn wait(&self) -> io::Result<Option<Reply>> {
        loop {
            if let Some(result) = self.value.pop() {
                return result;
            }
            thread::park();
        }
    }
}
impl Work {
    fn complete(mut self, result: io::Result<Option<Reply>>) {
        self.reply.take().expect("one reply owner").finish(result);
    }
}
impl Drop for Work {
    fn drop(&mut self) {
        // Includes owner unwind and transport errors before a flight is stored.
        if let Some(reply) = self.reply.take() {
            reply.finish(Err(io::Error::from(io::ErrorKind::BrokenPipe)));
        }
    }
}
struct Shared {
    input: ArrayQueue<Work>,
    ordinary: ArrayQueue<Work>,
    stop: AtomicBool,
    wake: Wake,
}
impl Shared {
    fn fail_pending(&self) {
        while let Some(work) = self.input.pop().or_else(|| self.ordinary.pop()) {
            fail(work, io::ErrorKind::BrokenPipe);
        }
    }
}
struct Exit<'a>(&'a Shared);
impl Drop for Exit<'_> {
    fn drop(&mut self) {
        self.0.stop.store(true, Ordering::Release);
        self.0.fail_pending();
    }
}
struct Flight {
    id: u32,
    work: Option<Work>,
    deadline: Option<Instant>,
    writing: bool,
}
pub struct Client {
    shared: Arc<Shared>,
    worker: std::sync::Mutex<Option<thread::JoinHandle<()>>>,
}
impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HelperClient")
            .field("input_pending", &self.shared.input.len())
            .finish_non_exhaustive()
    }
}
impl Client {
    /// Retire the authenticated injection session and discard pending requests.
    /// Existing callers observe failure; no command is replayed on a new owner.
    pub fn shutdown(&self) {
        self.shared.stop.store(true, Ordering::Release);
        (self.shared.wake)();
        if let Some(worker) = self.worker.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = worker.join();
        }
    }
    #[cfg(test)]
    pub(crate) fn input_pending(&self) -> usize {
        self.shared.input.len()
    }
    #[cfg(test)]
    pub(crate) fn stop_for_test(&self) {
        self.shared.stop.store(true, Ordering::Release);
        (self.shared.wake)();
    }
    pub fn new<T: Transport>(
        transport: T,
        wake: Wake,
        setup: impl FnOnce() -> io::Result<()> + Send + 'static,
    ) -> io::Result<Self> {
        let shared = Arc::new(Shared {
            input: ArrayQueue::new(CAPACITY),
            ordinary: ArrayQueue::new(CAPACITY),
            stop: AtomicBool::new(false),
            wake,
        });
        let state = shared.clone();
        let (ready, started) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("oink-helper-client".into())
            .spawn(move || {
                let _exit = Exit(&state);
                let result = setup();
                let ok = result.is_ok();
                let _ = ready.send(result);
                if ok {
                    run(transport, &state);
                }
            })?;
        started.recv().map_err(io::Error::other)??;
        Ok(Self {
            shared,
            worker: std::sync::Mutex::new(Some(worker)),
        })
    }
    pub fn request(&self, command: Command, ordinary: bool, only_connected: bool) -> io::Result<Option<Reply>> {
        if command == Command::Heartbeat || ordinary && !matches!(command, Command::Ping | Command::Position) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "ordinary input command"));
        }
        if self.shared.stop.load(Ordering::Acquire) {
            return Err(io::Error::from(io::ErrorKind::BrokenPipe));
        }
        let result = Arc::new(Completion {
            value: ArrayQueue::new(1),
            waiter: thread::current(),
        });
        let queue = if ordinary {
            &self.shared.ordinary
        } else {
            &self.shared.input
        };
        queue
            .push(Work {
                command,
                only_connected,
                reply: Some(result.clone()),
            })
            .map_err(|_| io::Error::new(io::ErrorKind::WouldBlock, "helper request reservation exceeded"))?;
        // Close the enqueue/worker-exit race without a producer mutex. A failed
        // owner must fail queued ACK waiters, including work published at exit.
        if self.shared.stop.load(Ordering::Acquire) {
            self.shared.fail_pending();
        }
        (self.shared.wake)();
        result.wait()
    }
}
impl Drop for Client {
    fn drop(&mut self) {
        self.shutdown();
    }
}
fn fail(work: Work, kind: io::ErrorKind) {
    work.complete(Err(io::Error::from(kind)));
}
fn run<T: Transport>(mut transport: T, shared: &Shared) {
    let mut flights: [Option<Flight>; 2] = [None, None];
    let mut late_writes = [None, None];
    let mut next_id = 1u32;
    let mut heartbeat = transport.now() + Duration::from_millis(400);
    loop {
        if shared.stop.load(Ordering::Acquire) {
            break;
        }
        let mut error = None;
        // Input always gets the first send and only one input can be in flight:
        // an UP/Release/DnD barrier cannot overtake the preceding native ACK.
        for (lane, queue) in [&shared.input, &shared.ordinary].into_iter().enumerate() {
            if flights[lane].is_some() {
                continue;
            }
            let Some(work) = queue.pop() else {
                continue;
            };
            if work.only_connected && !transport.connected() {
                work.complete(Ok(None));
                continue;
            }
            let id = loop {
                let id = next_id;
                next_id = (next_id + 1) & 0x7fff_ffff;
                if next_id == 0 {
                    next_id = 1;
                }
                if !flights.iter().flatten().any(|f| f.id == id) && !late_writes.contains(&Some(id)) {
                    break id;
                }
            };
            match transport.send(Request {
                id,
                ordinary: lane == 1,
                command: work.command,
            }) {
                Ok(written) => {
                    flights[lane] = Some(Flight {
                        id,
                        work: Some(work),
                        deadline: Some(transport.now() + Duration::from_millis(150)),
                        writing: !written,
                    })
                }
                Err(e) => {
                    work.complete(Err(e));
                    error = Some(io::ErrorKind::BrokenPipe);
                    break;
                }
            }
        }
        loop {
            match transport.written() {
                Ok(Some(0)) => {} // lease heartbeat has no reply/flight
                Ok(Some(id)) => {
                    if let Some(slot) = late_writes.iter_mut().find(|slot| **slot == Some(id)) {
                        *slot = None;
                    } else if let Some(flight) = flights.iter_mut().flatten().find(|f| f.id == id && f.writing) {
                        flight.writing = false;
                        flight.deadline = Some(transport.now() + Duration::from_millis(150));
                    } else {
                        error = Some(io::ErrorKind::InvalidData);
                        break;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    error = Some(e.kind());
                    break;
                }
            }
        }
        loop {
            match transport.receive() {
                Ok(Some(reply)) => {
                    let id = reply.id();
                    if let Some(lane) = flights.iter().position(|f| f.as_ref().is_some_and(|f| f.id == id)) {
                        let flight = flights[lane].take().unwrap();
                        if flight.writing {
                            // An ACK proves delivery even if the local kernel's
                            // write notification arrives later. Consume it once;
                            // a later write completion is not an unrelated ACK.
                            late_writes[lane] = Some(id);
                        }
                        if let Some(work) = flight.work {
                            work.complete(Ok(Some(reply)));
                        }
                        if lane == 0 && reply.0[..4] != [0; 4] {
                            error = Some(io::ErrorKind::BrokenPipe);
                        }
                    } else {
                        error = Some(io::ErrorKind::InvalidData);
                        break;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    error = Some(e.kind());
                    break;
                }
            }
        }
        let now = transport.now();
        for (lane, slot) in flights.iter_mut().enumerate() {
            if let Some(flight) = slot
                && flight.deadline.is_some_and(|at| now >= at)
            {
                if let Some(work) = flight.work.take() {
                    fail(work, io::ErrorKind::TimedOut);
                }
                flight.deadline = None;
                if lane == 0 || flight.writing {
                    error = Some(io::ErrorKind::TimedOut);
                }
                // Retain the ordinary flight until its late reply. No second
                // query can accumulate behind a stuck ordinary worker.
            }
        }
        if let Some(kind) = error {
            transport.disconnect();
            late_writes.fill(None);
            for flight in &mut flights {
                if let Some(f) = flight.take()
                    && let Some(work) = f.work
                {
                    fail(work, kind);
                }
            }
        }
        if now >= heartbeat {
            if transport.connected()
                && transport
                    .send(Request {
                        id: 0,
                        ordinary: false,
                        command: Command::Heartbeat,
                    })
                    .is_err()
            {
                transport.disconnect();
                late_writes.fill(None);
                for flight in &mut flights {
                    if let Some(f) = flight.take()
                        && let Some(work) = f.work
                    {
                        fail(work, io::ErrorKind::BrokenPipe);
                    }
                }
            }
            heartbeat = now + Duration::from_millis(400);
        }
        // Durable queue predicates prevent an IPC implementation consuming a
        // wake token from stranding a queued request. No cadence gates input.
        if flights[0].is_none() && !shared.input.is_empty() || flights[1].is_none() && !shared.ordinary.is_empty() {
            continue;
        }
        let deadline = flights
            .iter()
            .flatten()
            .filter_map(|f| f.deadline)
            .min()
            .unwrap_or(heartbeat)
            .min(heartbeat);
        if let Err(e) = transport.wait(deadline.saturating_duration_since(transport.now())) {
            transport.disconnect();
            late_writes.fill(None);
            for flight in &mut flights {
                if let Some(f) = flight.take()
                    && let Some(work) = f.work
                {
                    fail(work, e.kind());
                }
            }
        }
    }
    transport.disconnect();
    for flight in flights.into_iter().flatten() {
        if let Some(work) = flight.work {
            fail(work, io::ErrorKind::BrokenPipe);
        }
    }
    shared.fail_pending();
}
