use oink_audio::diagnostics::{Histogram, History, monotonic_us};
use std::time::{Duration, Instant};

type ReceiveEvent = (u64, Option<i64>, i64, i64, Option<u64>, bool, Option<i64>, Option<i64>);
type LateEvent = (u64, i64, i64, Option<i64>, Option<i64>);
#[derive(Default)]
pub(super) struct ReceiveEvents {
    history: History<ReceiveEvent, 16>,
    previous: Option<Instant>,
    previous_emit: Option<u64>,
    report_started: Option<Instant>,
    late: Histogram,
    late_events: History<LateEvent, 64>,
    late_count: u64,
    late_unknown: u64,
    dispatch: Histogram,
    insertion: Histogram,
}
impl ReceiveEvents {
    #[allow(clippy::too_many_arguments)]
    pub fn packet(
        &mut self,
        peer: oink_foundation::DeviceId,
        sequence: u64,
        received: Option<Instant>,
        dispatched: Instant,
        inserted: Instant,
        emit: Option<u64>,
        accepted: bool,
        late: Option<oink_audio::dsp::LatePacket>,
    ) {
        let arrival = received.unwrap_or(dispatched);
        let gap = self
            .previous
            .replace(arrival)
            .map(|old| arrival.saturating_duration_since(old))
            .unwrap_or_default();
        let sender_gap = emit.zip(self.previous_emit).map(|(now, old)| now.saturating_sub(old));
        self.previous_emit = emit;
        let consumed = late.and_then(|late| late.consumed_at);
        let late_us = consumed.map(|at| monotonic_us(inserted) - monotonic_us(at));
        let arrival_late_us = consumed.map(|at| monotonic_us(arrival) - monotonic_us(at));
        if late.is_some() {
            self.late_count += 1;
            self.late_events.push((
                sequence,
                monotonic_us(arrival),
                monotonic_us(inserted),
                late_us,
                arrival_late_us,
            ));
        }
        if let Some(at) = consumed {
            self.late.observe(inserted.saturating_duration_since(at));
        }
        if late.is_some() && consumed.is_none() {
            self.late_unknown += 1;
        }
        if let Some(received) = received {
            self.dispatch.observe(dispatched.saturating_duration_since(received));
        }
        self.insertion.observe(inserted.saturating_duration_since(dispatched));
        self.history.push((
            sequence,
            received.map(monotonic_us),
            monotonic_us(dispatched),
            monotonic_us(inserted),
            emit,
            accepted,
            late_us,
            arrival_late_us,
        ));
        if gap >= Duration::from_millis(20)
            || sender_gap.is_some_and(|gap| gap >= 20_000_000)
            || inserted.saturating_duration_since(arrival) >= Duration::from_millis(20)
            || late_us.is_some_and(|late| late >= 20_000)
        {
            let context = self.history;
            super::report::submit(move || {
                tracing::info!(
                    ?peer,
                    sequence,
                    arrival_gap_us = gap.as_micros() as u64,
                    sender_gap_us = sender_gap.map(|ns| ns / 1000),
                    lateness_ms = late_us.map(|us| us as f64 / 1000.0),
                    arrival_lateness_ms = arrival_late_us.map(|us| us as f64 / 1000.0),
                    ?context,
                    "audio receiver gap (seq,quic_read/dispatch/insert us,source_emit_ns,accepted,insertion/arrival_late_us)"
                )
            });
        }
    }
    pub fn summary(&mut self, peer: oink_foundation::DeviceId) -> bool {
        let report_now = Instant::now();
        let report_window = self
            .report_started
            .replace(report_now)
            .map(|at| report_now.saturating_duration_since(at).as_micros() as u64);
        let at = monotonic_us(Instant::now());
        let lateness = self.late.take();
        let late_unknown = std::mem::take(&mut self.late_unknown);
        let late_count = std::mem::take(&mut self.late_count);
        let late_events = (late_count > 0).then_some(std::mem::take(&mut self.late_events));
        let quic_to_dispatch = self.dispatch.take();
        let dispatch_to_insertion = self.insertion.take();
        if let Some(window) = report_window {
            crate::telemetry::projections::audio_histogram(
                oink_telemetry::Metric::AudioReceiveDispatch,
                quic_to_dispatch.buckets,
                window,
            );
            crate::telemetry::projections::audio_histogram(
                oink_telemetry::Metric::AudioDispatchInsert,
                dispatch_to_insertion.buckets,
                window,
            );
        }
        super::report::submit(move || {
            tracing::debug!(
                ?peer,
                monotonic_us = at,
                ?lateness,
                late_unknown,
                late_count,
                late_context_overwritten = late_count.saturating_sub(64),
                ?late_events,
                ?quic_to_dispatch,
                ?dispatch_to_insertion,
                "audio receiver event summary (late: seq,arrival/insert us,insertion/arrival_late_us; since decode/PLC decision, not hardware deadline)"
            )
        });
        quic_to_dispatch.buckets.iter().any(|count| *count > 0)
    }
}
