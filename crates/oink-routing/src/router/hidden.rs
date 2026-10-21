//! Hidden screens retain geometry and focus; their edges are native walls too.
use super::*;
impl Router {
    /// The native screen containing the visible local pointer. Confinement is
    /// suspended with capture, game raw mode or remote cursor ownership.
    pub fn hidden_cursor_constraint(&self) -> Option<Rect> {
        if !self.capture_available
            || self.paused
            || self.fully_paused
            || self.game_mode().active
            || self.local_mouse_hidden
            || (self.holder != self.me() && !self.mouse_local())
        {
            return None;
        }
        let topology = self.layout.gap_topology()?;
        if !topology
            .screens()
            .iter()
            .any(|s| s.id.device == self.me() && !s.enabled)
        {
            return None;
        }
        let at = if self.mouse_local() {
            self.local_cursor.as_ref()
        } else {
            self.pointer.as_ref()
        }?;
        self.displays
            .iter()
            .find(|d| d.key == at.screen.display)
            .map(|d| d.bounds)
    }
    pub fn constrained_local_cursor(&self) -> Option<Point> {
        self.hidden_cursor_constraint()?;
        if self.mouse_local() {
            self.local_cursor.as_ref()
        } else {
            self.pointer.as_ref()
        }
        .map(|p| p.local)
    }
    pub(super) fn sync_hidden_constraint(&mut self) {
        let next = self.hidden_cursor_constraint();
        if next != self.native_constraint {
            self.native_constraint = next;
            // A return first releases the remote raw-capture lease and restores
            // its explicit entry point. Ordinary moves require the new bounds
            // first; leaving for remote capture releases our bounds first.
            let after_restore = next
                .and_then(|_| {
                    self.out
                        .iter()
                        .rposition(|o| matches!(o, Output::RestoreLocal { .. } | Output::SetMode(CaptureMode::Local)))
                })
                .map_or(0, |i| i + 1);
            self.out.insert(after_restore, Output::ConstrainCursor(next));
        }
    }
    pub(super) fn hidden_local_motion(&mut self, dx: f64, dy: f64, now: Micros) -> Option<Disposition> {
        self.hidden_cursor_constraint()?;
        let from = if self.mouse_local() {
            self.local_cursor.as_ref()
        } else {
            self.pointer.as_ref()
        }?
        .clone();
        let topology = self.layout.gap_topology()?;
        let id = oink_display::layout::ScreenId::new(from.screen.device, &from.screen.display);
        let scale = topology.reference_per_native_unit(&id)?;
        let step = topology.step_filtered(
            &oink_display::layout::ScreenPos {
                screen: id,
                local: from.local,
            },
            (dx * scale, dy * scale),
            |device| *device == self.me(),
        )?;
        let candidate = step.to.local;
        // The ordinary path still owns cross-device edge pressure, locks, game
        // policy and gap travel. Only native same-device movement is virtualized.
        let before = self.out.len();
        let result = self.local_motion_unconstrained(dx, dy, Some(candidate), now);
        if result == Disposition::Pass
            && !self.system_clip_recheck_pending()
            && let Some(at) = self.constrained_local_cursor()
            && !self.out[before..].iter().any(|o| {
                matches!(
                    o,
                    Output::Inject(InjectEvent::MoveTo { .. } | InjectEvent::MoveToWithDelta { .. })
                        | Output::RestoreLocal { .. }
                )
            })
        {
            self.out
                .push(Output::Inject(InjectEvent::MoveToWithDelta { pos: at, dx, dy }));
        }
        Some(Disposition::Swallow)
    }
    pub(super) fn hidden_device(&self, device: DeviceId) -> bool {
        self.layout.gap_topology().is_some_and(|t| {
            let mut screens = t.screens().iter().filter(|s| s.id.device == device).peekable();
            screens.peek().is_some() && screens.all(|s| !s.enabled)
        })
    }
    pub(super) fn hidden_entry(&self, device: DeviceId, display: &str) -> bool {
        self.layout
            .gap_topology()
            .and_then(|t| t.screen(&oink_display::layout::ScreenId::new(device, display)))
            .is_some_and(|s| !s.enabled)
    }
}
