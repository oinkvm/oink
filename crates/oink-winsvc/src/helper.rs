//! Helper-side lanes: ordinary queries own no injection state. Generation
//! changes are cleanup barriers on the sole native owner, including reconnects.
use crate::{
    owner::{CAPACITY, Wake},
    wire::{Command, Reply, Request},
};
use crossbeam_queue::ArrayQueue;
use std::{
    io,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
pub trait Native: 'static {
    fn apply(&mut self, command: Command) -> Reply;
    fn release(&mut self);
    fn maintain(&mut self);
}
pub trait Query: 'static {
    fn query(&mut self, command: Command) -> Reply;
}
struct Cleanup<N: Native>(N);
impl<N: Native> Drop for Cleanup<N> {
    fn drop(&mut self) {
        // Cleanup also owns unwinding: a native worker panic must not abandon
        // its held-key ledger merely because the pipe owner is still alive.
        self.0.release();
    }
}
#[derive(Clone, Copy, Debug)]
pub struct Work {
    pub generation: u64,
    pub request: Request,
}
#[derive(Clone, Copy, Debug)]
pub struct Result {
    pub generation: u64,
    pub ordinary: bool,
    pub reply: Reply,
}
struct Lane {
    requests: ArrayQueue<Work>,
    replies: ArrayQueue<Result>,
    worker: OnceLock<thread::Thread>,
}
impl Lane {
    fn new() -> Self {
        Self {
            requests: ArrayQueue::new(CAPACITY),
            replies: ArrayQueue::new(CAPACITY),
            worker: OnceLock::new(),
        }
    }
    fn wake(&self) {
        if let Some(worker) = self.worker.get() {
            worker.unpark();
        }
    }
}
struct Shared {
    input: Lane,
    ordinary: Lane,
    generation: AtomicU64,
    failed: AtomicU64,
    stopped: AtomicBool,
    notify: Wake,
}
pub struct Server {
    state: Arc<Shared>,
    native: Option<thread::JoinHandle<()>>,
    query: Option<thread::JoinHandle<()>>,
    next: u64,
}
impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HelperServer")
            .field("generation", &self.state.generation.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}
impl Server {
    pub fn new<N: Native, Q: Query>(
        native: impl FnOnce() -> io::Result<N> + Send + 'static,
        query: impl FnOnce() -> Q + Send + 'static,
        notify: Wake,
    ) -> io::Result<Self> {
        let state = Arc::new(Shared {
            input: Lane::new(),
            ordinary: Lane::new(),
            generation: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            stopped: AtomicBool::new(false),
            notify,
        });
        let shared = state.clone();
        let (ready, started) = std::sync::mpsc::sync_channel(1);
        let native = thread::Builder::new()
            .name("oink-helper-inject".into())
            .spawn(move || {
                let mut backend = Cleanup(match native() {
                    Ok(n) => {
                        let _ = ready.send(Ok(()));
                        n
                    }
                    Err(e) => {
                        let _ = ready.send(Err(e));
                        return;
                    }
                });
                let _ = shared.input.worker.set(thread::current());
                let mut generation = 0;
                let mut maintenance = Instant::now();
                loop {
                    let current = shared.generation.load(Ordering::Acquire);
                    if generation != current {
                        backend.0.release();
                        generation = current;
                    }
                    if shared.stopped.load(Ordering::Acquire) {
                        break;
                    }
                    if let Some(work) = shared.input.requests.pop() {
                        // The cleanup snapshot above can become stale while
                        // release calls the OS. A reconnect may already have
                        // accepted this work: compare against the live session,
                        // never silently discard it using the old snapshot.
                        if work.generation != shared.generation.load(Ordering::Acquire)
                            || work.generation == 0
                            || shared.failed.load(Ordering::Acquire) == work.generation
                        {
                            continue;
                        }
                        if generation != work.generation {
                            backend.0.release();
                            generation = work.generation;
                        }
                        // Cleanup itself can span another disconnect. Retired
                        // work must not enter the OS or be replayed in a new session.
                        if generation != shared.generation.load(Ordering::Acquire) {
                            continue;
                        }
                        let reply = backend.0.apply(work.request.command).with_id(work.request.id);
                        if reply.0[..4] != [0; 4] {
                            shared.failed.store(generation, Ordering::Release);
                        }
                        if generation == shared.generation.load(Ordering::Acquire) {
                            if shared
                                .input
                                .replies
                                .push(Result {
                                    generation,
                                    ordinary: false,
                                    reply,
                                })
                                .is_err()
                            {
                                shared.generation.store(0, Ordering::Release);
                            }
                            (shared.notify)();
                        }
                        if reply.0[..4] != [0; 4] {
                            backend.0.release();
                        }
                        continue;
                    }
                    let now = Instant::now();
                    if now >= maintenance {
                        backend.0.maintain();
                        maintenance = now + Duration::from_millis(100);
                    }
                    if !shared.input.requests.is_empty() || generation != shared.generation.load(Ordering::Acquire) {
                        continue;
                    }
                    thread::park_timeout(maintenance.saturating_duration_since(Instant::now()));
                }
            })?;
        started.recv().map_err(io::Error::other)??;
        // Own the native worker before the second spawn. A query-spawn failure
        // then follows the same stop/cleanup path as a normal helper shutdown.
        let mut server = Self {
            state: state.clone(),
            native: Some(native),
            query: None,
            next: 0,
        };
        let shared = state.clone();
        let query = thread::Builder::new().name("oink-helper-query".into()).spawn(move || {
            let mut backend = query();
            let _ = shared.ordinary.worker.set(thread::current());
            loop {
                if shared.stopped.load(Ordering::Acquire) {
                    break;
                }
                if let Some(work) = shared.ordinary.requests.pop() {
                    if work.generation != shared.generation.load(Ordering::Acquire) {
                        continue;
                    }
                    let reply = backend.query(work.request.command).with_id(work.request.id);
                    if work.generation == shared.generation.load(Ordering::Acquire) {
                        let _ = shared.ordinary.replies.push(Result {
                            generation: work.generation,
                            ordinary: true,
                            reply,
                        });
                        (shared.notify)();
                    }
                    continue;
                }
                thread::park();
            }
        })?;
        server.query = Some(query);
        Ok(server)
    }
    pub fn begin(&mut self) -> u64 {
        self.next += 1;
        self.state.generation.store(self.next, Ordering::Release);
        self.state.input.wake();
        self.next
    }
    pub fn disconnect(&self) {
        self.state.generation.store(0, Ordering::Release);
        self.state.input.wake();
    }
    pub fn submit(&self, work: Work) -> io::Result<()> {
        if work.generation == 0
            || work.generation != self.state.generation.load(Ordering::Acquire)
            || self.state.failed.load(Ordering::Acquire) == work.generation
        {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "stale helper session"));
        }
        let lane = if work.request.ordinary {
            &self.state.ordinary
        } else {
            &self.state.input
        };
        if work.request.ordinary && !matches!(work.request.command, Command::Ping | Command::Position) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "ordinary mutation"));
        }
        lane.requests
            .push(work)
            .map_err(|_| io::Error::new(io::ErrorKind::WouldBlock, "helper lane full"))?;
        lane.wake();
        Ok(())
    }
    pub fn receive(&self) -> Option<Result> {
        self.state
            .input
            .replies
            .pop()
            .or_else(|| self.state.ordinary.replies.pop())
    }
    pub fn stop_native(&mut self) {
        self.state.stopped.store(true, Ordering::Release);
        self.state.input.wake();
        self.state.ordinary.wake();
        if let Some(native) = self.native.take() {
            let _ = native.join();
        }
        // An ordinary system query may be stuck. It owns no held-input state;
        // helper process exit must not wait for it before native cleanup.
        self.query.take();
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop_native();
    }
}
