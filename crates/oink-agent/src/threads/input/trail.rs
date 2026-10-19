//! Pure emission model owned by the capture thread. Router's synchronous seam probe
//! uses thread-local ownership; it never locks the ordinary window renderer.
use oink_foundation::{Point, Rect, display::DisplayInfo};
use oink_locator::{Effect, Params};
use oink_routing::Output;
use oink_routing::control::TrailContinuation;
use std::{
    cell::RefCell,
    collections::HashMap,
    sync::atomic::{AtomicU64, Ordering},
};

thread_local! { static MODELS: RefCell<HashMap<u64, Model>> = RefCell::default(); }
static NEXT: AtomicU64 = AtomicU64::new(1);
const SEED: u64 = 0x5DEE_CE66_D1CE_4E5B;
struct Model {
    effect: Effect,
    displays: Vec<DisplayInfo>,
    active: bool,
    suppressed: bool,
    at: f64,
}
impl Default for Model {
    fn default() -> Self {
        Self {
            effect: {
                let mut effect = Effect::new(Params::default(), SEED);
                effect.set_decimation(false);
                effect
            },
            displays: Vec::new(),
            active: false,
            suppressed: false,
            at: 0.,
        }
    }
}
impl Model {
    fn cell_at(&mut self, p: Point) {
        if let Some(display) = self
            .displays
            .iter()
            .find(|display| display.bounds.contains(p))
            .or_else(|| self.displays.iter().find(|display| display.is_primary))
            .or_else(|| self.displays.first())
        {
            let (cell, mm) = oink_locator::cell_for(display);
            self.effect.set_cell(cell, mm);
        }
    }
    fn reset(&mut self) {
        self.effect = Effect::new(*self.effect.params(), SEED);
        self.effect.set_decimation(false);
        self.active = false;
    }
}
pub(super) struct Trail(u64);
impl Trail {
    pub(super) fn new() -> Self {
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }
    fn with<T>(&self, run: impl FnOnce(&mut Model) -> T) -> T {
        MODELS.with(|models| run(models.borrow_mut().entry(self.0).or_default()))
    }
    pub(super) fn probe(&self) -> std::sync::Arc<dyn Fn(Point) -> Option<TrailContinuation> + Send + Sync> {
        let id = self.0;
        std::sync::Arc::new(move |exit| {
            MODELS.with(|models| {
                let mut models = models.borrow_mut();
                let model = models.get_mut(&id)?;
                model.effect.push(model.at, exit);
                model.effect.continuation(model.at)
            })
        })
    }
    pub(super) fn duration(&self, ms: u32) {
        self.with(|model| model.effect.set_trail_duration(f64::from(ms) / 1000.));
    }
    pub(super) fn at(&self, at: f64) {
        self.with(|model| model.at = at);
    }
    pub(super) fn displays(&self, displays: Vec<DisplayInfo>) {
        self.with(|model| model.displays = displays);
    }
    pub(super) fn pause(&self) {
        self.with(Model::reset);
    }
    pub(super) fn suppress(&self, hidden: bool) {
        self.with(|model| {
            model.suppressed = hidden;
            model.effect.set_active(model.at, model.active && !hidden);
        });
    }
    pub(super) fn pointer(&self, head: Point, visible: Point, clip: Option<Rect>) {
        self.with(|model| {
            model.effect.finish_if_done(model.at);
            model.cell_at(visible);
            model.effect.set_clip(clip);
            model.effect.push(model.at, head);
        });
    }
    pub(super) fn output(&self, output: &Output) {
        self.with(|model| match output {
            Output::CursorTrail { active } => {
                model.active = *active;
                model.effect.set_active(model.at, *active && !model.suppressed);
            }
            Output::CursorTrailResume(state) | Output::CursorTrailResumeClipped { state, .. } => {
                model.reset();
                if let Some(point) = state.points.last() {
                    model.cell_at(point.pos);
                }
                model.effect.resume(model.at, state);
                model.effect.set_clip(match output {
                    Output::CursorTrailResumeClipped { clip, .. } => Some(*clip),
                    _ => None,
                });
                model.suppressed = false;
                model.active = true;
                model.effect.set_active(model.at, true);
            }
            Output::CursorTrailDepart { .. } => model.reset(),
            _ => {}
        });
    }
}
impl Drop for Trail {
    fn drop(&mut self) {
        MODELS.with(|models| {
            models.borrow_mut().remove(&self.0);
        });
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    #[test]
    fn seam_probe_contains_only_this_session_and_a_fixed_short_tail() {
        let trail = Trail::new();
        trail.duration(10_000);
        for i in 0..=12_000 {
            trail.at(f64::from(i) / 1000.);
            trail.pointer(Point::new(f64::from(i), 0.), Point::new(f64::from(i), 0.), None);
        }
        trail.output(&Output::CursorTrail { active: true });
        let state = trail.probe()(Point::new(12_001., 0.)).unwrap();
        assert!(state.points.iter().all(|p| p.age_secs == 0.));
        for i in 1..=500 {
            trail.at(12. + f64::from(i) / 1000.);
            trail.pointer(Point::new(f64::from(i), 0.), Point::new(f64::from(i), 0.), None);
        }
        let state = trail.probe()(Point::new(501., 0.)).unwrap();
        assert!(state.points.iter().all(|p| p.age_secs <= 0.27));
        trail.output(&Output::CursorTrail { active: false });
        trail.at(12.6);
        trail.pointer(Point::new(700., 0.), Point::new(700., 0.), None);
        trail.output(&Output::CursorTrail { active: true });
        let state = trail.probe()(Point::new(800., 0.)).unwrap();
        assert_eq!(state.points.len(), 1);
        assert_eq!(state.points[0].pos, Point::new(800., 0.));
        assert_eq!(state.emit_carry, 0.);
    }

    #[test]
    fn configured_duration_survives_resets_without_extending_seam_history() {
        let trail = Trail::new();
        trail.duration(10_000);
        trail.output(&Output::CursorTrail { active: true });
        for i in 0..=12_000 {
            trail.at(f64::from(i) / 1000.);
            trail.pointer(Point::new(f64::from(i), 0.), Point::new(f64::from(i), 0.), None);
        }
        let state = trail.probe()(Point::new(12_001., 0.)).unwrap();
        assert!(state.valid());
        assert!(state.points[0].age_secs > 0.25 && state.points[0].age_secs <= 0.27);
        trail.with(|m| assert!(m.effect.ribbon_count(m.at) <= 512));
        trail.output(&Output::CursorTrailDepart {
            exit: Point::new(12_001., 0.),
        });
        trail.output(&Output::CursorTrailResume(state));
        trail.with(|m| assert_eq!(m.effect.params().trail_secs, 10.));
        trail.pause();
        trail.with(|m| assert_eq!(m.effect.params().trail_secs, 10.));
    }

    use super::*;
    #[test]
    fn seam_probe_is_synchronous_and_independent_of_renderer_progress() {
        let trail = Trail::new();
        let probe = trail.probe();
        trail.output(&Output::CursorTrail { active: true });
        for i in 0..10 {
            trail.at(f64::from(i) * 0.01);
            trail.pointer(Point::new(f64::from(i), 20.), Point::new(f64::from(i), 20.), None);
        }
        let state = probe(Point::new(10., 20.)).unwrap();
        assert!(state.valid());
        assert!(state.points.len() > 1);
        assert_eq!(state.points.last().unwrap().pos, Point::new(10., 20.));
        trail.output(&Output::CursorTrailDepart {
            exit: Point::new(10., 20.),
        });
        assert!(probe(Point::new(11., 20.)).is_none());
        trail.output(&Output::CursorTrailResume(state));
        assert!(probe(Point::new(12., 20.)).is_some());
        trail.suppress(true);
        assert!(probe(Point::new(13., 20.)).is_none());
    }
}
