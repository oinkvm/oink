//! One ordinary park attempt per cover generation. Only its validated native
//! input can commit it; an ACK or an unrelated physical MOVE cannot place it.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Queued,
    Accepted,
    Complete,
    Failed,
}

#[derive(Default)]
pub(crate) struct ParkCommit {
    attempt: Option<(u64, Phase)>,
}

impl ParkCommit {
    pub(crate) fn request(&mut self, generation: u64) -> bool {
        if self.attempt.is_some_and(|(current, _)| current == generation) {
            return false;
        }
        self.attempt = Some((generation, Phase::Queued));
        true
    }

    pub(crate) fn accept(&mut self, generation: u64) -> bool {
        if self.attempt != Some((generation, Phase::Queued)) {
            return false;
        }
        self.attempt = Some((generation, Phase::Accepted));
        true
    }

    pub(crate) fn accepted(&self, generation: u64) -> bool {
        self.attempt == Some((generation, Phase::Accepted))
    }

    pub(crate) fn complete(&mut self, generation: u64) -> bool {
        if !self.accepted(generation) {
            return false;
        }
        self.attempt = Some((generation, Phase::Complete));
        true
    }

    pub(crate) fn fail(&mut self, generation: u64) -> bool {
        if !matches!(self.attempt, Some((current, Phase::Queued | Phase::Accepted)) if current == generation) {
            return false;
        }
        self.attempt = Some((generation, Phase::Failed));
        true
    }

    pub(crate) fn cancel(&mut self) {
        self.attempt = None;
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    use crate::{stale_step::StaleSteps, warp_base::WarpBase};
    use oink_foundation::Point;

    #[test]
    fn cover_ack_input_acceptance_and_native_completion_are_distinct() {
        let mut commit = ParkCommit::default();
        assert!(commit.request(1));
        assert!(
            !commit.complete(1),
            "neither ACK nor SendInput return alone proves hook acceptance"
        );
        assert!(!commit.request(1), "duplicate ACK never submits another input");
        assert!(commit.accept(1));
        assert!(commit.accepted(1));
        assert!(!commit.accept(1), "a duplicated native marker cannot commit twice");
        assert!(commit.complete(1));
        assert!(!commit.complete(1));
        assert!(!commit.request(1));
        assert!(!commit.fail(1));
    }

    #[test]
    fn failed_or_cancelled_input_never_silently_retries_its_generation() {
        for accepted in [false, true] {
            let mut commit = ParkCommit::default();
            assert!(commit.request(8));
            if accepted {
                assert!(commit.accept(8));
            }
            assert!(commit.fail(8));
            assert!(!commit.request(8));
            assert!(!commit.accept(8));
            assert!(!commit.complete(8));
            assert!(!commit.fail(8), "record a failure only once");
            assert!(commit.request(9));
            assert!(!commit.accept(8));
            commit.cancel();
            assert!(!commit.accept(9));
            assert!(!commit.complete(9));
        }
    }

    #[test]
    fn park_input_before_readback_does_not_forward_its_displacement() {
        let park = Point::new(1920.0, 1080.0);
        let from = Point::new(2170.0, 2159.0);
        for (before, target) in [(from, park), (park, from)] {
            for step in [Point::new(-1.0, 0.0), Point::new(-1.0, 1.0)] {
                let mut stale = StaleSteps::new();
                let mut base = WarpBase::default();
                let mut commit = ParkCommit::default();
                assert!(commit.request(1));
                assert_eq!(base.sample(before), before, "ACK never pre-warps");
                assert!(commit.accept(1));
                base.register(before, target);
                stale.switched(before, 1_000);
                // The native input is accepted but GetCursorPos still reads its
                // previous position. Both classification and subtraction use
                // the same accounted sample, for park AND return.
                let pt = Point::new(target.x + step.x, target.y + step.y);
                let current = base.sample(before);
                let current = stale.classify(pt, current, 2_000).unwrap_or(current);
                assert_eq!(Point::new(pt.x - current.x, pt.y - current.y), step);
                assert!(commit.complete(1));
            }
        }
    }
}
