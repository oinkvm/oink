use oink_audio::diagnostics::{Histogram, History, monotonic_us};
use std::time::{Duration, Instant};

type ReceiveEvent = (u64, Option<i64>, i64, i64, Option<u64>, bool, Option<i64>, Option<i64>);
type LateEvent = (u64, i64, i64, Option<i64>, Option<i64>);
#[derive(Default)]
pub(super) struct ReceiveEvents {
    history: History<ReceiveEvent, 16>,
    gap_reports: GapReports,
    gap_count: u64,
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
// Retain the first incident immediately, then bound expensive context reports.
#[derive(Default)]
struct GapReports {
    last: Option<Instant>,
    suppressed: u64,
}
impl GapReports {
    fn observe(&mut self, now: Instant) -> Option<u64> {
        if self
            .last
            .is_some_and(|last| now.saturating_duration_since(last) < Duration::from_secs(1))
        {
            self.suppressed += 1;
            return None;
        }
        self.last = Some(now);
        Some(std::mem::take(&mut self.suppressed))
    }
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
        let exceptional = gap >= Duration::from_millis(20)
            || sender_gap.is_some_and(|gap| gap >= 20_000_000)
            || inserted.saturating_duration_since(arrival) >= Duration::from_millis(20)
            || late_us.is_some_and(|late| late >= 20_000);
        if exceptional {
            self.gap_count += 1;
        }
        if exceptional && let Some(suppressed) = self.gap_reports.observe(inserted) {
            let context = self.history;
            super::report::submit(move || {
                tracing::info!(
                    ?peer,
                    sequence,
                    suppressed,
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
        let gap_count = std::mem::take(&mut self.gap_count);
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
                gap_count,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "isolated resource benchmark; run with --ignored --nocapture"]
    fn resource_gap_report_benchmark() {
        let origin = Instant::now();
        let mut reports = GapReports::default();
        let history = History::<ReceiveEvent, 16> {
            events: [Some((42, Some(1000), 2000, 3000, Some(4000), true, None, None)); 16],
            next: 0,
        };
        let mut emitted = 0;
        let started = Instant::now();
        for packet in 0..100_000 {
            if reports.observe(origin + Duration::from_millis(packet * 10)).is_some() {
                std::hint::black_box(format!("{history:?}"));
                emitted += 1;
            }
        }
        println!(
            "exceptional_packets=100000 context_reports={emitted} elapsed_us={}",
            started.elapsed().as_micros()
        );
    }

    #[test]
    fn gap_report_storm_is_bounded_without_losing_incident_counts() {
        let now = Instant::now();
        let mut reports = GapReports::default();
        let mut emitted = 0;
        let mut accounted = 0;
        for packet in 0..1000 {
            if let Some(suppressed) = reports.observe(now + Duration::from_millis(packet * 10)) {
                emitted += 1;
                accounted += 1 + suppressed;
            }
        }
        assert_eq!(emitted, 10);
        assert_eq!(accounted + reports.suppressed, 1000);
        assert_eq!(reports.observe(now + Duration::from_secs(10)), Some(99));
    }

    #[test]
    fn isolated_gap_is_reported_immediately_after_quiet() {
        let now = Instant::now();
        let mut reports = GapReports::default();
        assert_eq!(reports.observe(now), Some(0));
        assert_eq!(reports.observe(now + Duration::from_secs(5)), Some(0));
    }
}
