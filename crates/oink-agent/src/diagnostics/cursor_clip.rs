//! Numeric system-clip transitions and bounded cumulative crossing counts.
use super::{Event, Recorder, input_trace::InputTrace};
use oink_foundation::{DeviceId, Rect};
#[derive(Clone, Copy, PartialEq)]
pub(crate) struct Record {
    pub holder: DeviceId,
    pub epoch: u64,
    pub rect: Option<Rect>,
    pub active: bool,
    pub available: bool,
    pub suppressed: u64,
}
impl Record {
    pub(super) fn export(self, recorder: &Recorder) -> Event {
        let mut fields = vec![
            ("epoch".into(), self.epoch as f64),
            ("active".into(), f64::from(self.active)),
            ("sample_available".into(), f64::from(self.available)),
            ("suppressed_crossings".into(), self.suppressed as f64),
        ];
        if let Some(r) = self.rect {
            fields.extend([
                ("x".into(), r.x),
                ("y".into(), r.y),
                ("width".into(), r.w),
                ("height".into(), r.h),
            ]);
        }
        Event::Log {
            module: "oink_cursor_clip".into(),
            line: 0,
            level: 2,
            fields,
            labels: vec![("holder".into(), recorder.anonymizer().device(self.holder))],
        }
    }
}
#[derive(Default)]
pub(crate) struct State {
    last: Option<Record>,
    next_count: u64,
}
impl State {
    pub(crate) fn observe(&mut self, record: Record, now: u64, trace: Option<&InputTrace>) {
        let Some(trace) = trace.filter(|t| t.enabled()) else {
            self.last = None;
            return;
        };
        let transition = self.last.is_none_or(|p| {
            p.rect != record.rect
                || p.active != record.active
                || p.available != record.available
                || p.holder != record.holder
                || p.epoch != record.epoch
        });
        if transition || (self.last != Some(record) && now >= self.next_count) {
            self.last = Some(record);
            self.next_count = now.saturating_add(1_000_000);
            trace.system_cursor_clip(record);
        }
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    #[test]
    fn cursor_clip_records_transitions_and_coalesces_suppression_counts() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = Recorder::start(dir.path().into(), DeviceId([1; 32]), None, true).unwrap();
        recorder.wait_recovery();
        let trace = InputTrace::for_recorder(&recorder);
        let mut state = State::default();
        let mut record = Record {
            holder: DeviceId([1; 32]),
            epoch: 2,
            rect: Some(Rect::new(-100., 20., 300., 400.)),
            active: true,
            available: true,
            suppressed: 0,
        };
        state.observe(record, 0, Some(&trace));
        for count in 1..100 {
            record.suppressed = count;
            state.observe(record, count * 1000, Some(&trace));
        }
        state.observe(record, 1_000_000, Some(&trace));
        record.rect = None;
        record.active = false;
        state.observe(record, 1_001_000, Some(&trace));
        trace.drain(&recorder);
        let bundle = recorder.save(super::super::Reason::Manual).unwrap();
        let records: Vec<_> = super::super::tests::timeline(&bundle.path, "merged.ndjson")
            .into_iter()
            .filter_map(|row| row["event"].get("Log").cloned())
            .filter(|row| row["module"] == "oink_cursor_clip")
            .collect();
        assert_eq!(records.len(), 3);
        let text = serde_json::to_string(&records).unwrap();
        assert!(text.contains("suppressed_crossings") && text.contains("width") && text.contains("-100"));
    }
}
