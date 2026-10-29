//! A held keyboard-source key opens ordinary edges, never predicts game position.
use super::*;

impl Router {
    /// Any driving keyboard can enable readback for this mouse source in game mode.
    pub fn game_escape_readback(&self, source: DeviceId) -> bool {
        self.has_pointer_focus()
            && self.game_mode().active
            && self.input_ready(source)
            && self.pointer_lock_escape_held()
    }

    /// One fresh native sample is consumed by one relative packet. A timeout,
    /// failed read or key release cannot reuse a previously observed edge.
    pub fn reconcile_game_escape_snapshot(
        &mut self,
        source: DeviceId,
        position: Option<Point>,
        clip: Option<Option<Rect>>,
    ) {
        self.game.escape_cursor = None;
        if !self.game_escape_readback(source) {
            return;
        }
        if let Some(clip) = clip {
            self.update_system_cursor_clip(clip);
        }
        if let Some(pos) = position.filter(|p| p.x.is_finite() && p.y.is_finite())
            && let Some((key, bounds)) = self.locate(pos)
        {
            let at = ScreenPos::new(ScreenId::new(self.me(), key), bounds.clamp(pos));
            if !self.hidden_entry(self.me(), &at.screen.display) {
                self.game.escape_cursor = Some((source, at.clone()));
                if self.gap.is_none() {
                    self.pointer = Some(at.clone());
                    self.local_cursor = Some(at);
                }
            }
        }
    }

    /// Project only an observed outward boundary onto its monitor edge. The
    /// right/bottom Windows pixels are exclusive ClipCursor bounds minus one.
    fn escape_edge(&self, actual: &ScreenPos, delta: (f64, f64)) -> Option<ScreenPos> {
        let bounds = self.displays.iter().find(|d| d.key == actual.screen.display)?.bounds;
        let clip = self.cursor_clip.rect.filter(|clip| *clip != bounds);
        if clip.is_some_and(|clip| !cursor_clip::passive_clip(clip, bounds)) {
            return None;
        }
        let edge = clip.unwrap_or(bounds);
        let p = actual.local;
        // No widening of an ordinary application confinement, and no inferred
        // edge from a game's raw relative delta while its cursor is centered.
        let left = delta.0 < 0. && p.x <= edge.x;
        let right = delta.0 > 0. && p.x >= edge.right() - 1.;
        let top = delta.1 < 0. && p.y <= edge.y;
        let bottom = delta.1 > 0. && p.y >= edge.bottom() - 1.;
        if !(left || right || top || bottom) {
            return None;
        }
        Some(ScreenPos::new(
            actual.screen.clone(),
            bounds.clamp(Point::new(
                if left {
                    bounds.x
                } else if right {
                    bounds.right()
                } else {
                    p.x
                },
                if top {
                    bounds.y
                } else if bottom {
                    bounds.bottom()
                } else {
                    p.y
                },
            )),
        ))
    }

    /// Once ordinary gap travel owns the pointer, its parked OS cursor is ours,
    /// not game evidence. Continue the same virtual path in native physical units.
    pub(super) fn escape_gap_motion(
        &mut self,
        origin: DeviceId,
        from: &ScreenPos,
        delta: (f64, f64),
        now: Micros,
    ) -> Option<Disposition> {
        let topology = self.layout.gap_topology()?;
        let id = oink_display::layout::ScreenId::new(from.screen.device, &from.screen.display);
        let screen = topology.screen(&id)?;
        let (sx, sy) = screen.physical_scale();
        let gain = topology.pointer_gain(&id, origin)?;
        self.gap_motion(from, (delta.0 * sx / gain, delta.1 * sy / gain), origin, now)
    }

    /// The normal topology, gap and handoff gates decide the crossing. Holding
    /// escape skips all damping, so there is deliberately no extra meter here.
    pub(super) fn escape_cursor_motion(
        &mut self,
        origin: DeviceId,
        actual: &ScreenPos,
        delta: (f64, f64),
        now: Micros,
    ) -> bool {
        if !self.pointer_lock_escape_held() || !delta.0.is_finite() || !delta.1.is_finite() {
            return false;
        }
        self.last_pointer_source = Some(origin);
        if self.leave_blocked() {
            return false;
        }
        let Some(edge) = self.escape_edge(actual, delta) else {
            return false;
        };
        self.resistance = None;
        if self.gap_allowed() && self.escape_gap_motion(origin, &edge, delta, now).is_some() {
            return true;
        }
        let Some(dst) = self.layout.exit_through_edge(&edge, delta) else {
            return false;
        };
        if dst.screen.device == self.me() || !self.input_ready(dst.screen.device) {
            return false;
        }
        self.pointer = Some(actual.clone());
        self.hand_off(dst, self.trail_pointer());
        true
    }
}
