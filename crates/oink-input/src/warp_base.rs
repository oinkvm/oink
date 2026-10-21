//! Windows native cursor commits may precede GetCursorPos readback. Keep the
//! exact displaced bases across readback reversals; never infer a warp from the
//! size, rate, or age of physical movement.

use oink_foundation::Point;

#[derive(Default, Debug)]
pub struct WarpBase {
    sequence: u64,
    target: Option<Point>,
    displaced: Vec<Point>,
}

#[derive(Debug)]
pub struct Registration {
    sequence: u64,
    target: Option<Point>,
    displaced: Vec<Point>,
}

impl WarpBase {
    /// A swallowed hook target can still be based on a displaced native cursor,
    /// even after callbacks based on the accepted target have arrived. Resolve
    /// it against the registered positions, not the age of the placement. A real
    /// passed movement or a readback outside this chain retires those positions.
    #[cfg(any(windows, all(test, debug_assertions)))]
    pub(crate) fn displaced_base_for(&self, destination: Point, current: Point) -> Option<Point> {
        if self.target != Some(current) {
            return None;
        }
        let distance = |p: Point| (destination.x - p.x).powi(2) + (destination.y - p.y).powi(2);
        self.displaced
            .iter()
            .copied()
            .fold((None, distance(current)), |best, p| {
                let d = distance(p);
                if d < best.1 { (Some(p), d) } else { best }
            })
            .0
    }

    pub fn register(&mut self, before: Point, target: Point) -> Registration {
        self.sequence += 1;
        // Snapshot before retirement: a later hook may reject this placement.
        // Its rollback must restore the prior readback aliases as well as target.
        let registration = Registration {
            sequence: self.sequence,
            target: self.target,
            displaced: self.displaced.clone(),
        };
        // A subsequent placement starts from this observed native position.
        // Keep the existing chain only when that placement itself sees an older
        // base. Ordinary readback during a held cursor must not do this: Windows
        // can report target, then the displaced base again after swallowed hooks.
        if self.target == Some(before) {
            self.target = None;
            self.displaced.clear();
        } else {
            self.sample(before);
        }
        if before != target || self.target.is_some_and(|p| p != target) {
            if let Some(previous) = self.target
                && !self.displaced.contains(&previous)
            {
                self.displaced.push(previous);
            }
            if !self.displaced.contains(&before) {
                self.displaced.push(before);
            }
            self.target = Some(target);
        }
        registration
    }

    /// A failed native read cannot invent (0,0) as a displaced base. Use the
    /// latest registered commit when known, otherwise establish only the target.
    #[cfg(any(windows, all(test, debug_assertions)))]
    pub(crate) fn register_unobserved(&mut self, target: Point) -> Registration {
        self.sequence += 1;
        let registration = Registration {
            sequence: self.sequence,
            target: self.target,
            displaced: self.displaced.clone(),
        };
        // No readback confirmed that earlier aliases retired. In particular,
        // do not call register(previous_target, target): that would falsely
        // treat the previous commit as a freshly observed native position.
        if let Some(previous) = self.target
            && previous != target
            && !self.displaced.contains(&previous)
        {
            self.displaced.push(previous);
        }
        self.target = Some(target);
        registration
    }

    #[cfg(any(windows, all(test, debug_assertions)))]
    pub(crate) fn registered_target(&self) -> Option<Point> {
        self.target
    }

    /// A failed older operation must not cancel a newer native commit.
    pub fn cancel(&mut self, registration: Registration) -> bool {
        if self.sequence == registration.sequence {
            self.target = registration.target;
            self.displaced = registration.displaced;
            return true;
        }
        false
    }

    /// Take ONE snapshot per hook and use it for both old-step classification
    /// and delta calculation. Visibility still uses the actual native readback.
    pub fn sample(&mut self, observed: Point) -> Point {
        let Some(target) = self.target else { return observed };
        if observed == target || self.displaced.contains(&observed) {
            return target;
        }
        self.target = None;
        self.displaced.clear();
        observed
    }

    /// A physical target allowed through the hook can legitimately revisit an
    /// old displaced point. It is no longer an alias for lagging readback. A
    /// swallowed callback never calls this, because its target did not commit.
    pub fn passed(&mut self, target: Point) {
        self.displaced.retain(|p| *p != target);
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;

    #[test]
    fn rejected_external_commit_keeps_previous_readback_aliases() {
        let mut base = WarpBase::default();
        let park = Point::new(1000., 600.);
        let external = Point::new(300., 200.);
        base.register(park, external);
        let transaction = base.register(external, Point::new(10., 20.));
        assert!(base.cancel(transaction));
        assert_eq!(base.sample(park), external);
        assert_eq!(base.displaced_base_for(Point::new(1001., 600.), external), Some(park));
    }

    #[test]
    fn unavailable_readback_uses_known_commit_without_inventing_a_displacement() {
        let mut base = WarpBase::default();
        let first = Point::new(400., 500.);
        let second = Point::new(-100., 200.);
        base.register_unobserved(first);
        assert_eq!(base.registered_target(), Some(first));
        let token = base.register_unobserved(second);
        assert_eq!(base.sample(first), second);
        base.cancel(token);
        assert_eq!(base.sample(first), first);
    }

    #[test]
    fn unavailable_readback_keeps_all_known_displaced_aliases() {
        let mut base = WarpBase::default();
        let park = Point::new(1000., 600.);
        let first = Point::new(400., 500.);
        let second = Point::new(-100., 200.);
        base.register(park, first);
        let token = base.register_unobserved(second);
        for readback in [park, first, second, park] {
            assert_eq!(base.sample(readback), second);
        }
        assert!(base.cancel(token));
        assert_eq!(base.sample(park), first);
    }

    #[test]
    fn hook_targets_can_revisit_a_displaced_base_after_fresh_park_motion() {
        let departure = Point::new(501., 400.);
        let park = Point::new(1920., 1080.);
        let mut base = WarpBase::default();
        base.register(departure, park);
        assert_eq!(base.displaced_base_for(Point::new(1921., 1080.), park), None);
        for observed in [park, departure, park] {
            let current = base.sample(observed);
            let destination = Point::new(500., 402.);
            let selected = base.displaced_base_for(destination, current).unwrap_or(current);
            assert_eq!(
                Point::new(destination.x - selected.x, destination.y - selected.y),
                Point::new(-1., 2.),
                "a swallowed hook target never commits the program displacement as physical motion"
            );
        }
    }

    #[test]
    fn real_motion_and_retired_placements_do_not_reuse_displaced_coordinates() {
        let departure = Point::new(501., 400.);
        let park = Point::new(1920., 1080.);
        let mut base = WarpBase::default();
        base.register(departure, park);
        let fast = Point::new(3000., 1900.);
        assert_eq!(base.displaced_base_for(fast, base.target.unwrap()), None);
        base.passed(departure);
        let current = base.sample(departure);
        assert_eq!(current, departure);
        assert_eq!(base.displaced_base_for(Point::new(1921., 1080.), current), None);
    }

    #[test]
    fn exact_commits_cover_small_and_large_warps_without_expiry_or_motion_filter() {
        let from = Point::new(1920.0, 1080.0);
        for to in [Point::new(1921.0, 1080.0), Point::new(2170.0, 2159.0)] {
            let mut base = WarpBase::default();
            base.register(from, to);
            for _ in 0..1000 {
                assert_eq!(base.sample(from), to);
            }
            assert_eq!(base.sample(to), to);
            // A genuine passed move, not a readback, makes return to that exact
            // old coordinate real motion again.
            base.passed(from);
            assert_eq!(base.sample(from), from);
        }
    }

    #[test]
    fn a_swallowed_sample_at_the_park_does_not_retire_the_displaced_base() {
        let from = Point::new(2331., 2148.);
        let park = Point::new(1920., 1080.);
        let mut base = WarpBase::default();
        base.register(from, park);
        // All physical callbacks are swallowed. Native readback can alternate
        // between the acknowledged park and the old departure, without a move.
        for observed in [park, from, from, park, from] {
            let at = Point::new(1920., 1084.);
            let origin = base.sample(observed);
            assert_eq!((at.x - origin.x, at.y - origin.y), (0., 4.));
        }
    }

    #[test]
    fn successive_accepted_placements_do_not_accumulate_completed_history() {
        let mut base = WarpBase::default();
        for x in 0..10_000 {
            let before = Point::new(f64::from(x), 10.);
            let target = Point::new(f64::from(x + 1), 10.);
            base.register(before, target);
            assert_eq!(base.sample(target), target);
            assert_eq!(base.displaced, vec![before]);
        }
    }

    #[test]
    fn ordered_replacements_and_failed_calls_cannot_reactivate_old_targets() {
        let mut base = WarpBase::default();
        let a = Point::new(1.0, 1.0);
        let b = Point::new(2.0, 1.0);
        let c = Point::new(3.0, 1.0);
        let first = base.register(a, b);
        let second = base.register(a, c); // native readback is still a
        base.cancel(first);
        assert_eq!(base.sample(a), c);
        assert_eq!(base.sample(b), c);
        base.cancel(second);
        assert_eq!(base.sample(a), b); // failed c restores the accepted b commit
    }

    #[test]
    fn round_trip_refresh_replaces_the_intermediate_target_even_with_lagging_readback() {
        let a = Point::new(100.0, 100.0);
        let b = Point::new(101.0, 100.0);
        let mut base = WarpBase::default();
        base.register(a, b);
        base.register(a, a);
        assert_eq!(base.sample(b), a);
        assert_eq!(base.sample(a), a);
    }
}
