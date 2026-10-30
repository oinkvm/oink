//! Windows native cursor commits may precede GetCursorPos readback. Keep the
//! exact displaced bases across readback reversals; never infer a warp from the
//! size, rate, or age of physical movement.

use oink_foundation::Point;

#[derive(Default, Debug)]
pub struct WarpBase {
    sequence: u64,
    target: Option<Point>,
    // Every own placement can still be returned by lagging native readback.
    displaced: Vec<Point>,
    // Shape-refresh coordinates are not lasting origins of physical motion.
    motion_origins: Vec<Point>,
}

#[derive(Debug)]
pub struct Registration {
    sequence: u64,
    target: Option<Point>,
    displaced: Vec<Point>,
    motion_origins: Vec<Point>,
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
        self.motion_origins
            .iter()
            .copied()
            .fold((None, distance(current)), |best, p| {
                let d = distance(p);
                if d < best.1 { (Some(p), d) } else { best }
            })
            .0
    }

    pub fn register(&mut self, before: Point, target: Point) -> Registration {
        self.register_placement(before, target, false)
    }

    #[cfg(any(windows, all(test, debug_assertions)))]
    pub(crate) fn register_refresh(&mut self, before: Point, target: Point) -> Registration {
        self.register_placement(before, target, true)
    }

    fn register_placement(&mut self, before: Point, target: Point, refresh: bool) -> Registration {
        self.sequence += 1;
        // Snapshot before retirement: a later hook may reject this placement.
        // Its rollback must restore the prior readback aliases as well as target.
        let registration = Registration {
            sequence: self.sequence,
            target: self.target,
            displaced: self.displaced.clone(),
            motion_origins: self.motion_origins.clone(),
        };
        // A subsequent placement starts from this observed native position.
        // Keep the existing chain only when that placement itself sees an older
        // base. Ordinary readback during a held cursor must not do this: Windows
        // can report target, then the displaced base again after swallowed hooks.
        // A tagged refresh preserves those earlier real origins and aliases even
        // when its starting readback has caught up. Geometry alone never selects it.
        if !refresh && self.target == Some(before) {
            self.target = None;
            self.displaced.clear();
            self.motion_origins.clear();
        } else {
            self.sample(before);
        }
        if before != target || self.target.is_some_and(|p| p != target) {
            for p in self.target.into_iter().chain([before]) {
                if !self.displaced.contains(&p) {
                    self.displaced.push(p);
                }
                if !refresh && !self.motion_origins.contains(&p) {
                    self.motion_origins.push(p);
                }
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
            motion_origins: self.motion_origins.clone(),
        };
        // No readback confirmed that earlier aliases retired. In particular,
        // do not call register(previous_target, target): that would falsely
        // treat the previous commit as a freshly observed native position.
        if let Some(previous) = self.target.filter(|p| *p != target) {
            if !self.displaced.contains(&previous) {
                self.displaced.push(previous);
            }
            if !self.motion_origins.contains(&previous) {
                self.motion_origins.push(previous);
            }
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
            self.motion_origins = registration.motion_origins;
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
        self.motion_origins.clear();
        observed
    }

    /// A physical target allowed through the hook can legitimately revisit an
    /// old displaced point. It is no longer an alias for lagging readback. A
    /// swallowed callback never calls this, because its target did not commit.
    pub fn passed(&mut self, target: Point) {
        self.displaced.retain(|p| *p != target);
        self.motion_origins.retain(|p| *p != target);
    }
}

// Pure native displacement accounting, shared with deterministic host tests.
#[cfg(any(windows, all(test, debug_assertions)))]
#[derive(Default)]
pub(crate) struct Accounting {
    pub(crate) base: WarpBase,
    // Preserve the pre-existing old-coordinate-flight correction. This is
    // separate from the exact readback repair above; all placements now update
    // both, rather than only mode transitions knowing their displaced base.
    pub(crate) stale: crate::stale_step::StaleSteps,
}

#[cfg(any(windows, all(test, debug_assertions)))]
impl Accounting {
    pub(crate) fn stale_base_for(&mut self, pt: Point, current: Point, now_us: u64) -> Option<Point> {
        // Windows can resume old-coordinate targets even after fresh park-based
        // hooks and an idle interval. Keep those origins beyond StaleSteps' flight
        // window so Remote never forwards the park displacement as hand motion.
        // A delayed old-park target can also arrive after a Local restore. Both
        // modes need genuine displaced origins; explicitly tagged shape refresh
        // placements contribute only readback aliases, never those origins.
        self.base
            .displaced_base_for(pt, current)
            .or_else(|| self.stale.classify(pt, current, now_us))
    }

    pub(crate) fn register(
        &mut self,
        before: Point,
        target: Point,
        now_us: u64,
    ) -> (Registration, crate::stale_step::StaleSteps) {
        let previous = self.stale;
        let current = self.base.sample(before);
        let registration = self.base.register(before, target);
        if current != target {
            self.stale.switched(current, now_us);
        }
        (registration, previous)
    }

    pub(crate) fn register_refresh(
        &mut self,
        before: Point,
        target: Point,
    ) -> (Registration, crate::stale_step::StaleSteps) {
        // Preserve the real pre-refresh flight base and its original clock.
        // cursor::nudge tags both legs, including their reentrant hook echoes.
        (self.base.register_refresh(before, target), self.stale)
    }

    pub(crate) fn cancel(&mut self, (registration, previous): (Registration, crate::stale_step::StaleSteps)) -> bool {
        if self.base.cancel(registration) {
            self.stale = previous;
            return true;
        }
        false
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;

    /// The native shape refresh moves one pixel right and back. Local motion
    /// must advance from the returned position instead of repeatedly matching
    /// the intermediate point and being swallowed as an old-coordinate event.
    #[test]
    fn local_circle_keeps_its_rightward_arc_after_shape_refresh() {
        let entry = Point::new(1200., 2100.);
        let nudge = Point::new(entry.x + 1., entry.y);
        for return_readback in [entry, nudge] {
            let mut accounting = Accounting::default();
            accounting.register_refresh(entry, nudge);
            accounting.register_refresh(return_readback, entry);
            let mut native = entry;
            for now_us in 2_000..2_100 {
                let current = accounting.base.sample(native);
                let next = Point::new(native.x + 4., native.y - 2.);
                if accounting.stale_base_for(next, current, now_us).is_none() {
                    // This is the native commit of an allowed Local callback.
                    accounting.base.passed(next);
                    native = next;
                }
            }
            assert_eq!(
                native,
                Point::new(1600., 1900.),
                "the rightward arc must not be swallowed"
            );
        }
    }

    #[test]
    fn local_same_entry_restore_keeps_old_park_motion_after_idle() {
        let entry = Point::new(1200., 2100.);
        let park = Point::new(1920., 1080.);
        for fresh_seen in [false, true] {
            let mut accounting = Accounting::default();
            accounting.register(entry, park, 1_000);
            accounting.register(park, entry, 1_001);
            if fresh_seen {
                // A same-coordinate physical callback can pass and retire the
                // short classifier without moving away from the entry. An old
                // park-based target can then arrive immediately, not just late.
                assert_eq!(accounting.stale_base_for(entry, entry, 1_002), None);
                accounting.base.passed(entry);
            }
            let late = 1_001 + crate::stale_step::STALE_WINDOW_US + 10_000_000;
            for now_us in [1_003, late] {
                let current = accounting.base.sample(entry);
                let target = Point::new(park.x + 4., park.y - 2.);
                let base = accounting.stale_base_for(target, current, now_us).unwrap_or(current);
                assert_eq!(Point::new(target.x - base.x, target.y - base.y), Point::new(4., -2.));
            }
        }
    }

    #[test]
    fn remote_shape_refresh_preserves_the_forwarded_motion_sum() {
        let park = Point::new(1920., 1080.);
        let nudge = Point::new(park.x + 1., park.y);
        for readback in [park, nudge] {
            let mut accounting = Accounting::default();
            accounting.register_refresh(park, nudge);
            accounting.register_refresh(readback, park);
            let mut forwarded = Point::new(0., 0.);
            for now_us in 2_000..2_100 {
                let current = accounting.base.sample(park);
                let target = Point::new(park.x + 4., park.y - 2.);
                let base = accounting.stale_base_for(target, current, now_us).unwrap_or(current);
                forwarded.x += target.x - base.x;
                forwarded.y += target.y - base.y;
                // Remote swallows the native callback; the cursor stays at park.
            }
            assert_eq!(forwarded, Point::new(400., -200.));
        }
    }

    #[test]
    fn tagged_refresh_preserves_real_departure_origins_and_lagging_readbacks() {
        let departure = Point::new(500., 2100.);
        let park = Point::new(1920., 1080.);
        let nudge = Point::new(park.x + 1., park.y);
        for before_return in [park, nudge] {
            let mut accounting = Accounting::default();
            accounting.register(departure, park, 1_000);
            accounting.register_refresh(park, nudge);
            accounting.register_refresh(before_return, park);
            let late = 1_000 + crate::stale_step::STALE_WINDOW_US + 10_000_000;
            for readback in [park, nudge, departure, park] {
                let current = accounting.base.sample(readback);
                assert_eq!(current, park);
                assert_eq!(
                    accounting.stale_base_for(Point::new(departure.x - 1., departure.y + 2.), current, late),
                    Some(departure)
                );
                assert_eq!(
                    accounting.stale_base_for(Point::new(park.x + 4., park.y - 2.), current, late),
                    None,
                    "only the explicitly tagged refresh point is excluded"
                );
            }
        }
    }

    #[test]
    fn identical_geometry_distinguishes_real_placements_from_tagged_refresh() {
        let a = Point::new(1200., 2100.);
        let b = Point::new(a.x + 1., a.y);
        let next = Point::new(a.x + 4., a.y - 2.);
        let mut real = WarpBase::default();
        real.register(a, b);
        real.register(b, a);
        assert_eq!(real.displaced_base_for(next, a), Some(b));
        let mut refresh = WarpBase::default();
        refresh.register_refresh(a, b);
        refresh.register_refresh(b, a);
        assert_eq!(refresh.displaced_base_for(next, a), None);
        assert_eq!(refresh.sample(b), a);
    }

    #[test]
    fn rejected_tagged_refresh_restores_both_aliases_and_real_origins() {
        let departure = Point::new(500., 2100.);
        let park = Point::new(1920., 1080.);
        let mut accounting = Accounting::default();
        accounting.register(departure, park, 1_000);
        let rejected = accounting.register_refresh(park, Point::new(park.x + 1., park.y));
        assert!(accounting.cancel(rejected));
        assert_eq!(accounting.base.sample(departure), park);
        assert_eq!(
            accounting
                .base
                .displaced_base_for(Point::new(departure.x + 4., departure.y - 2.), park),
            Some(departure)
        );
    }

    #[test]
    fn local_shape_refresh_preserves_exact_lagging_readback_repair() {
        let entry = Point::new(1200., 2100.);
        let nudge = Point::new(entry.x + 1., entry.y);
        let mut accounting = Accounting::default();
        accounting.register_refresh(entry, nudge);
        accounting.register_refresh(entry, entry);
        let current = accounting.base.sample(nudge);
        assert_eq!(
            current, entry,
            "readback aliases still describe pending native placements"
        );
        assert_eq!(
            accounting.stale_base_for(Point::new(entry.x + 4., entry.y - 2.), current, 2_000),
            None,
            "a readback alias is not a Local physical movement origin"
        );
    }

    #[test]
    fn local_restore_keeps_the_existing_old_park_step_classifier() {
        let park = Point::new(1920., 1080.);
        let entry = Point::new(1200., 2100.);
        let mut accounting = Accounting::default();
        accounting.register(park, entry, 1_000);
        assert_eq!(accounting.base.sample(park), entry);
        assert_eq!(
            accounting.stale_base_for(Point::new(park.x + 4., park.y - 2.), entry, 2_000),
            Some(park)
        );
        assert_eq!(
            accounting.stale_base_for(Point::new(entry.x + 4., entry.y - 2.), entry, 3_000),
            None
        );
    }

    #[test]
    fn remote_swallowed_steps_keep_displaced_origins_after_the_flight_window() {
        let departure = Point::new(500., 2100.);
        let park = Point::new(1920., 1080.);
        let mut accounting = Accounting::default();
        accounting.register(departure, park, 1_000);
        let now_us = 1_000 + crate::stale_step::STALE_WINDOW_US + 1;
        for readback in [park, departure, park] {
            let current = accounting.base.sample(readback);
            assert_eq!(current, park);
            assert_eq!(
                accounting.stale_base_for(Point::new(departure.x - 1., departure.y + 2.), current, now_us),
                Some(departure),
                "Remote callbacks remain swallowed and may still use displaced native coordinates"
            );
        }
    }

    #[test]
    fn rejected_placement_restores_both_readback_and_flight_accounting() {
        let park = Point::new(1920., 1080.);
        let entry = Point::new(1200., 2100.);
        let mut accounting = Accounting::default();
        accounting.register(park, entry, 1_000);
        let rejected = accounting.register(entry, Point::new(500., 200.), 2_000);
        assert!(accounting.cancel(rejected));
        assert_eq!(accounting.base.sample(park), entry);
        assert_eq!(
            accounting.stale_base_for(Point::new(park.x + 4., park.y - 2.), entry, 3_000),
            Some(park)
        );
    }

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
