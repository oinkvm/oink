//! Controlled transport/OS substitutes. These run the production client and
//! helper owners but never open a desktop, installed pipe, or inject real input.
use crate::{
    helper, owner,
    wire::{Command, REPLY_SIZE, Reply, Request},
};
use std::{
    io,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};
#[derive(Default)]
pub(crate) struct Os {
    pub events: Vec<Command>,
    pub held: Vec<Command>,
}
pub(crate) struct Native {
    pub os: Arc<Mutex<Os>>,
}
impl helper::Native for Native {
    fn apply(&mut self, command: Command) -> Reply {
        let mut os = self.os.lock().unwrap();
        os.events.push(command);
        if command == (Command::Move { x: 99, y: 0 }) {
            let mut reply = Reply([0; REPLY_SIZE]);
            reply.0[0] = 5;
            return reply;
        }
        match command {
            Command::Button { button, down: true } => os.held.push(Command::Button { button, down: false }),
            Command::DragPress { .. } => os.held.push(Command::Button { button: 0, down: false }),
            Command::DragRelease { .. } => os.held.retain(|up| *up != (Command::Button { button: 0, down: false })),
            Command::Key {
                code,
                scan,
                extended,
                down: true,
            } => os.held.push(Command::Key {
                code,
                scan,
                extended,
                down: false,
            }),
            Command::Button { .. } | Command::Key { .. } => os.held.retain(|up| *up != command),
            Command::Release => {
                let releases = std::mem::take(&mut os.held);
                os.events.extend(releases);
            }
            _ => {}
        }
        Reply([0; REPLY_SIZE])
    }
    fn release(&mut self) {
        let mut os = self.os.lock().unwrap();
        let releases = std::mem::take(&mut os.held);
        os.events.extend(releases);
    }
    fn maintain(&mut self) {}
}
pub(crate) struct Query {
    pub entered: mpsc::Sender<()>,
    pub gate: mpsc::Receiver<()>,
}
impl helper::Query for Query {
    fn query(&mut self, _: Command) -> Reply {
        self.entered.send(()).unwrap();
        self.gate.recv().unwrap();
        Reply([0; REPLY_SIZE])
    }
}
struct Loopback {
    server: helper::Server,
    generation: u64,
    connected: bool,
    starts: Arc<Mutex<usize>>,
    origin: Instant,
    clock: Arc<AtomicU64>,
    submitted: mpsc::Sender<Command>,
}
impl owner::Transport for Loopback {
    fn now(&self) -> Instant {
        self.origin + Duration::from_micros(self.clock.load(Ordering::Acquire))
    }
    fn send(&mut self, request: Request) -> io::Result<bool> {
        if !self.connected {
            self.generation = self.server.begin();
            self.connected = true;
            *self.starts.lock().unwrap() += 1;
        }
        if request.command == Command::Heartbeat {
            return Ok(true);
        }
        self.server.submit(helper::Work {
            generation: self.generation,
            request,
        })?;
        let _ = self.submitted.send(request.command);
        Ok(true)
    }
    fn receive(&mut self) -> io::Result<Option<Reply>> {
        while let Some(reply) = self.server.receive() {
            if reply.generation == self.generation {
                return Ok(Some(reply.reply));
            }
        }
        Ok(None)
    }
    fn wait(&mut self, timeout: Duration) -> io::Result<()> {
        thread::park_timeout(timeout);
        Ok(())
    }
    fn connected(&self) -> bool {
        self.connected
    }
    fn disconnect(&mut self) {
        self.connected = false;
        self.server.disconnect();
    }
}
struct Fixture {
    client: Arc<owner::Client>,
    os: Arc<Mutex<Os>>,
    starts: Arc<Mutex<usize>>,
    entered: mpsc::Receiver<()>,
    release: mpsc::Sender<()>,
    clock: Arc<AtomicU64>,
    wake: owner::Wake,
    submitted: mpsc::Receiver<Command>,
}
fn fixture() -> Fixture {
    let os = Arc::new(Mutex::new(Os::default()));
    let native_os = os.clone();
    fixture_native(os, move || Ok(Native { os: native_os }), None)
}
fn fixture_native<N: helper::Native>(
    os: Arc<Mutex<Os>>,
    native: impl FnOnce() -> io::Result<N> + Send + 'static,
    clock: Option<Instant>,
) -> Fixture {
    let (entered, ready) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let thread: Arc<OnceLock<thread::Thread>> = Arc::default();
    let wake_thread = thread.clone();
    let wake: owner::Wake = Arc::new(move || {
        if let Some(thread) = wake_thread.get() {
            thread.unpark();
        }
    });
    let server = helper::Server::new(native, move || Query { entered, gate }, wake.clone()).unwrap();
    let starts = Arc::new(Mutex::new(0));
    let clock_us = Arc::new(AtomicU64::new(0));
    let (submitted, submissions) = mpsc::channel();
    let transport = Loopback {
        server,
        generation: 0,
        connected: false,
        starts: starts.clone(),
        origin: clock.unwrap_or_else(Instant::now),
        clock: clock_us.clone(),
        submitted,
    };
    let client = Arc::new(
        owner::Client::new(transport, wake.clone(), move || {
            thread.set(thread::current()).unwrap();
            Ok(())
        })
        .unwrap(),
    );
    Fixture {
        client,
        os,
        starts,
        entered: ready,
        release,
        clock: clock_us,
        wake,
        submitted: submissions,
    }
}
#[test]
fn blocked_ordinary_round_trip_must_not_delay_input_ack() {
    let fixture = fixture();
    let ordinary = fixture.client.clone();
    let query = thread::spawn(move || ordinary.request(Command::Ping, true, false));
    fixture.entered.recv_timeout(Duration::from_secs(3)).unwrap();
    let input = fixture.client.clone();
    let (done, completed) = mpsc::channel();
    let injection = thread::spawn(move || {
        done.send(input.request(Command::Button { button: 0, down: true }, false, false))
            .unwrap();
    });
    let result = completed.recv_timeout(Duration::from_secs(3));
    // Always release the fake IPC before asserting, including a regressed owner.
    fixture.release.send(()).unwrap();
    injection.join().unwrap();
    let _ = query.join().unwrap();
    assert!(
        matches!(result, Ok(Ok(Some(_)))),
        "input ACK waited for ordinary IPC: {result:?}"
    );
    assert_eq!(*fixture.starts.lock().unwrap(), 1, "one authenticated connection");
    fixture.client.request(Command::Release, false, true).unwrap();
    assert!(fixture.os.lock().unwrap().held.is_empty());
}
#[test]
fn ordinary_timeout_does_not_disconnect_or_consume_an_input_ack() {
    let fixture = fixture();
    let ordinary = fixture.client.clone();
    let query = thread::spawn(move || ordinary.request(Command::Position, true, false));
    fixture.entered.recv_timeout(Duration::from_secs(3)).unwrap();
    fixture.clock.store(150_001, Ordering::Release);
    (fixture.wake)();
    assert_eq!(query.join().unwrap().unwrap_err().kind(), io::ErrorKind::TimedOut);
    let result = fixture.client.request(Command::Move { x: 1, y: 2 }, false, false);
    fixture.release.send(()).unwrap();
    assert!(result.is_ok());
    assert_eq!(*fixture.starts.lock().unwrap(), 1);
}
#[test]
fn native_error_releases_owned_keys_then_reconnects_without_replaying_input() {
    let fixture = fixture();
    let down = Command::Key {
        code: 30,
        scan: true,
        extended: false,
        down: true,
    };
    fixture.client.request(down, false, false).unwrap();
    let error = fixture
        .client
        .request(Command::Move { x: 99, y: 0 }, false, false)
        .unwrap()
        .unwrap();
    assert_ne!(error.0[0], 0);
    // The new-generation native barrier must finish release before applying this.
    fixture
        .client
        .request(Command::Move { x: 1, y: 2 }, false, false)
        .unwrap();
    let os = fixture.os.lock().unwrap();
    assert!(os.held.is_empty());
    assert_eq!(os.events.iter().filter(|c| **c == down).count(), 1);
    assert_eq!(*fixture.starts.lock().unwrap(), 2);
}

/// Force a generation change while the native owner is inside the cleanup for
/// its snapshot of generation 0. No wall-clock timeout or request replay is used.
struct ReconnectCleanupGate {
    native: Native,
    initial_idle: Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>,
    failed: bool,
    error_released: bool,
    cleanup_gate: Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>,
    after_cleanup: bool,
    checkpoint: Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>,
}
impl helper::Native for ReconnectCleanupGate {
    fn apply(&mut self, command: Command) -> Reply {
        let reply = self.native.apply(command);
        self.failed |= reply.0[..4] != [0; 4];
        reply
    }
    fn release(&mut self) {
        if self.failed {
            if self.error_released {
                if let Some((entered, gate)) = self.cleanup_gate.take() {
                    entered.send(()).unwrap();
                    gate.recv().unwrap();
                    self.after_cleanup = true;
                }
            } else {
                self.error_released = true;
            }
        }
        self.native.release();
    }
    fn maintain(&mut self) {
        if let Some((entered, gate)) = self.initial_idle.take() {
            entered.send(()).unwrap();
            gate.recv().unwrap();
        }
        if self.after_cleanup
            && let Some((entered, gate)) = self.checkpoint.take()
        {
            // Reached only after the queued post-reconnect work was considered.
            // On the old code it was discarded, on fixed code it was applied.
            entered.send(()).unwrap();
            gate.recv().unwrap();
        }
    }
}
#[test]
fn reconnect_during_disconnected_generation_cleanup_cannot_discard_new_input() {
    let os = Arc::new(Mutex::new(Os::default()));
    let native_os = os.clone();
    let (initial, inside_initial) = mpsc::channel();
    let (start, start_initial) = mpsc::channel();
    let (cleanup, inside_cleanup) = mpsc::channel();
    let (resume, resume_cleanup) = mpsc::channel();
    let (checkpoint, reached_checkpoint) = mpsc::channel();
    let (finish, finish_checkpoint) = mpsc::channel();
    let fixture = fixture_native(
        os,
        move || {
            Ok(ReconnectCleanupGate {
                native: Native { os: native_os },
                initial_idle: Some((initial, start_initial)),
                failed: false,
                error_released: false,
                cleanup_gate: Some((cleanup, resume_cleanup)),
                after_cleanup: false,
                checkpoint: Some((checkpoint, finish_checkpoint)),
            })
        },
        None,
    );
    let down = Command::Key {
        code: 30,
        scan: true,
        extended: false,
        down: true,
    };
    inside_initial.recv().unwrap();
    let client = fixture.client.clone();
    let pressed = thread::spawn(move || client.request(down, false, false));
    assert_eq!(fixture.submitted.recv().unwrap(), down);
    // Avoid relying on startup scheduling for the first connection: the native
    // owner is parked inside maintain and will see gen1 at its next iteration.
    start.send(()).unwrap();
    pressed.join().unwrap().unwrap();
    let failed = Command::Move { x: 99, y: 0 };
    assert_ne!(fixture.client.request(failed, false, false).unwrap().unwrap().0[0], 0);
    inside_cleanup.recv().unwrap();
    let next = Command::Move { x: 1, y: 2 };
    let client = fixture.client.clone();
    let request = thread::spawn(move || client.request(next, false, false));
    assert_eq!(fixture.submitted.recv().unwrap(), failed);
    assert_eq!(fixture.submitted.recv().unwrap(), next);
    // The new generation's request is accepted before old snapshot cleanup ends.
    assert_eq!(*fixture.starts.lock().unwrap(), 2);
    resume.send(()).unwrap();
    reached_checkpoint.recv().unwrap();
    let applied = fixture.os.lock().unwrap().events.contains(&next);
    finish.send(()).unwrap();
    if !applied {
        // Release the stranded ACK waiter on the old code before failing the
        // deterministic assertion; this is test teardown, never a timeout fix.
        fixture.client.stop_for_test();
    }
    let result = request.join().unwrap();
    assert!(
        applied,
        "accepted new-generation input was discarded using a stale generation snapshot"
    );
    assert!(matches!(result, Ok(Some(_))));
    let os = fixture.os.lock().unwrap();
    assert!(os.held.is_empty());
    assert_eq!(os.events.iter().filter(|c| **c == down).count(), 1);
}
#[test]
fn native_fifo_preserves_drag_press_up_and_release_barriers() {
    let os = Arc::new(Mutex::new(Os::default()));
    let native_os = os.clone();
    let (entered, ready) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let fixture = fixture_native(
        os,
        move || {
            Ok(GatedNative {
                native: Native { os: native_os },
                entered,
                gate,
            })
        },
        Some(Instant::now()),
    );
    let commands = [
        Command::DragPress { x: 123, y: 456 },
        Command::Move { x: 1, y: 2 },
        Command::Button { button: 0, down: false },
        Command::Release,
    ];
    let first = fixture.client.clone();
    let down = thread::spawn(move || first.request(commands[0], false, false));
    ready.recv_timeout(Duration::from_secs(3)).unwrap();
    let mut callers = Vec::new();
    for (i, command) in commands[1..].iter().copied().enumerate() {
        let client = fixture.client.clone();
        callers.push(thread::spawn(move || client.request(command, false, false)));
        until(|| fixture.client.input_pending() == i + 1);
    }
    // The down is already applied but its ACK is explicitly blocked. No later
    // input has left the client lane, regardless of how long the gate is held.
    assert_eq!(fixture.os.lock().unwrap().events, commands[..1]);
    release.send(()).unwrap();
    down.join().unwrap().unwrap();
    for caller in callers {
        caller.join().unwrap().unwrap();
    }
    assert_eq!(fixture.os.lock().unwrap().events, commands);
    assert!(fixture.os.lock().unwrap().held.is_empty());
}
struct GatedNative {
    native: Native,
    entered: mpsc::Sender<()>,
    gate: mpsc::Receiver<()>,
}
impl helper::Native for GatedNative {
    fn apply(&mut self, command: Command) -> Reply {
        let reply = self.native.apply(command);
        if matches!(command, Command::DragPress { .. } | Command::Key { down: true, .. }) {
            self.entered.send(()).unwrap();
            self.gate.recv().unwrap();
        }
        reply
    }
    fn release(&mut self) {
        self.native.release();
    }
    fn maintain(&mut self) {
        self.native.maintain();
    }
}
fn until(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !predicate() {
        assert!(Instant::now() < deadline, "test handshake watchdog");
        thread::yield_now();
    }
}
#[test]
fn disconnect_and_restart_release_before_new_input_and_discard_stale_work() {
    let os = Arc::new(Mutex::new(Os::default()));
    let native_os = os.clone();
    let (entered, ready) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let (notice, notices) = mpsc::channel();
    let (query_notice, _) = mpsc::channel();
    let (_, query_gate) = mpsc::channel();
    let mut server = helper::Server::new(
        move || {
            Ok(GatedNative {
                native: Native { os: native_os },
                entered,
                gate,
            })
        },
        move || Query {
            entered: query_notice,
            gate: query_gate,
        },
        Arc::new(move || {
            let _ = notice.send(());
        }),
    )
    .unwrap();
    let down = Command::Key {
        code: 30,
        scan: true,
        extended: false,
        down: true,
    };
    let generation = server.begin();
    let work = |generation, id, command| helper::Work {
        generation,
        request: Request {
            id,
            ordinary: false,
            command,
        },
    };
    server.submit(work(generation, 1, down)).unwrap();
    ready.recv_timeout(Duration::from_secs(3)).unwrap();
    server
        .submit(work(generation, 2, Command::Move { x: 7, y: 8 }))
        .unwrap();
    server.disconnect();
    let next = server.begin();
    let new = Command::Move { x: 3, y: 4 };
    server.submit(work(next, 3, new)).unwrap();
    release.send(()).unwrap();
    notices.recv_timeout(Duration::from_secs(3)).unwrap();
    let reply = server.receive().unwrap();
    assert_eq!((reply.generation, reply.reply.id()), (next, 3));
    let up = Command::Key {
        code: 30,
        scan: true,
        extended: false,
        down: false,
    };
    assert_eq!(os.lock().unwrap().events, [down, up, new]);
    assert!(os.lock().unwrap().held.is_empty());
    server.stop_native();
    // A controlled helper restart constructs a new native owner after the old
    // stop has released its ledger. Neither a down nor an old ACK is replayed.
    let native_os = os.clone();
    let mut restarted =
        helper::Server::new(move || Ok(Native { os: native_os }), || IdleQuery, Arc::new(|| {})).unwrap();
    let generation = restarted.begin();
    restarted.submit(work(generation, 1, new)).unwrap();
    until(|| os.lock().unwrap().events.len() == 4);
    restarted.stop_native();
    assert_eq!(os.lock().unwrap().events, [down, up, new, new]);
}
struct IdleQuery;
impl helper::Query for IdleQuery {
    fn query(&mut self, _: Command) -> Reply {
        Reply([0; REPLY_SIZE])
    }
}
#[test]
fn controlled_stop_releases_keys_without_waiting_for_blocked_ordinary_query() {
    let os = Arc::new(Mutex::new(Os::default()));
    let native_os = os.clone();
    let (entered, ready) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let (notice, notices) = mpsc::channel();
    let mut server = helper::Server::new(
        move || Ok(Native { os: native_os }),
        move || Query { entered, gate },
        Arc::new(move || {
            let _ = notice.send(());
        }),
    )
    .unwrap();
    let generation = server.begin();
    server
        .submit(helper::Work {
            generation,
            request: Request {
                id: 1,
                ordinary: false,
                command: Command::Button { button: 0, down: true },
            },
        })
        .unwrap();
    notices.recv_timeout(Duration::from_secs(3)).unwrap();
    assert_eq!(server.receive().unwrap().reply.id(), 1);
    server
        .submit(helper::Work {
            generation,
            request: Request {
                id: 2,
                ordinary: true,
                command: Command::Ping,
            },
        })
        .unwrap();
    ready.recv_timeout(Duration::from_secs(3)).unwrap();
    let (done, stopped) = mpsc::channel();
    let worker = thread::spawn(move || {
        server.stop_native();
        done.send(()).unwrap();
    });
    let before_release = stopped.recv_timeout(Duration::from_secs(3));
    let held = os.lock().unwrap().held.clone();
    release.send(()).unwrap();
    worker.join().unwrap();
    assert!(before_release.is_ok(), "ordinary query blocked native stop");
    assert!(held.is_empty(), "stop completed before releasing held input");
}
#[test]
fn input_timing_excludes_blocked_ordinary_work() {
    use oink_input::timing::{CallClass, InjectionTiming};
    let fixture = fixture();
    let query_client = fixture.client.clone();
    let query = thread::spawn(move || query_client.request(Command::Ping, true, false));
    fixture.entered.recv_timeout(Duration::from_secs(3)).unwrap();
    let timing = InjectionTiming::new(2);
    let at = Instant::now();
    let pending = timing.pending(at);
    timing.native_begin(CallClass::Motion, at);
    let result = fixture.client.request(Command::Move { x: 1, y: 2 }, false, false);
    fixture.release.send(()).unwrap();
    let _ = query.join().unwrap();
    result.unwrap();
    timing.native_end(CallClass::Motion, 60);
    drop(pending);
    // Explicit test clock; no native latency or performance claim.
    let mut sample = timing.take(at + Duration::from_secs(1));
    sample.window_us = 1_000_000;
    assert_eq!(
        (sample.pending, sample.oldest_pending_us, sample.native_inflight_age_us),
        (0, 0, 0)
    );
    assert_eq!(sample.inject_native_max_us[0], 60);
}

#[test]
fn pending_ordinary_write_cannot_delay_an_arrived_input_ack() {
    use std::sync::atomic::AtomicBool;
    struct State {
        input_ready: AtomicBool,
        ordinary_ready: AtomicBool,
        wake: owner::Wake,
    }
    struct PendingWrite {
        state: Arc<State>,
        sent: mpsc::Sender<bool>,
        input: Option<u32>,
        ordinary: Option<u32>,
        ordinary_written: bool,
        origin: Instant,
    }
    impl owner::Transport for PendingWrite {
        fn now(&self) -> Instant {
            self.origin
        }
        fn send(&mut self, request: Request) -> io::Result<bool> {
            if request.ordinary {
                self.ordinary = Some(request.id);
            } else {
                self.input = Some(request.id);
            }
            self.sent.send(request.ordinary).unwrap();
            Ok(!request.ordinary)
        }
        fn written(&mut self) -> io::Result<Option<u32>> {
            if self.state.ordinary_ready.load(Ordering::Acquire) && !self.ordinary_written {
                self.ordinary_written = true;
                return Ok(self.ordinary);
            }
            Ok(None)
        }
        fn receive(&mut self) -> io::Result<Option<Reply>> {
            if self.state.input_ready.load(Ordering::Acquire)
                && let Some(id) = self.input.take()
            {
                return Ok(Some(Reply([0; REPLY_SIZE]).with_id(id)));
            }
            if self.ordinary_written
                && let Some(id) = self.ordinary.take()
            {
                return Ok(Some(Reply([0; REPLY_SIZE]).with_id(id)));
            }
            Ok(None)
        }
        fn wait(&mut self, timeout: Duration) -> io::Result<()> {
            thread::park_timeout(timeout);
            Ok(())
        }
        fn connected(&self) -> bool {
            true
        }
        fn disconnect(&mut self) {
            self.input = None;
            self.ordinary = None;
        }
    }
    let worker: Arc<OnceLock<thread::Thread>> = Arc::default();
    let notify = worker.clone();
    let wake: owner::Wake = Arc::new(move || {
        if let Some(thread) = notify.get() {
            thread.unpark();
        }
    });
    let state = Arc::new(State {
        input_ready: AtomicBool::new(false),
        ordinary_ready: AtomicBool::new(false),
        wake: wake.clone(),
    });
    let (sent, observed) = mpsc::channel();
    let client = Arc::new(
        owner::Client::new(
            PendingWrite {
                state: state.clone(),
                sent,
                input: None,
                ordinary: None,
                ordinary_written: false,
                origin: Instant::now(),
            },
            wake,
            move || {
                worker.set(thread::current()).unwrap();
                Ok(())
            },
        )
        .unwrap(),
    );
    let input = client.clone();
    let (done, acked) = mpsc::channel();
    let input = thread::spawn(move || {
        done.send(input.request(Command::Move { x: 1, y: 2 }, false, false))
            .unwrap();
    });
    assert!(!observed.recv_timeout(Duration::from_secs(3)).unwrap());
    let ordinary = client.clone();
    let ordinary = thread::spawn(move || ordinary.request(Command::Ping, true, false));
    assert!(observed.recv_timeout(Duration::from_secs(3)).unwrap());
    state.input_ready.store(true, Ordering::Release);
    (state.wake)();
    let before_write_completes = acked.recv_timeout(Duration::from_secs(3));
    state.ordinary_ready.store(true, Ordering::Release);
    (state.wake)();
    input.join().unwrap();
    ordinary.join().unwrap().unwrap();
    assert!(
        matches!(before_write_completes, Ok(Ok(Some(_)))),
        "ordinary write delayed an arrived native ACK"
    );
}

#[test]
fn native_ack_can_precede_local_write_notification_without_replay_or_disconnect() {
    struct EarlyAck {
        writes: std::collections::VecDeque<(u32, bool, bool)>,
        disconnects: Arc<AtomicU64>,
    }
    impl owner::Transport for EarlyAck {
        fn send(&mut self, request: Request) -> io::Result<bool> {
            self.writes.push_back((request.id, false, false));
            Ok(false)
        }
        fn written(&mut self) -> io::Result<Option<u32>> {
            if let Some((id, _, notified)) = self.writes.iter_mut().find(|(_, acked, notified)| *acked && !*notified) {
                *notified = true;
                return Ok(Some(*id));
            }
            Ok(None)
        }
        fn receive(&mut self) -> io::Result<Option<Reply>> {
            if let Some((id, acked, _)) = self.writes.iter_mut().find(|(_, acked, _)| !*acked) {
                *acked = true;
                return Ok(Some(Reply([0; REPLY_SIZE]).with_id(*id)));
            }
            Ok(None)
        }
        fn wait(&mut self, timeout: Duration) -> io::Result<()> {
            if !self.writes.iter().any(|(_, acked, notified)| *acked && !*notified) {
                thread::park_timeout(timeout);
            }
            Ok(())
        }
        fn connected(&self) -> bool {
            true
        }
        fn disconnect(&mut self) {
            self.disconnects.fetch_add(1, Ordering::Relaxed);
        }
    }
    let worker: Arc<OnceLock<thread::Thread>> = Arc::default();
    let notify = worker.clone();
    let disconnects = Arc::new(AtomicU64::new(0));
    let client = owner::Client::new(
        EarlyAck {
            writes: Default::default(),
            disconnects: disconnects.clone(),
        },
        Arc::new(move || {
            if let Some(thread) = notify.get() {
                thread.unpark();
            }
        }),
        move || {
            worker.set(thread::current()).unwrap();
            Ok(())
        },
    )
    .unwrap();
    for command in [
        Command::Button { button: 0, down: true },
        Command::Button { button: 0, down: false },
    ] {
        client.request(command, false, false).unwrap();
    }
    assert_eq!(disconnects.load(Ordering::Relaxed), 0);
}
