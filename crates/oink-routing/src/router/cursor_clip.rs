//! Target-owned system confinement. No platform calls or game-mode inference.
use super::*;
#[derive(Default)]
pub(super) struct CursorClip {
    rect: Option<Rect>,
    suppressed: u64,
    recheck_local: bool,
    checked: bool,
    pending: Option<LocalMotion>,
}
struct LocalMotion {
    epoch: u64,
    dx: f64,
    dy: f64,
    cursor: Option<Point>,
    now: Micros,
}

impl Router {
    /// The Windows owner must resolve local edge candidates outside the LL hook.
    pub fn enable_system_clip_recheck(&mut self, enabled: bool) {
        self.cursor_clip.recheck_local = enabled && self.cfg.os == Os::Windows;
        if !self.cursor_clip.recheck_local {
            self.cancel_system_clip_motion();
        }
    }
    pub(super) fn cancel_system_clip_motion(&mut self) {
        self.cursor_clip.pending = None;
    }
    pub fn system_clip_recheck_pending(&self) -> bool {
        self.cursor_clip.pending.is_some()
    }
    /// Call only after a successful fresh native snapshot, on the ordinary loop.
    pub fn resume_system_clip_motion(&mut self) {
        let Some(motion) = self.cursor_clip.pending.take() else {
            return;
        };
        if self.epoch != motion.epoch || self.holder != self.me() || self.paused {
            return;
        }
        self.cursor_clip.checked = true;
        self.local_motion(motion.dx, motion.dy, motion.cursor, motion.now);
        self.cursor_clip.checked = false;
    }
    pub(super) fn defer_system_clip_motion(&mut self, dx: f64, dy: f64, cursor: Option<Point>, now: Micros) -> bool {
        if !self.cursor_clip.recheck_local || self.cursor_clip.checked {
            return false;
        }
        let edge = cursor.is_some_and(|p| {
            self.locate(p).is_some_and(|(key, b)| {
                p.x <= b.x + EDGE_UNITS
                    || p.y <= b.y + EDGE_UNITS
                    || p.x >= b.right() - EDGE_UNITS
                    || p.y >= b.bottom() - EDGE_UNITS
                    || self.pointer.as_ref().is_some_and(|from| from.screen.display != key)
            })
        });
        if !edge && !self.system_clip_active() && self.gap.is_none() && self.cursor_clip.pending.is_none() {
            return false;
        }
        // Native movement has already been observed; keep one bounded net motion
        // and the latest OS point until the next ordinary loop turn. No injection
        // or OS query is needed in the callback. Explicit focus changes/escape
        // invalidate this candidate by epoch/holder before it can be replayed.
        if let Some(pending) = self.cursor_clip.pending.as_mut().filter(|p| p.epoch == self.epoch) {
            pending.dx += dx;
            pending.dy += dy;
            pending.cursor = cursor;
            pending.now = now;
        } else {
            self.cursor_clip.pending = Some(LocalMotion {
                epoch: self.epoch,
                dx,
                dy,
                cursor,
                now,
            });
        }
        true
    }

    /// A fresh target-side observation. Only Windows exposes this constraint;
    /// the backend has already excluded its explicitly owned source clip.
    fn update_system_cursor_clip(&mut self, clip: Option<Rect>) {
        let clip = clip.filter(|r| {
            [r.x, r.y, r.w, r.h, r.right(), r.bottom()]
                .into_iter()
                .all(f64::is_finite)
                && r.w >= 1.
                && r.h >= 1.
        });
        if self.cursor_clip.rect != clip {
            self.cursor_clip.rect = clip;
            self.resistance = None;
            if self.system_clip_active() {
                self.gap_cancel();
            }
        }
    }
    /// Install a single native readback's constraint before adopting its base.
    /// Do not emit a clamp of the obsolete predicted point ahead of the rebase.
    pub fn reconcile_target_snapshot(&mut self, position: Option<Point>, clip: Option<Option<Rect>>) {
        if let Some(clip) = clip {
            self.update_system_cursor_clip(clip);
        }
        if let Some(position) = position {
            self.reconcile_target_cursor(position);
        }
        self.clamp_system_cursor_clip();
    }
    pub fn set_system_cursor_clip(&mut self, clip: Option<Rect>) {
        self.update_system_cursor_clip(clip);
        self.clamp_system_cursor_clip();
    }
    fn clamp_system_cursor_clip(&mut self) {
        if let Some(at) = self.pointer.as_ref().and_then(|p| self.system_clip_position(p.local))
            && self.pointer.as_ref() != Some(&at)
        {
            self.pointer = Some(at.clone());
            self.local_cursor = Some(at.clone());
            if !self.game_mode().active && self.cursor_clip.pending.is_none() {
                self.out.push(Output::Inject(InjectEvent::MoveTo { pos: at.local }));
            }
        }
    }
    /// Lifetime cumulative candidates suppressed by system confinement.
    pub fn system_cursor_clip_status(&self) -> (Option<Rect>, u64) {
        (self.cursor_clip.rect, self.cursor_clip.suppressed)
    }
    pub(super) fn system_clip_active(&self) -> bool {
        self.holder == self.me() && !self.paused && self.cursor_clip.rect.is_some()
    }
    pub(super) fn system_clip_position(&self, p: Point) -> Option<ScreenPos> {
        if !self.system_clip_active() {
            return None;
        }
        let clip = self.cursor_clip.rect?;
        // Intersect with every actual display, not the group's canvas. A clip
        // may span screens and negative origins, or contain desktop gaps.
        self.displays
            .iter()
            .filter_map(|d| {
                let x = clip.x.max(d.bounds.x);
                let y = clip.y.max(d.bounds.y);
                let right = clip.right().min(d.bounds.right());
                let bottom = clip.bottom().min(d.bounds.bottom());
                if right - x < 1. || bottom - y < 1. {
                    return None;
                }
                let at = Point::new(p.x.clamp(x, right - 1.), p.y.clamp(y, bottom - 1.));
                let distance = (p.x - at.x).hypot(p.y - at.y);
                Some((ScreenPos::new(ScreenId::new(self.me(), &d.key), at), distance))
            })
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(at, _)| at)
    }
    pub(super) fn system_clip_remote_motion(
        &mut self,
        from: &ScreenPos,
        origin: DeviceId,
        dx: f64,
        dy: f64,
        delta: (f64, f64),
    ) -> bool {
        if !self.system_clip_active() {
            return false;
        }
        if self.layout.step_from_source(from, (dx, dy), origin).to.screen.device != self.me() {
            self.cursor_clip.suppressed = self.cursor_clip.suppressed.saturating_add(1);
        }
        if let Some(at) = self.system_clip_position(Point::new(from.local.x + delta.0, from.local.y + delta.1)) {
            self.pointer = Some(at.clone());
            self.local_cursor = Some(at.clone());
            self.out.push(Output::Inject(InjectEvent::MoveToWithDelta {
                pos: at.local,
                dx: delta.0,
                dy: delta.1,
            }));
        }
        self.resistance = None;
        self.dnd_off_edge();
        true
    }
    pub(super) fn system_clip_local_motion(&mut self, dx: f64, dy: f64, cursor: Option<Point>) -> bool {
        if !self.system_clip_active() {
            return false;
        }
        if let Some(from) = self.pointer.as_ref()
            && self
                .layout
                .exit_through_edge(from, (dx, dy))
                .is_some_and(|to| to.screen.device != self.me())
        {
            self.cursor_clip.suppressed = self.cursor_clip.suppressed.saturating_add(1);
        }
        if let Some(at) = cursor.and_then(|p| self.system_clip_position(p)) {
            self.pointer = Some(at.clone());
            self.local_cursor = Some(at);
        }
        self.resistance = None;
        self.dnd_off_edge();
        true
    }
    pub(super) fn constrain_system_clip_outputs(&mut self) {
        if !self.system_clip_active() {
            return;
        }
        if let Some(at) = self.pointer.as_ref().and_then(|p| self.system_clip_position(p.local)) {
            self.pointer = Some(at.clone());
            self.local_cursor = Some(at);
        }
        // FocusEnter, recall/restore and other absolute placements must use the
        // same resolved pixels as the virtual pointer, including the first frame.
        let mut outputs = std::mem::take(&mut self.out);
        for output in &mut outputs {
            let position = match output {
                Output::Inject(InjectEvent::MoveTo { pos } | InjectEvent::MoveToWithDelta { pos, .. }) => pos,
                Output::RestoreLocal { at } => at,
                _ => continue,
            };
            if let Some(at) = self.system_clip_position(*position) {
                *position = at.local;
            }
        }
        self.out = outputs;
    }
}
