//! Ordered target-side readback. No native calls, routing, or failure policy.
//! A read error preserves the caller's predicted point; it never pauses input.
use oink_foundation::Point;

/// A target actually submitted to the native input owner. This is not proof
/// that the desktop has committed its cursor position yet.
#[derive(Clone, Copy, Debug)]
pub struct Expected {
    pub scope: u64,
    pub sequence: u64,
    pub before: Option<Point>,
    pub target: Point,
    pub tolerance: f64,
}
#[derive(Clone, Copy, Debug)]
pub struct Observation {
    pub scope: u64,
    pub sequence: u64,
    pub position: Option<Point>,
    pub expected: Option<Expected>,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Decision {
    /// Keep the virtual fractional remainder and existing edge pressure.
    Expected,
    Rebase(Point),
    /// Fresh actual position wins after the explicitly bounded ambiguity grace.
    Ambiguous(Point),
    WaitUntil(u64),
    /// Neither result is a native injection fault. Preserve today's prediction.
    Unavailable,
    Stale,
}
#[derive(Default, Debug)]
pub struct Reconciler {
    scope: Option<u64>,
    sequence: u64,
    ambiguous: Option<(u64, u64)>,
}
impl Reconciler {
    pub fn observe(&mut self, scope: u64, sample: Observation, now: u64) -> Decision {
        if self.scope != Some(scope) {
            self.scope = Some(scope);
            self.sequence = 0;
            self.ambiguous = None;
        }
        if sample.scope != scope || sample.sequence < self.sequence {
            return Decision::Stale;
        }
        self.sequence = sample.sequence;
        let Some(actual) = sample.position.filter(|p| p.x.is_finite() && p.y.is_finite()) else {
            return Decision::Unavailable;
        };
        if let Some(expected) = sample
            .expected
            .filter(|e| e.scope == scope && e.sequence == sample.sequence)
        {
            let tolerance = if expected.tolerance.is_finite() {
                expected.tolerance.max(0.)
            } else {
                0.
            };
            let matches = |p: Point| (p.x - actual.x).abs() <= tolerance && (p.y - actual.y).abs() <= tolerance;
            if matches(expected.target) {
                self.ambiguous = None;
                return Decision::Expected;
            }
            if expected.before.is_some_and(matches) {
                let (_, since) = self.ambiguous.get_or_insert((expected.sequence, now));
                let since = *since;
                if self.ambiguous.is_some_and(|(seq, _)| seq != expected.sequence) {
                    self.ambiguous = Some((expected.sequence, now));
                    return Decision::WaitUntil(now.saturating_add(250));
                }
                let age = now.saturating_sub(since);
                if let Some(delay) = [250, 1000, 2000].into_iter().find(|delay| age < *delay) {
                    return Decision::WaitUntil(since.saturating_add(delay));
                }
                // Do not reset the grace for this same receipt. Repeated reads
                // must not indefinitely defer a real warp back to `before`.
                return Decision::Ambiguous(actual);
            }
        }
        self.ambiguous = None;
        Decision::Rebase(actual)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sample(at: f64, before: f64, target: f64) -> Observation {
        Observation {
            scope: 7,
            sequence: 1,
            position: Some(Point::new(at, 10.)),
            expected: Some(Expected {
                scope: 7,
                sequence: 1,
                before: Some(Point::new(before, 10.)),
                target: Point::new(target, 10.),
                tolerance: 0.,
            }),
        }
    }
    #[test]
    fn external_warp_is_adopted_before_the_next_delta() {
        let mut s = Reconciler::default();
        assert_eq!(
            s.observe(7, sample(400., 100., 110.), 0),
            Decision::Rebase(Point::new(400., 10.))
        );
    }
    #[test]
    fn own_expected_preserves_fractional_virtual_motion() {
        let mut s = Reconciler::default();
        assert_eq!(s.observe(7, sample(110., 100., 110.), 0), Decision::Expected);
    }
    #[test]
    fn delayed_own_sample_must_not_erase_already_routed_motion() {
        let mut s = Reconciler::default();
        let mut current = sample(110., 100., 110.);
        current.sequence = 2;
        current.expected.as_mut().unwrap().sequence = 2;
        assert_eq!(s.observe(7, current, 0), Decision::Expected);
        assert_eq!(s.observe(7, sample(100., 90., 100.), 100), Decision::Stale);
    }
    #[test]
    fn own_commit_lag_gets_two_ms_then_actual_wins() {
        let mut s = Reconciler::default();
        let x = sample(100., 100., 110.);
        assert_eq!(s.observe(7, x, 10), Decision::WaitUntil(260));
        assert_eq!(s.observe(7, x, 260), Decision::WaitUntil(1010));
        assert_eq!(s.observe(7, x, 1010), Decision::WaitUntil(2010));
        assert_eq!(s.observe(7, x, 2010), Decision::Ambiguous(Point::new(100., 10.)));
    }
    #[test]
    fn commit_during_grace_does_not_rebase_to_the_old_point() {
        let mut s = Reconciler::default();
        assert!(matches!(
            s.observe(7, sample(100., 100., 110.), 0),
            Decision::WaitUntil(_)
        ));
        assert_eq!(s.observe(7, sample(110., 100., 110.), 250), Decision::Expected);
    }
    #[test]
    fn external_movement_during_grace_does_not_wait_for_stillness() {
        let mut s = Reconciler::default();
        s.observe(7, sample(100., 100., 110.), 0);
        assert_eq!(
            s.observe(7, sample(400., 100., 110.), 100),
            Decision::Rebase(Point::new(400., 10.))
        );
        assert_eq!(
            s.observe(7, sample(450., 100., 110.), 200),
            Decision::Rebase(Point::new(450., 10.))
        );
    }
    #[test]
    fn errors_and_non_finite_positions_keep_todays_prediction_without_fault() {
        for position in [
            None,
            Some(Point::new(f64::NAN, 0.)),
            Some(Point::new(0., f64::INFINITY)),
        ] {
            let mut s = Reconciler::default();
            let mut x = sample(100., 100., 110.);
            x.position = position;
            assert_eq!(s.observe(7, x, 0), Decision::Unavailable);
            assert_eq!(s.observe(7, x, 5_000_000), Decision::Unavailable);
        }
    }
    #[test]
    fn old_focus_game_layout_or_safety_generation_cannot_rebase() {
        let mut s = Reconciler::default();
        for scope in [8, 9, 10, 11] {
            assert_eq!(s.observe(scope, sample(400., 100., 110.), 0), Decision::Stale);
        }
    }
    #[test]
    fn a_previous_scope_receipt_cannot_hide_a_new_external_position() {
        let mut s = Reconciler::default();
        let mut x = sample(100., 100., 110.);
        x.expected.as_mut().unwrap().scope = 6;
        assert_eq!(s.observe(7, x, 0), Decision::Rebase(Point::new(100., 10.)));
    }
    #[test]
    fn one_windows_pixel_is_an_external_move() {
        let mut s = Reconciler::default();
        assert_eq!(
            s.observe(7, sample(111., 100., 110.), 0),
            Decision::Rebase(Point::new(111., 10.))
        );
    }
    #[test]
    fn native_quantization_tolerance_is_not_a_desktop_pixel() {
        let mut s = Reconciler::default();
        let mut x = sample(110.0001, 100., 110.);
        x.expected.as_mut().unwrap().tolerance = 1. / 256.;
        assert_eq!(s.observe(7, x, 0), Decision::Expected);
        x.position = Some(Point::new(110.01, 10.));
        assert_eq!(s.observe(7, x, 1), Decision::Rebase(Point::new(110.01, 10.)));
    }
    #[test]
    fn no_receipt_means_a_real_read_not_a_zero_origin_assumption() {
        let mut s = Reconciler::default();
        let mut x = sample(-300., 100., 110.);
        x.expected = None;
        assert_eq!(s.observe(7, x, 0), Decision::Rebase(Point::new(-300., 10.)));
    }
    #[test]
    fn indistinguishable_warp_and_lag_have_the_same_explicit_policy() {
        let mut lag = Reconciler::default();
        let mut warp = Reconciler::default();
        for now in [0, 250, 1000, 2000] {
            assert_eq!(
                lag.observe(7, sample(100., 100., 110.), now),
                warp.observe(7, sample(100., 100., 110.), now)
            );
        }
    }
}
