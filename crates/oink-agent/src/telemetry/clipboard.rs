//! Projection from transient clipboard timings to the closed report schema.
use oink_clipboard::{
    model::Unavailable,
    timing::{Measurement, Outcome},
};
use oink_telemetry::*;
use std::time::Instant;

fn outcome(value: Outcome) -> (OperationResult, TransferFailure) {
    use OperationResult as R;
    use TransferFailure as F;
    match value {
        Outcome::Completed => (R::Completed, F::None),
        Outcome::Cancelled | Outcome::Failed(Unavailable::Cancelled) => (R::Cancelled, F::Cancelled),
        Outcome::Failed(Unavailable::NoProgress) => (R::TimedOut, F::Timeout),
        Outcome::Failed(reason) => (
            R::Failed,
            match reason {
                Unavailable::Unsupported | Unavailable::Superseded | Unavailable::NoSuchItem => F::Unavailable,
                Unavailable::Permission => F::Permission,
                Unavailable::InvalidData => F::InvalidData,
                Unavailable::LimitExceeded | Unavailable::StorageFull => F::ResourceLimit,
                _ => F::Io,
            },
        ),
    }
}
fn metric(measurement: Measurement, elapsed_us: u64) -> Metric {
    match measurement {
        Measurement::FileFirstByte { size } => Metric::FileFirstByte {
            size: SizeBucket::from_bytes(size),
        },
        Measurement::FileComplete { size, outcome: value } => {
            let (result, reason) = outcome(value);
            Metric::FileComplete {
                result,
                reason,
                size: SizeBucket::from_bytes(size),
                throughput: if result == OperationResult::Completed {
                    ThroughputBucket::measured(size, elapsed_us)
                } else {
                    ThroughputBucket::Unknown
                },
            }
        }
        Measurement::Delivery { drag, outcome: value } => {
            let (result, reason) = outcome(value);
            if drag {
                Metric::DragTransfer { result, reason }
            } else {
                Metric::ClipboardTransfer { result, reason }
            }
        }
        Measurement::DropAccepted { accepted } => Metric::DragDropAccepted {
            result: if accepted {
                OperationResult::Completed
            } else {
                OperationResult::Cancelled
            },
        },
    }
}
pub(super) fn record(measurement: Measurement, start: Instant, end: Instant) {
    let Some(p) = super::producer() else { return };
    record_to(p, measurement, start, end);
}
fn record_to(p: &super::Ingress, measurement: Measurement, start: Instant, end: Instant) {
    // Reject starts predating the current grant, including revoke/regrant. The
    // ring also fences revocation between this check and callback publication.
    let Some(stamp) = p.stamp_at(start) else { return };
    let Some(elapsed) = end.checked_duration_since(start) else {
        return;
    };
    p.interval(
        stamp,
        metric(measurement, elapsed.as_micros().min(u64::MAX as u128) as u64),
        end,
    );
}

pub(super) fn native_read(
    elapsed: std::time::Duration,
    outcome: oink_clipboard::content::native_read::NativeReadOutcome,
) {
    let Some(p) = super::producer() else { return };
    native_read_to(p, elapsed, outcome, Instant::now());
}
fn native_read_to(
    p: &super::Ingress,
    elapsed: std::time::Duration,
    outcome: oink_clipboard::content::native_read::NativeReadOutcome,
    end: Instant,
) {
    use oink_clipboard::content::native_read::NativeReadOutcome as N;
    let Some(start) = end.checked_sub(elapsed) else { return };
    let Some(stamp) = p.stamp_at(start) else { return };
    let result = match outcome {
        N::Served => OperationResult::Completed,
        N::Failed => OperationResult::Failed,
        N::TimedOut => OperationResult::TimedOut,
        N::Cancelled => OperationResult::Cancelled,
    };
    p.interval(stamp, Metric::NativeRepresentationRead { result }, end);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn file_projection_discards_exact_sizes_and_reports_only_coarse_failure() {
        let m = metric(
            Measurement::FileComplete {
                size: 123_456_789,
                outcome: Outcome::Completed,
            },
            2_000_000,
        );
        assert_eq!(
            m,
            Metric::FileComplete {
                result: OperationResult::Completed,
                reason: TransferFailure::None,
                size: SizeBucket::UpTo256MiB,
                throughput: ThroughputBucket::UpTo100MiBps
            }
        );
        let encoded = serde_json::to_string(&m).unwrap();
        assert!(!encoded.contains("123456789"));
        assert!(!encoded.contains("2000000"));
        assert_eq!(
            metric(
                Measurement::FileComplete {
                    size: 99,
                    outcome: Outcome::Failed(Unavailable::NoProgress)
                },
                10
            ),
            Metric::FileComplete {
                result: OperationResult::TimedOut,
                reason: TransferFailure::Timeout,
                size: SizeBucket::UpTo64KiB,
                throughput: ThroughputBucket::Unknown
            }
        );
    }
    #[test]
    fn native_read_outcomes_use_elapsed_duration_and_reject_prior_grants() {
        use oink_clipboard::content::native_read::NativeReadOutcome as N;
        use std::time::Duration;
        let dir = tempfile::tempdir().unwrap();
        let owner = super::super::Owner::new(dir.path(), ReportConsent::default());
        let p = &owner.ingress;
        let start = Instant::now();
        let end = start + Duration::from_millis(20);
        p.realtime.arm(1, start);
        for (outcome, result) in [
            (N::Served, OperationResult::Completed),
            (N::Failed, OperationResult::Failed),
            (N::TimedOut, OperationResult::TimedOut),
            (N::Cancelled, OperationResult::Cancelled),
        ] {
            native_read_to(p, Duration::from_millis(5), outcome, end);
            let record = p.realtime.pop().unwrap();
            assert_eq!(record.metric, Metric::NativeRepresentationRead { result });
            assert_eq!(record.elapsed_us, 5000);
        }
        p.realtime.arm(2, end);
        native_read_to(p, Duration::from_millis(5), N::Served, end);
        assert!(p.realtime.pop().is_none(), "the callback began before this grant");
        native_read_to(p, Duration::from_secs(121), N::Served, end + Duration::from_secs(121));
        assert!(p.realtime.pop().is_none(), "over-limit intervals must not be clipped");
    }

    #[test]
    fn collection_requires_the_current_grant_from_start_to_finish() {
        use std::{sync::atomic::Ordering, time::Duration};
        let dir = tempfile::tempdir().unwrap();
        let owner = super::super::Owner::new(dir.path(), ReportConsent::default());
        let p = &owner.ingress;
        let at = Instant::now();
        let end = at + Duration::from_millis(7);
        let measurement = Measurement::DropAccepted { accepted: true };
        record_to(p, measurement, at, end);
        assert!(p.realtime.pop().is_none());
        p.generation.store(1, Ordering::Release);
        p.realtime.arm(1, at);
        record_to(p, measurement, at, end);
        let record = p.realtime.pop().unwrap();
        assert_eq!(
            record.metric,
            Metric::DragDropAccepted {
                result: OperationResult::Completed
            }
        );
        assert_eq!(record.elapsed_us, 7000);
        p.realtime.disarm();
        record_to(p, measurement, at, end);
        assert!(p.realtime.pop().is_none());
        p.realtime.arm(2, end);
        record_to(p, measurement, at, end);
        assert!(
            p.realtime.pop().is_none(),
            "a new grant cannot collect an old operation"
        );
    }
}
