//! Bounded aggregate diagnostics; never record desktop coordinates.
use super::{Event, Recorder, input_trace::InputTrace};
use oink_input::cursor_reconciliation::Decision;
#[derive(Clone, Copy, Default)]
pub(crate) struct Record {
    counts: [u64; 6],
    timeouts: u64,
    late_reads: u64,
    wait_buckets: [u64; 6],
    read_max_us: u64,
    wait_max_us: u64,
}
impl Record {
    pub(super) fn export(self, _: &Recorder) -> Event {
        let mut fields: Vec<_> = ["expected", "rebase", "ambiguous", "grace", "unavailable", "stale"]
            .into_iter()
            .zip(self.counts)
            .map(|(name, count)| (name.into(), count as f64))
            .collect();
        fields.extend([
            ("read_max_us".into(), self.read_max_us as f64),
            ("wait_max_us".into(), self.wait_max_us as f64),
        ]);
        fields.extend([
            ("timeouts".into(), self.timeouts as f64),
            ("late_reads".into(), self.late_reads as f64),
        ]);
        fields.extend(
            [
                "wait_le_250us",
                "wait_le_500us",
                "wait_le_1000us",
                "wait_le_2000us",
                "wait_le_3000us",
                "wait_gt_3000us",
            ]
            .into_iter()
            .zip(self.wait_buckets)
            .map(|(name, count)| (name.into(), count as f64)),
        );
        Event::Log {
            module: "oink_cursor_reconciliation".into(),
            line: 0,
            level: 2,
            fields,
            labels: vec![],
        }
    }
}
#[derive(Default)]
pub(crate) struct State {
    record: Record,
    next: u64,
    dirty: bool,
}
impl State {
    pub(crate) fn late(&mut self, read: u64) {
        self.dirty = true;
        self.record.late_reads = self.record.late_reads.saturating_add(1);
        self.record.read_max_us = self.record.read_max_us.max(read);
    }
    fn wait(&mut self, wait: u64) {
        let bucket = [250, 500, 1000, 2000, 3000]
            .into_iter()
            .position(|limit| wait <= limit)
            .unwrap_or(5);
        self.record.wait_buckets[bucket] = self.record.wait_buckets[bucket].saturating_add(1);
        self.record.wait_max_us = self.record.wait_max_us.max(wait);
    }
    pub(crate) fn timeout(&mut self, wait: u64, now: u64, trace: Option<&InputTrace>) {
        self.dirty = true;
        self.record.timeouts = self.record.timeouts.saturating_add(1);
        self.wait(wait);
        self.emit(now, trace);
    }
    pub(crate) fn emit(&mut self, now: u64, trace: Option<&InputTrace>) {
        if let Some(trace) = trace.filter(|t| t.enabled())
            && self.dirty
            && now >= self.next
        {
            self.dirty = false;
            trace.cursor_reconciliation(self.record);
            self.next = now.saturating_add(1_000_000);
        }
    }
    pub(crate) fn observe(&mut self, decision: Decision, read: u64, wait: u64, now: u64, trace: Option<&InputTrace>) {
        self.dirty = true;
        let index = match decision {
            Decision::Expected => 0,
            Decision::Rebase(_) => 1,
            Decision::Ambiguous(_) => 2,
            Decision::WaitUntil(_) => 3,
            Decision::Unavailable => 4,
            Decision::Stale => 5,
        };
        self.record.counts[index] = self.record.counts[index].saturating_add(1);
        self.record.read_max_us = self.record.read_max_us.max(read);
        if !matches!(decision, Decision::WaitUntil(_) | Decision::Stale) {
            self.wait(wait);
        }
        self.emit(now, trace);
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    #[test]
    fn target_cursor_diagnostics_count_timeouts_and_waits_without_coordinates() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = Recorder::start(dir.path().into(), oink_foundation::DeviceId([1; 32]), None, true).unwrap();
        recorder.wait_recovery();
        let trace = InputTrace::for_recorder(&recorder);
        let mut state = State::default();
        tracing::subscriber::with_default(tracing::subscriber::NoSubscriber::default(), || {
            state.timeout(3000, 0, Some(&trace));
            state.timeout(3400, 100, Some(&trace));
            state.observe(
                Decision::Rebase(oink_foundation::Point::new(12345., -6789.)),
                2,
                200,
                200,
                Some(&trace),
            );
            state.observe(Decision::WaitUntil(1000), 1, 500, 500, Some(&trace));
            state.observe(Decision::Expected, 1, 1000, 1000, Some(&trace));
            state.late(4);
            state.emit(1_000_000, Some(&trace));
            state.emit(2_000_000, Some(&trace)); // No dirty data: no idle trace flood.
        });
        assert_eq!(state.record.timeouts, 2);
        assert_eq!(state.record.late_reads, 1);
        assert_eq!(state.record.wait_buckets, [1, 0, 1, 0, 1, 1]);
        trace.drain(&recorder);
        let bundle = recorder.save(super::super::Reason::Manual).unwrap();
        let records: Vec<_> = super::super::tests::timeline(&bundle.path, "merged.ndjson")
            .into_iter()
            .filter_map(|row| row["event"].get("Log").cloned())
            .filter(|row| row["module"] == "oink_cursor_reconciliation")
            .collect();
        assert_eq!(records.len(), 2);
        let text = serde_json::to_string(&records).unwrap();
        assert!(text.contains("timeouts") && text.contains("wait_gt_3000us"));
        assert!(!text.contains("12345") && !text.contains("6789"));
    }
}
