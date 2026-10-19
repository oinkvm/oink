//! One outgoing Low budget per interactive peer link, above the TCP/TLS adapter.
//! The feedback is an authenticated Ping/Pong on the interactive path, including
//! when bulk later uses another interface. A blocked transport never owns the
//! scheduler's turn or borrows future tokens.

use crate::record::RecordWrite;
use std::{
    collections::{HashMap, VecDeque},
    future::poll_fn,
    io,
    pin::Pin,
    sync::{Arc, Mutex, MutexGuard},
    task::{Context, Poll, Waker},
    time::Duration,
};
use tokio::{
    sync::Notify,
    time::{Instant, Sleep},
};

// The feedback moved from the bulk QUIC RTT to the EF interactive RTT. Keeping
// the old 50 ms allowance let that interactive path itself accumulate a large
// queue. The bounded 10/20/30 ms two-Mac comparison selected 10 ms for input tails.
// This is extra RTT above the rolling minimum, not a
// hard end-to-end latency ceiling (each relay hop has its own controller).
pub(crate) const TARGET: Duration = Duration::from_millis(10);
pub(crate) const PERIOD: Duration = Duration::from_millis(50);
pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_millis(500);
pub(crate) const QUANTUM: usize = 16 * 1024;
pub(crate) const MIN_RATE: f64 = 256.0 * 1024.0;
const MAX_RATE: f64 = 1024.0 * 1024.0 * 1024.0;
const START_RATE: f64 = 2.0 * 1024.0 * 1024.0;
const BURST: f64 = 256.0 * 1024.0;
const BASE_WINDOW: Duration = Duration::from_secs(60);

fn target() -> Duration {
    TARGET
}

#[derive(Debug)]
struct Waiter {
    waker: Waker,
    queued: bool,
}
#[derive(Debug)]
struct State {
    rate: f64,
    tokens: f64,
    held: usize,
    last_fill: Instant,
    last_adjust: Instant,
    base_prev: Option<Duration>,
    base_cur: Option<(Duration, Instant)>,
    observed: Option<(Duration, Instant)>,
    timed_out: bool,
    busy: bool,
    limited: bool,
    closed: bool,
    next: u64,
    waiters: HashMap<u64, Waiter>,
    ready: VecDeque<u64>,
}
impl State {
    fn fill(&mut self, now: Instant) {
        self.tokens =
            (self.tokens + now.duration_since(self.last_fill).as_secs_f64() * self.rate).min(BURST - self.held as f64);
        self.last_fill = now;
    }
    fn wake_front(&self) {
        if let Some(w) = self.ready.front().and_then(|id| self.waiters.get(id)) {
            w.waker.wake_by_ref();
        }
    }
}

#[derive(Debug)]
pub(crate) struct Pacer {
    st: Mutex<State>,
    pub activity: Arc<crate::activity::Activity>,
    /// Wake the peer's otherwise-idle keepalive task when Low work arrives.
    pub changed: Notify,
    telemetry: Mutex<Telemetry>,
}
impl Pacer {
    pub(crate) fn new(activity: Arc<crate::activity::Activity>) -> Self {
        let now = Instant::now();
        Self {
            st: Mutex::new(State {
                rate: START_RATE,
                tokens: BURST,
                held: 0,
                last_fill: now,
                last_adjust: now,
                base_prev: None,
                base_cur: None,
                observed: None,
                timed_out: false,
                busy: false,
                limited: false,
                closed: false,
                next: 0,
                waiters: HashMap::new(),
                ready: VecDeque::new(),
            }),
            changed: Notify::new(),
            telemetry: Mutex::new(Telemetry::default()),
            activity,
        }
    }
    /// Called by the existing ordinary pool worker, never from activity callbacks
    /// or pacing write grants. Read-only: does not refresh/adjust controller state.
    pub(crate) fn report(&self) {
        let now = Instant::now();
        let activity = self.activity.status();
        let (rate, active, timeout, rtt) = {
            let st = self.state();
            (
                st.rate,
                !st.waiters.is_empty(),
                st.timed_out,
                st.observed.map(|(rtt, _)| rtt),
            )
        };
        let mut telemetry = self.telemetry.lock().unwrap_or_else(|e| e.into_inner());
        telemetry.report(now, activity, rate, active, timeout, rtt);
    }
    fn state(&self) -> MutexGuard<'_, State> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }
    pub(crate) fn active(&self) -> bool {
        !self.state().waiters.is_empty()
    }
    fn refresh_activity(&self, st: &mut State, now: Instant) -> crate::activity::Status {
        let activity = self.activity.status();
        if activity.active() && !st.limited {
            // Idle throughput never becomes a controller rate. Start below the
            // previous working point and drain queues already filled by TCP.
            st.rate = st.rate.min(START_RATE);
            st.tokens = 0.0;
            st.last_fill = now;
            st.last_adjust = now;
            st.busy = false;
        }
        st.limited = activity.active();
        activity
    }
    pub(crate) fn activity_changed(&self) {
        let mut st = self.state();
        self.refresh_activity(&mut st, Instant::now());
        for waiter in st.waiters.values() {
            waiter.waker.wake_by_ref();
        }
    }
    pub(crate) fn snapshot(&self) -> crate::LowPacingStatus {
        let mut st = self.state();
        let activity = self.refresh_activity(&mut st, Instant::now());
        crate::LowPacingStatus {
            limited: st.limited,
            local_activity: activity.local,
            group_activity: activity.group,
            audio_activity: activity.audio,
            rate_bytes_per_second: st.rate as u64,
            active_streams: st.waiters.len(),
            rtt: st.observed.map(|(rtt, _)| rtt),
            observation_stale: st.timed_out || st.observed.is_none_or(|(_, at)| at.elapsed() >= PROBE_TIMEOUT),
            target: target(),
        }
    }
    /// Ordinary transfer observation without refreshing the controller or budget.
    pub(crate) fn observation(&self) -> crate::LowPacingStatus {
        let st = self.state();
        let activity = self.activity.status();
        crate::LowPacingStatus {
            limited: activity.active(),
            local_activity: activity.local,
            group_activity: activity.group,
            audio_activity: activity.audio,
            rate_bytes_per_second: st.rate as u64,
            active_streams: st.waiters.len(),
            rtt: st.observed.map(|(rtt, _)| rtt),
            observation_stale: st.timed_out || st.observed.is_none_or(|(_, at)| at.elapsed() >= PROBE_TIMEOUT),
            target: target(),
        }
    }
    pub(crate) fn observe(&self, rtt: Duration) {
        let now = Instant::now();
        let mut st = self.state();
        if st.closed {
            return;
        }
        self.refresh_activity(&mut st, now);
        let first = st.observed.is_none();
        st.observed = Some((rtt, now));
        st.timed_out = false;
        if first || now.duration_since(st.last_adjust) >= PERIOD {
            st.fill(now);
            st.last_adjust = now;
            st.base_cur = match st.base_cur {
                Some((b, t)) if now.duration_since(t) < BASE_WINDOW => Some((b.min(rtt), t)),
                Some((b, _)) => {
                    st.base_prev = Some(b);
                    Some((rtt, now))
                }
                None => Some((rtt, now)),
            };
            let cur = st.base_cur.map_or(rtt, |(b, _)| b);
            let base = st.base_prev.map_or(cur, |p| p.min(cur));
            let queued = rtt.saturating_sub(base);
            if st.limited && queued > target() {
                let over = (queued.as_secs_f64() / target().as_secs_f64()).min(4.0);
                st.rate = (st.rate * (1.0 - 0.125 * over)).max(MIN_RATE);
            } else if st.limited && st.busy {
                st.rate = (st.rate * 1.1 + 64.0 * 1024.0).min(MAX_RATE);
            }
            st.busy = false;
        }
        st.wake_front();
        self.changed.notify_one();
    }
    pub(crate) fn probe_timeout(&self) {
        let mut st = self.state();
        self.refresh_activity(&mut st, Instant::now());
        st.fill(Instant::now());
        st.timed_out = true;
        if st.limited {
            st.rate = (st.rate * 0.5).max(MIN_RATE);
        }
        st.busy = false;
        st.wake_front();
    }
    pub(crate) fn close(&self) {
        let mut st = self.state();
        st.closed = true;
        for waiter in st.waiters.values() {
            waiter.waker.wake_by_ref();
        }
        self.changed.notify_one();
    }
}
#[derive(Debug)]
struct Telemetry {
    id: u64,
    state: Option<(bool, bool, bool, bool)>,
    last_sample: Option<Instant>,
    last_rate: Option<(Instant, f64)>,
}
impl Default for Telemetry {
    fn default() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        Self {
            id: NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            state: None,
            last_sample: None,
            last_rate: None,
        }
    }
}
impl Telemetry {
    fn report(
        &mut self,
        now: Instant,
        activity: crate::activity::Status,
        rate: f64,
        active: bool,
        timeout: bool,
        rtt: Option<Duration>,
    ) {
        let state = (activity.local, activity.group, activity.audio, timeout);
        if self.state != Some(state) {
            tracing::info!(target: "oink_pacing_diagnostics", kind="pacing_state", pacing_id=self.id, limited=activity.active(), local_input=activity.local, group_input=activity.group, audio=activity.audio, probe_timeout=timeout, "Low pacing state");
            self.state = Some(state);
        }
        if active
            && self
                .last_sample
                .is_none_or(|at| now.duration_since(at) >= Duration::from_secs(1))
        {
            tracing::info!(target: "oink_pacing_diagnostics", kind="pacing_sample", pacing_id=self.id, limited=activity.active(), rtt_ms=rtt.map(|r| r.as_secs_f64()*1000.0), target_ms=target().as_secs_f64()*1000.0, probe_timeout=timeout, "Low pacing observation");
            self.last_sample = Some(now);
        }
        // Material = at least 10% from the last logged value; retain pending
        // changes until the one-second ceiling allows the latest value to log.
        if self.last_rate.is_none_or(|(at, previous)| {
            now.duration_since(at) >= Duration::from_secs(1) && (rate - previous).abs() >= previous * 0.1
        }) {
            tracing::debug!(target: "oink_local_transfer", pacing_id=self.id, rate_bytes_per_second=rate, limited=activity.active(), "Low pacing rate changed");
            self.last_rate = Some((now, rate));
        }
    }
}
fn closed() -> io::Error {
    io::Error::new(io::ErrorKind::ConnectionAborted, "Low pacing link retired")
}

/// A single stream's pending write. There is at most one entry per sender;
/// transport-blocked entries are retained for cancellation, but not in ready.
struct Turn {
    pacer: Arc<Pacer>,
    id: Option<u64>,
    timer: Option<Pin<Box<Sleep>>>,
}
impl Turn {
    fn new(pacer: Arc<Pacer>) -> Self {
        Self {
            pacer,
            id: None,
            timer: None,
        }
    }
    fn register(&mut self, cx: &Context<'_>) -> io::Result<()> {
        let mut st = self.pacer.state();
        if st.closed {
            return Err(closed());
        }
        if let Some(id) = self.id {
            st.waiters.get_mut(&id).expect("live turn").waker.clone_from(cx.waker());
        } else {
            let id = st.next;
            st.next = st.next.checked_add(1).expect("Low turn identifier exhausted");
            st.waiters.insert(
                id,
                Waiter {
                    waker: cx.waker().clone(),
                    queued: false,
                },
            );
            self.id = Some(id);
            self.pacer.changed.notify_one();
        }
        Ok(())
    }
    fn poll_grant(&mut self, cx: &mut Context<'_>, len: usize) -> Poll<io::Result<Permit>> {
        self.register(cx)?;
        let id = self.id.expect("registered");
        let mut st = self.pacer.state();
        if !st.waiters[&id].queued {
            st.waiters.get_mut(&id).expect("live turn").queued = true;
            st.ready.push_back(id);
        }
        let now = Instant::now();
        let activity = self.pacer.refresh_activity(&mut st, now);
        if st.ready.front() != Some(&id)
            || (st.limited && st.observed.is_none_or(|(_, at)| at.elapsed() >= PROBE_TIMEOUT) && !st.timed_out)
        {
            return Poll::Pending;
        }
        st.fill(now);
        if st.limited && st.tokens < len as f64 {
            let deadline =
                now + Duration::from_secs_f64((len as f64 - st.tokens) / st.rate).max(Duration::from_micros(1));
            let deadline = activity.expires.map_or(deadline, |expiry| expiry.min(deadline));
            drop(st);
            let timer = self
                .timer
                .get_or_insert_with(|| Box::pin(tokio::time::sleep_until(deadline)));
            timer.as_mut().reset(deadline);
            let _ = std::future::Future::poll(timer.as_mut(), cx);
            return Poll::Pending;
        }
        let charged = st.limited;
        if charged {
            st.tokens -= len as f64;
            st.held += len;
        }
        st.ready.pop_front();
        st.waiters.get_mut(&id).expect("live turn").queued = false;
        st.wake_front();
        Poll::Ready(Ok(Permit {
            pacer: self.pacer.clone(),
            len,
            used: 0,
            charged,
        }))
    }
    fn poll_write<S: RecordWrite>(
        &mut self,
        cx: &mut Context<'_>,
        stream: &mut S,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        let mut permit = std::task::ready!(self.poll_grant(cx, data.len()))?;
        match stream.poll_write_bytes(cx, data) {
            Poll::Ready(Ok(0)) => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "Low write made no progress",
            ))),
            Poll::Ready(Ok(n)) => {
                permit.used = n;
                Poll::Ready(Ok(n))
            }
            other => other,
        }
        // Pending: refund all tokens and wait only for the transport waker.
        // The next poll joins the tail rather than holding up other streams.
    }
    fn poll_flush<S: RecordWrite>(&mut self, cx: &mut Context<'_>, stream: &mut S) -> Poll<io::Result<()>> {
        self.register(cx)?;
        stream.poll_flush_bytes(cx)
    }
    async fn write_all<S: RecordWrite>(&mut self, stream: &mut S, data: &[u8]) -> io::Result<()> {
        for mut chunk in data.chunks(QUANTUM) {
            while !chunk.is_empty() {
                let n = poll_fn(|cx| self.poll_write(cx, stream, chunk)).await?;
                chunk = &chunk[n..];
            }
            // A TLS buffer must not collect grants and flush them as a later
            // multi-stream burst. QUIC's adapter has no extra plaintext buffer.
            poll_fn(|cx| self.poll_flush(cx, stream)).await?;
            tokio::task::yield_now().await;
        }
        Ok(())
    }
}
impl Drop for Turn {
    fn drop(&mut self) {
        if let Some(id) = self.id {
            let mut st = self.pacer.state();
            st.waiters.remove(&id);
            st.ready.retain(|i| *i != id);
            st.wake_front();
        }
    }
}
struct Permit {
    pacer: Arc<Pacer>,
    len: usize,
    used: usize,
    charged: bool,
}
impl Drop for Permit {
    fn drop(&mut self) {
        let mut st = self.pacer.state();
        if self.charged {
            st.held -= self.len;
            st.tokens = (st.tokens + (self.len - self.used) as f64).min(BURST - st.held as f64);
            st.busy |= self.used > 0;
        }
        st.wake_front();
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Paced {
    pub pacer: Arc<Pacer>,
}
impl Paced {
    pub(crate) async fn write_all<S: RecordWrite>(&self, stream: &mut S, data: &[u8]) -> io::Result<()> {
        Turn::new(self.pacer.clone()).write_all(stream, data).await
    }
    pub(crate) async fn record<S: RecordWrite>(&self, stream: &mut S, header: &[u8], body: &[u8]) -> io::Result<()> {
        // Coalesce the small header with the first body slice without changing
        // record boundaries or allocating another whole Message.
        let mut first = [0; QUANTUM];
        let count = body.len().min(QUANTUM - header.len());
        first[..header.len()].copy_from_slice(header);
        first[header.len()..header.len() + count].copy_from_slice(&body[..count]);
        let mut turn = Turn::new(self.pacer.clone());
        turn.write_all(stream, &first[..header.len() + count]).await?;
        turn.write_all(stream, &body[count..]).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test(start_paused = true)]
    async fn telemetry_is_read_only_and_coalesces_material_rates_and_samples() {
        let p = active_pacer();
        p.observe(Duration::from_millis(2));
        let before = {
            let st = p.state();
            (st.rate, st.tokens, st.last_adjust)
        };
        p.report();
        {
            let st = p.state();
            assert_eq!((st.rate, st.tokens, st.last_adjust), before);
        }
        let start = Instant::now();
        let mut t = Telemetry::default();
        let activity = p.activity.status();
        t.report(start, activity, START_RATE, true, false, Some(Duration::from_millis(2)));
        assert_eq!(t.last_rate, Some((start, START_RATE)));
        assert_eq!(t.last_sample, Some(start));
        for millis in [50, 100, 500, 999] {
            t.report(
                start + Duration::from_millis(millis),
                activity,
                START_RATE * 2.0,
                true,
                false,
                None,
            );
            assert_eq!(t.last_rate, Some((start, START_RATE)));
            assert_eq!(t.last_sample, Some(start));
        }
        let next = start + Duration::from_secs(1);
        t.report(next, activity, START_RATE * 2.0, true, true, None);
        assert_eq!(t.last_rate, Some((next, START_RATE * 2.0)));
        assert_eq!(t.last_sample, Some(next));
        assert_eq!(t.state, Some((true, false, false, true)));
        t.report(
            next + Duration::from_secs(1),
            activity,
            START_RATE * 2.05,
            false,
            false,
            None,
        );
        assert_eq!(
            t.last_rate,
            Some((next, START_RATE * 2.0)),
            "small changes are not logged"
        );
        assert_eq!(t.last_sample, Some(next), "idle has no periodic sampling");
        assert_eq!(t.state, Some((true, false, false, false)));
    }
    #[derive(Default)]
    struct Writer {
        bytes: usize,
        flushes: usize,
        block_write: bool,
        block_flush: bool,
        partial: bool,
    }
    impl RecordWrite for Writer {
        async fn record(&mut self, _: &[u8], _: &[u8]) -> io::Result<()> {
            unreachable!()
        }
        async fn flush_message(&mut self) -> io::Result<()> {
            unreachable!()
        }
        fn poll_write_bytes(&mut self, _: &mut Context<'_>, data: &[u8]) -> Poll<io::Result<usize>> {
            if self.block_write {
                return Poll::Pending;
            }
            let n = if self.partial { data.len().min(257) } else { data.len() };
            self.bytes += n;
            Poll::Ready(Ok(n))
        }
        fn poll_flush_bytes(&mut self, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            if self.block_flush {
                return Poll::Pending;
            }
            self.flushes += 1;
            Poll::Ready(Ok(()))
        }
        async fn finish_records(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    fn active_pacer() -> Arc<Pacer> {
        let activity = Arc::new(crate::activity::Activity::default());
        activity.note(crate::activity::Kind::Local);
        Arc::new(Pacer::new(activity))
    }
    fn ready() -> Arc<Pacer> {
        let p = active_pacer();
        p.observe(Duration::from_millis(2));
        p.state().tokens = BURST;
        p
    }

    #[tokio::test(start_paused = true)]
    async fn idle_has_no_budget_or_probe_gate_and_activity_reenters_conservatively() {
        let activity = Arc::new(crate::activity::Activity::default());
        let p = Arc::new(Pacer::new(activity.clone()));
        let mut writer = Writer::default();
        let start = Instant::now();
        Paced { pacer: p.clone() }
            .write_all(&mut writer, &vec![0; 4 * 1024 * 1024])
            .await
            .unwrap();
        assert_eq!(
            start.elapsed(),
            Duration::ZERO,
            "idle cannot wait for tokens or a probe"
        );
        assert!(!p.snapshot().limited);
        p.state().rate = 32.0 * 1024.0 * 1024.0;
        activity.note(crate::activity::Kind::Local);
        let status = p.snapshot();
        assert!(status.limited && status.local_activity && !status.group_activity && !status.audio_activity);
        assert_eq!(status.rate_bytes_per_second, START_RATE as u64);
        assert_eq!(p.state().tokens, 0.0);
        p.observe(Duration::from_millis(40));
        let mut turn = Turn::new(p.clone());
        assert!(
            turn.poll_grant(&mut Context::from_waker(Waker::noop()), QUANTUM)
                .is_pending()
        );
        tokio::time::advance(crate::activity::HOLD).await;
        assert!(!p.snapshot().limited);
        assert!(matches!(
            turn.poll_grant(&mut Context::from_waker(Waker::noop()), QUANTUM),
            Poll::Ready(Ok(_))
        ));
        p.state().rate = MIN_RATE;
        activity.note(crate::activity::Kind::Local);
        assert_eq!(
            p.snapshot().rate_bytes_per_second,
            MIN_RATE as u64,
            "flapping never grants a higher rate"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn four_streams_share_rate_and_do_not_get_new_bursts() {
        let p = ready();
        {
            let mut st = p.state();
            st.tokens = 0.0;
            st.rate = 1024.0 * 1024.0;
            st.timed_out = true;
        }
        let start = Instant::now();
        let mut tasks = Vec::new();
        for _ in 0..4 {
            let paced = Paced { pacer: p.clone() };
            tasks.push(tokio::spawn(async move {
                let mut writer = Writer::default();
                paced.write_all(&mut writer, &vec![0; 128 * 1024]).await.unwrap();
                assert_eq!(writer.flushes, 8);
                writer.bytes
            }));
        }
        let mut bytes = 0;
        for task in tasks {
            bytes += task.await.unwrap();
        }
        assert_eq!(bytes, 512 * 1024);
        assert!(start.elapsed() >= Duration::from_millis(500));
        assert!(start.elapsed() < Duration::from_millis(520));
        assert!(!p.active());
        // A new stream has no independent initial burst.
        let mut turn = Turn::new(p.clone());
        let mut cx = Context::from_waker(Waker::noop());
        assert!(turn.poll_grant(&mut cx, QUANTUM).is_pending());
    }

    #[tokio::test(start_paused = true)]
    async fn ready_streams_rotate_and_blocked_transport_refunds_immediately() {
        let p = ready();
        p.state().tokens = 0.0;
        let mut turns: Vec<_> = (0..4).map(|_| Turn::new(p.clone())).collect();
        let mut cx = Context::from_waker(Waker::noop());
        let mut writer = Writer::default();
        for turn in &mut turns {
            assert!(turn.poll_write(&mut cx, &mut writer, &[0; QUANTUM]).is_pending());
        }
        for expected in 0..16 {
            p.state().tokens = QUANTUM as f64;
            // Poll in adversarial order. Only the queue's head can spend.
            for offset in 1..4 {
                assert!(
                    turns[(expected + offset) % 4]
                        .poll_write(&mut cx, &mut writer, &[0; QUANTUM])
                        .is_pending()
                );
            }
            assert!(matches!(
                turns[expected % 4].poll_write(&mut cx, &mut writer, &[0; QUANTUM]),
                Poll::Ready(Ok(QUANTUM))
            ));
            assert!(
                turns[expected % 4]
                    .poll_write(&mut cx, &mut writer, &[0; QUANTUM])
                    .is_pending()
            );
        }
        drop(turns);
        p.state().tokens = QUANTUM as f64;
        let mut blocked = Turn::new(p.clone());
        let mut blocked_writer = Writer {
            block_write: true,
            ..Writer::default()
        };
        assert!(
            blocked
                .poll_write(&mut cx, &mut blocked_writer, &[0; QUANTUM])
                .is_pending()
        );
        assert_eq!(p.state().tokens, QUANTUM as f64);
        assert!(p.state().ready.is_empty());
        let mut fast = Turn::new(p.clone());
        assert!(matches!(
            fast.poll_write(&mut cx, &mut writer, &[0; QUANTUM]),
            Poll::Ready(Ok(QUANTUM))
        ));
        drop(blocked);
        drop(fast);
        assert!(!p.active());
    }

    #[tokio::test(start_paused = true)]
    async fn blocked_flush_does_not_hold_budget_or_turn_and_close_wakes_it() {
        let p = ready();
        let task_p = p.clone();
        let slow = tokio::spawn(async move {
            let mut writer = Writer {
                block_flush: true,
                ..Writer::default()
            };
            Paced { pacer: task_p }.write_all(&mut writer, &[0; QUANTUM]).await
        });
        tokio::task::yield_now().await;
        assert_eq!(p.snapshot().active_streams, 1);
        let mut writer = Writer::default();
        Paced { pacer: p.clone() }
            .write_all(&mut writer, &[0; QUANTUM])
            .await
            .unwrap();
        assert_eq!(writer.bytes, QUANTUM);
        assert_eq!(p.state().held, 0);
        p.close();
        assert!(slow.await.unwrap().is_err());
        assert!(!p.active());
    }

    #[tokio::test(start_paused = true)]
    async fn partial_writes_charge_only_accepted_bytes_and_cancellation_unqueues() {
        let p = ready();
        let mut writer = Writer {
            partial: true,
            ..Writer::default()
        };
        Paced { pacer: p.clone() }
            .record(&mut writer, &[0; 32], &[0; 65536])
            .await
            .unwrap();
        assert_eq!(writer.bytes, 65568);
        assert_eq!(writer.flushes, 5);
        assert_eq!(p.state().tokens, BURST - 65568.0);
        p.state().tokens = 0.0;
        let mut turn = Turn::new(p.clone());
        assert!(
            turn.poll_grant(&mut Context::from_waker(Waker::noop()), QUANTUM)
                .is_pending()
        );
        drop(turn);
        assert!(p.state().ready.is_empty());
        assert_eq!(p.state().held, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn stale_feedback_gates_then_timeout_backs_off_and_idle_does_not_grow() {
        let p = active_pacer();
        let mut turn = Turn::new(p.clone());
        let mut cx = Context::from_waker(Waker::noop());
        assert!(turn.poll_grant(&mut cx, QUANTUM).is_pending());
        p.observe(Duration::from_millis(2));
        let rate = p.snapshot().rate_bytes_per_second;
        tokio::time::advance(PERIOD).await;
        p.observe(Duration::from_millis(2));
        assert_eq!(p.snapshot().rate_bytes_per_second, rate);
        p.state().busy = true;
        tokio::time::advance(PERIOD).await;
        p.observe(Duration::from_millis(2));
        assert!(p.snapshot().rate_bytes_per_second > rate);
        tokio::time::advance(PERIOD).await;
        p.observe(Duration::from_millis(202));
        assert!(p.snapshot().rate_bytes_per_second < rate);
        tokio::time::advance(PROBE_TIMEOUT).await;
        assert!(p.snapshot().observation_stale);
        assert!(turn.poll_grant(&mut cx, QUANTUM).is_pending());
        let rate = p.snapshot().rate_bytes_per_second;
        p.probe_timeout();
        assert!(p.snapshot().rate_bytes_per_second <= rate);
        assert!(matches!(turn.poll_grant(&mut cx, QUANTUM), Poll::Ready(Ok(_))));
        p.close();
        assert!(matches!(turn.poll_grant(&mut cx, QUANTUM), Poll::Ready(Err(_))));
    }
}
