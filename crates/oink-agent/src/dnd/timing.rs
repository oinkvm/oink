//! Host-local drag intervals. Correlation keys never enter the report schema.
#[cfg(test)]
mod tests {
    use super::*;
    use oink_foundation::DeviceId;
    use std::time::{Duration, Instant};
    #[test]
    fn source_notice_requires_matching_inventory_and_learned_and_completes_once() {
        let at = Instant::now();
        let op = oink_clipboard::dnd::DragOperation::new();
        let id = ClipId {
            origin: DeviceId([1; 32]),
            seq: 1,
        };
        let other = ClipId { seq: 2, ..id };
        let mut trace = SourceTimeline::default();
        trace.observed(oink_clipboard::dnd::SourceTiming {
            operation: op,
            probed_at: Some(at),
            recognized_at: at + Duration::from_micros(2),
            native_read_at: at + Duration::from_micros(4),
            inventory_at: at + Duration::from_micros(5),
        });
        trace.bind(op, id);
        assert!(
            trace
                .notice(other, DragState::Announced, at + Duration::from_micros(8))
                .is_none()
        );
        assert!(
            trace
                .notice(id, DragState::Announced, at + Duration::from_micros(8))
                .is_none()
        );
        trace.learned(id, at + Duration::from_micros(7));
        assert!(
            trace
                .notice(id, DragState::Ended { accepted: false }, at + Duration::from_micros(8))
                .is_none()
        );
        let completed = trace
            .notice(id, DragState::Announced, at + Duration::from_micros(12))
            .unwrap();
        assert_eq!(completed.native.recognized_at, at + Duration::from_micros(2));
        assert_eq!(completed.learned_at, Some(at + Duration::from_micros(7)));
        assert!(
            trace
                .notice(id, DragState::Announced, at + Duration::from_micros(13))
                .is_none()
        );
        trace.observed(completed.native);
        assert!(
            trace.active.is_none(),
            "a completed operation cannot be resurrected by its late worker"
        );
    }
    #[test]
    fn late_source_worker_cannot_replace_newer_operation() {
        let at = Instant::now();
        let old = oink_clipboard::dnd::DragOperation::new();
        let new = oink_clipboard::dnd::DragOperation::new();
        let observed = |operation| oink_clipboard::dnd::SourceTiming {
            operation,
            probed_at: None,
            recognized_at: at,
            native_read_at: at,
            inventory_at: at,
        };
        let mut trace = SourceTimeline::default();
        trace.observed(observed(new));
        trace.observed(observed(old));
        assert_eq!(trace.active.as_ref().unwrap().native.operation, new);
    }
}

use oink_clipboard::dnd::{DragOperation, NativeBeginTiming, SourceTiming};
use oink_foundation::content::ClipId;
use oink_routing::dnd::DragState;
use oink_telemetry::{Metric, SourceStage, TargetStartBoundary};
use std::{sync::Mutex, time::Instant};

#[derive(Default)]
struct SourceTimeline {
    active: Option<SourceTrace>,
    last_operation: u64,
}
struct SourceTrace {
    native: SourceTiming,
    id: Option<ClipId>,
    learned_at: Option<Instant>,
}
impl SourceTimeline {
    fn observed(&mut self, native: SourceTiming) {
        if native.operation.sequence > self.last_operation {
            self.last_operation = native.operation.sequence;
            self.active = Some(SourceTrace {
                native,
                id: None,
                learned_at: None,
            });
        }
    }
    fn bind(&mut self, operation: DragOperation, id: ClipId) {
        if let Some(trace) = self.active.as_mut().filter(|trace| trace.native.operation == operation) {
            trace.id = Some(id);
        }
    }
    fn learned(&mut self, id: ClipId, at: Instant) {
        if let Some(trace) = self.active.as_mut().filter(|trace| trace.id == Some(id)) {
            trace.learned_at.get_or_insert(at);
        }
    }
    fn notice(&mut self, id: ClipId, state: DragState, at: Instant) -> Option<SourceTrace> {
        if !matches!(state, DragState::Announced | DragState::Offered { .. }) {
            return None;
        }
        let trace = self.active.as_ref()?;
        if trace.id != Some(id) || at < trace.learned_at? || trace.learned_at? < trace.native.inventory_at {
            return None;
        }
        self.active.take()
    }
}
static SOURCE: Mutex<SourceTimeline> = Mutex::new(SourceTimeline {
    active: None,
    last_operation: 0,
});
fn source_interval(stage: SourceStage, start: Instant, end: Instant) {
    let Some(elapsed) = end.checked_duration_since(start) else {
        return;
    };
    tracing::debug!(
        dnd_phase = 60_u64,
        ?stage,
        elapsed_us = elapsed.as_micros() as u64,
        "source drag local stage finished"
    );
    if let Some(p) = crate::telemetry::producer()
        && let Some(stamp) = p.stamp_at(start)
    {
        p.interval(stamp, Metric::DragSourceStage { stage }, end);
    }
}
pub(crate) fn observed(native: SourceTiming) {
    if native.native_read_at < native.recognized_at || native.inventory_at < native.native_read_at {
        return;
    }
    source_interval(SourceStage::NativeRead, native.recognized_at, native.native_read_at);
    source_interval(
        SourceStage::MetadataCompletion,
        native.native_read_at,
        native.inventory_at,
    );
    if let Some(probed) = native.probed_at {
        source_interval(SourceStage::ProbeRecognition, probed, native.recognized_at);
    }
    source_interval(SourceStage::Inventory, native.recognized_at, native.inventory_at);
    SOURCE.lock().unwrap().observed(native);
}
pub(crate) fn bind(operation: DragOperation, id: ClipId) {
    SOURCE.lock().unwrap().bind(operation, id);
}
pub(crate) fn learned(id: ClipId, at: Instant) {
    SOURCE.lock().unwrap().learned(id, at);
}
pub(crate) fn notice(id: ClipId, state: DragState, at: Instant) {
    let trace = SOURCE.lock().unwrap().notice(id, state, at);
    if let Some(trace) = trace {
        let learned = trace.learned_at.expect("paired learned");
        source_interval(SourceStage::InventoryToLearned, trace.native.inventory_at, learned);
        source_interval(SourceStage::LearnedToNotice, learned, at);
        source_interval(SourceStage::RecognizedToNotice, trace.native.recognized_at, at);
    }
}

pub(crate) fn arm_target(
    backend: &dyn oink_clipboard::dnd::DndBackend,
    operation: DragOperation,
    entered: Option<Instant>,
) {
    let timing = entered.map(|at| NativeBeginTiming {
        queued_at: at,
        generation: crate::telemetry::producer()
            .and_then(|p| p.stamp_at(at))
            .map_or(0, |stamp| stamp.generation),
        record: target_started,
    });
    backend.target_start_timing(operation, timing);
}
fn target_started(generation: u64, start: Instant, end: Instant) {
    let Some(elapsed) = end.checked_duration_since(start) else {
        return;
    };
    let boundary = if cfg!(target_os = "macos") {
        TargetStartBoundary::AppkitSessionReturned
    } else {
        TargetStartBoundary::OleCallEnter
    };
    tracing::debug!(
        dnd_phase = 29_u64,
        ?boundary,
        elapsed_us = elapsed.as_micros() as u64,
        "target drag entry to native start finished"
    );
    if generation != 0
        && let Some(p) = crate::telemetry::producer()
    {
        p.interval(
            crate::telemetry::Stamp {
                generation,
                at: start,
                transport: None,
            },
            Metric::DragTargetStart { boundary },
            end,
        );
    }
}
