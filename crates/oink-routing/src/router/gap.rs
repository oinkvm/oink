//! Optional physical travel between devices. Focus stays on the leaving screen;
//! only the trail uses the virtual, off-screen head. No transport state is added.
use super::*;
use oink_display::layout::{ScreenPlacement, Side};

#[derive(Debug)]
pub(super) struct GapTravel {
    source: ScreenPlacement,
    head: Point,
    gain: f64,
    zone: Option<ZoneTravel>,
}

/// A near miss can meet the target's normal face just outside its rectangle.
/// Keep the virtual path in physical space; only acquisition snaps the tangent.
#[derive(Clone, Debug)]
struct ZoneTravel {
    target: ScreenPlacement,
    side: Side,
    along: (f64, f64),
    entry: Point,
}
impl ZoneTravel {
    fn entry_time(&self, p: Point, d: (f64, f64)) -> Option<f64> {
        let r = self.target.rect;
        let (start, delta, face, outward) = match self.side {
            Side::Right => (p.x, d.0, r.x, d.0 > 0.),
            Side::Left => (p.x, d.0, r.right(), d.0 < 0.),
            Side::Bottom => (p.y, d.1, r.y, d.1 > 0.),
            Side::Top => (p.y, d.1, r.bottom(), d.1 < 0.),
        };
        if !outward {
            return None;
        }
        let t = (face - start) / delta;
        let hit = moved(p, d, t);
        let along = if self.side.is_vertical() { hit.y } else { hit.x };
        ((0. ..=1.).contains(&t) && (self.along.0..=self.along.1).contains(&along)).then_some(t)
    }
}

/// Only the machine that drops an event keeps this bookkeeping. Modifiers
/// differ from ordinary keys: their physical state is reconciled on return.
#[derive(Debug, Default)]
pub(super) struct GapInput {
    local_dropped: HashSet<HeldItem>,
    remote_dropped: HashSet<(DeviceId, HeldItem)>,
    pub(super) modifiers: HashMap<(DeviceId, KeyCode), DeviceId>,
}

impl GapTravel {
    fn position(&self) -> ScreenPos {
        screen_position(&self.source, self.head)
    }

    fn trail_head(&self) -> Point {
        self.source.plane_to_local(self.head)
    }
}

/// Ray entry into the rectangle's interior. Tangency does not acquire focus;
/// reaching an edge at the end of a step does, if motion points inward.
fn entry_time(r: Rect, p: Point, d: (f64, f64)) -> Option<f64> {
    let (mut near, mut far) = (f64::NEG_INFINITY, f64::INFINITY);
    for (p, d, lo, hi) in [(p.x, d.0, r.x, r.right()), (p.y, d.1, r.y, r.bottom())] {
        if d == 0. {
            if p <= lo || p >= hi {
                return None;
            }
        } else {
            let (a, b) = ((lo - p) / d, (hi - p) / d);
            near = near.max(a.min(b));
            far = far.min(a.max(b));
        }
    }
    let t = near.max(0.);
    (t <= 1. && far > t).then_some(t)
}

fn exit_time(r: Rect, p: Point, d: (f64, f64)) -> f64 {
    let mut t = f64::INFINITY;
    for (p, d, lo, hi) in [(p.x, d.0, r.x, r.right()), (p.y, d.1, r.y, r.bottom())] {
        if d > 0. {
            t = t.min((hi - p) / d);
        }
        if d < 0. {
            t = t.min((lo - p) / d);
        }
    }
    t.max(0.)
}

fn moved(p: Point, d: (f64, f64), t: f64) -> Point {
    Point::new(p.x + d.0 * t, p.y + d.1 * t)
}

fn screen_position(s: &ScreenPlacement, p: Point) -> ScreenPos {
    ScreenPos::new(
        ScreenId::new(s.id.device, s.id.display.clone()),
        s.local.clamp(s.plane_to_local(p)),
    )
}

/// Split at outer walls before sliding, rather than drawing a shortcut from
/// start to a clamped endpoint (which could hit the wrong screen).
fn plane_motion(
    p: Point,
    d: (f64, f64),
    bounds: Rect,
    screens: &[ScreenPlacement],
    leaving: Option<&ScreenPlacement>,
) -> (Point, Option<ScreenPlacement>, bool) {
    let (p, target, departed, _) = plane_motion_zone(p, d, bounds, screens, leaving, None);
    (p, target, departed)
}

fn plane_motion_zone(
    mut p: Point,
    mut d: (f64, f64),
    bounds: Rect,
    screens: &[ScreenPlacement],
    mut leaving: Option<&ScreenPlacement>,
    zone: Option<&ZoneTravel>,
) -> (Point, Option<ScreenPlacement>, bool, bool) {
    let mut departed = leaving.is_none();
    for _ in 0..3 {
        if d == (0., 0.) {
            break;
        }
        let t = exit_time(bounds, p, d).min(1.);
        let segment = (d.0 * t, d.1 * t);
        let end = moved(p, d, t);
        if segment != (0., 0.) {
            let first = leaving.map_or(0., |s| exit_time(s.rect, p, segment));
            if first <= 1. {
                let exit = moved(p, segment, first);
                // At the outer wall there is no empty space beyond this edge.
                let can_leave = leaving.is_none_or(|s| !s.rect.contains(bounds.clamp(moved(exit, d, 1e-7))));
                if can_leave {
                    let hit = screens
                        .iter()
                        .filter(|s| leaving.is_none_or(|old| old.id != s.id))
                        .filter_map(|s| {
                            entry_time(s.rect, exit, segment)
                                .filter(|v| *v <= 1. - first + 1e-9)
                                .map(|v| (first + v, s))
                        })
                        .min_by(|(a, sa), (b, sb)| a.total_cmp(b).then_with(|| sa.id.cmp(&sb.id)));
                    let zone_hit = zone.and_then(|z| z.entry_time(p, segment).map(|t| (t, z)));
                    if let Some((t, z)) = zone_hit
                        && hit.as_ref().is_none_or(|(normal, _)| t < *normal)
                    {
                        return (z.entry, Some(z.target.clone()), true, true);
                    }
                    if let Some((t, s)) = hit {
                        return (moved(p, segment, t), Some(s.clone()), true, false);
                    }
                    leaving = None;
                    departed = true;
                }
            }
        }
        p = end;
        if t >= 1. {
            break;
        }
        d = (d.0 * (1. - t), d.1 * (1. - t));
        if (p.x <= bounds.x && d.0 < 0.) || (p.x >= bounds.right() && d.0 > 0.) {
            d.0 = 0.;
        }
        if (p.y <= bounds.y && d.1 < 0.) || (p.y >= bounds.bottom() && d.1 > 0.) {
            d.1 = 0.;
        }
    }
    (p, None, departed, false)
}

impl Router {
    pub(super) fn gap_allowed(&self) -> bool {
        self.holder == self.me()
            && !self.leave_blocked()
            && !self.pointer_buttons_held()
            && self.dnd_state_name() == "idle"
            && self.layout.gap_topology().is_some_and(|t| t.settings().gap_travel)
    }

    pub(super) fn gap_sync(&mut self) {
        if self.gap.is_some() && !self.gap_allowed() {
            self.gap_cancel();
        }
    }

    /// Screen focus, as opposed to the group holder that computes gap travel.
    pub fn has_pointer_focus(&self) -> bool {
        self.holder == self.me() && self.gap.is_none()
    }

    /// Still the group holder, but no screen owns the virtual pointer yet.
    pub fn pointer_in_gap(&self) -> bool {
        self.gap.is_some()
    }

    /// Resume at the safe projection, or a remaining display if the source was
    /// removed. Presses dropped in the gap remain dropped until their release.
    pub(super) fn gap_cancel(&mut self) {
        if self.gap.take().is_none() {
            return;
        }
        self.diagnostic(
            DecisionKind::GapReturn,
            DecisionReason::GapTravel,
            self.pointer.as_ref(),
        );
        if self.holder == self.me() {
            self.local_mouse_hidden = false;
            let position = self
                .pointer
                .as_ref()
                .and_then(|p| {
                    self.displays
                        .iter()
                        .find(|d| d.key == p.screen.display)
                        .map(|d| ScreenPos::new(p.screen.clone(), d.bounds.clamp(p.local)))
                })
                .or_else(|| {
                    self.displays
                        .first()
                        .map(|d| ScreenPos::new(ScreenId::new(self.me(), d.key.clone()), d.bounds.center()))
                });
            if let Some(p) = position {
                self.out.push(Output::RestoreLocal { at: p.local });
                self.pointer = Some(p);
            } else {
                self.out.push(Output::SetMode(CaptureMode::Local));
            }
            self.gap_resume_modifiers();
        } else {
            self.gap_input.modifiers.clear();
        }
    }

    pub(super) fn gap_resume_modifiers(&mut self) {
        for ((origin, code), from) in std::mem::take(&mut self.gap_input.modifiers) {
            self.target_key(origin, code, true);
            if self.is_held(origin, HeldItem::Key(code)) {
                self.press_routes
                    .entry((origin, HeldItem::Key(code)))
                    .or_default()
                    .insert(from);
            }
        }
        // Includes locally held modifiers whose down was swallowed in the gap.
        self.retarget_modifiers();
    }

    pub(super) fn gap_peer_lost(&mut self, peer: DeviceId) {
        self.gap_input.remote_dropped.retain(|(origin, _)| *origin != peer);
        self.gap_input
            .modifiers
            .retain(|(origin, _), from| *origin != peer && *from != peer);
    }

    /// Filter before hotkeys and before normal local event dispatch. Existing
    /// releases keep their recorded targets, including Local while frozen.
    pub(super) fn gap_local_event(&mut self, event: &InputEvent) -> Option<Disposition> {
        let item = match *event {
            InputEvent::Key { code, pressed } => Some((HeldItem::Key(code), pressed)),
            InputEvent::Button { button, pressed } => Some((HeldItem::Button(button), pressed)),
            _ => None,
        };
        if let Some((item, pressed)) = item {
            if self.gap_input.local_dropped.contains(&item) {
                if !pressed {
                    self.gap_input.local_dropped.remove(&item);
                    match item {
                        HeldItem::Key(code) => {
                            self.keys.remove(&code);
                        }
                        HeldItem::Button(button) => {
                            self.buttons.remove(&button);
                        }
                    }
                }
                return Some(Disposition::Swallow);
            }
            self.gap.as_ref()?;
            if pressed {
                match item {
                    HeldItem::Key(code) => {
                        if let std::collections::hash_map::Entry::Vacant(e) = self.keys.entry(code) {
                            e.insert(vec![Target::Dropped]);
                            if !code.is_modifier() {
                                self.gap_input.local_dropped.insert(item);
                            }
                        }
                    }
                    HeldItem::Button(button) => {
                        if let std::collections::hash_map::Entry::Vacant(e) = self.buttons.entry(button) {
                            e.insert(Target::Dropped);
                            self.gap_input.local_dropped.insert(item);
                        }
                    }
                }
                return Some(Disposition::Swallow);
            }
            let held = match item {
                HeldItem::Key(code) => self.keys.contains_key(&code),
                HeldItem::Button(button) => self.buttons.contains_key(&button),
            };
            return (!held).then_some(Disposition::Swallow);
        }
        (self.gap.is_some() && !matches!(event, InputEvent::PointerDelta { .. })).then_some(Disposition::Swallow)
    }

    /// Forwarded messages are unwrapped by on_input before this filter. The
    /// source keeps routing releases to this machine, so no wire ledger is needed.
    pub(super) fn gap_remote_event(&mut self, from: DeviceId, origin: DeviceId, msg: &InputMsg) -> bool {
        let item = match *msg {
            InputMsg::Key { code, pressed } | InputMsg::Volume { code, pressed, .. } => {
                Some((HeldItem::Key(code), pressed))
            }
            InputMsg::CapsKey { pressed, .. } => Some((HeldItem::Key(KeyCode::CAPS_LOCK), pressed)),
            InputMsg::Button { button, pressed } => Some((HeldItem::Button(button), pressed)),
            _ => None,
        };
        if let Some((item, pressed)) = item {
            if self.gap_input.remote_dropped.contains(&(origin, item)) {
                if !pressed {
                    self.gap_input.remote_dropped.remove(&(origin, item));
                }
                return true;
            }
            if let HeldItem::Key(code) = item
                && !pressed
                && self.gap_input.modifiers.remove(&(origin, code)).is_some()
                && !self.is_held(origin, item)
            {
                return true;
            }
            if self.gap.is_none() {
                return false;
            }
            if !pressed {
                return false;
            } // pre-gap release follows its existing route
            if !self.is_held(origin, item) && !self.forwarded.contains_key(&(origin, item)) {
                match item {
                    HeldItem::Key(code) if code.is_modifier() => {
                        self.gap_input.modifiers.insert((origin, code), from);
                    }
                    _ => {
                        self.gap_input.remote_dropped.insert((origin, item));
                    }
                }
            }
            return true;
        }
        if let InputMsg::HeldSync { keys, buttons } = msg {
            self.gap_input
                .modifiers
                .retain(|(o, code), path| *o != origin || *path != from || keys.contains(code));
            self.gap_input.remote_dropped.retain(|(o, item)| {
                *o != origin
                    || match item {
                        HeldItem::Key(code) => keys.contains(code),
                        HeldItem::Button(button) => buttons.contains(button),
                    }
            });
            return false; // existing releases and peer bookkeeping still run
        }
        if let InputMsg::KeyRepeat { code, .. } = msg
            && self.gap_input.remote_dropped.contains(&(origin, HeldItem::Key(*code)))
        {
            return true;
        }
        self.gap.is_some() && !matches!(msg, InputMsg::Motion { .. } | InputMsg::Forwarded { .. })
    }

    /// In physical-space mode a virtual point must not paint onto an unrelated
    /// screen merely because it happens to occupy that OS desktop coordinate.
    pub fn trail_clip_bounds(&self) -> Option<Rect> {
        if !self.gap_allowed() {
            return None;
        }
        let p = self.pointer.as_ref()?;
        self.displays
            .iter()
            .find(|d| d.key == p.screen.display)
            .map(|d| d.bounds)
    }

    /// The renderer follows this point even when it lies outside a display.
    pub fn trail_pointer(&self) -> Option<Point> {
        if self.gap_allowed()
            && let Some(gap) = &self.gap
        {
            Some(gap.trail_head())
        } else {
            self.local_pointer()
        }
    }

    /// Local deltas are already accelerated screen units. Convert using the
    /// leaving screen's physical axes, cancelling its hand-to-plane gain.
    pub(super) fn gap_local_motion(
        &mut self,
        from: &ScreenPos,
        delta: (f64, f64),
        cursor: Option<Point>,
        now: Micros,
    ) -> Option<Disposition> {
        if self.gap.is_none()
            && cursor
                .and_then(|p| self.locate(p))
                .is_some_and(|(key, _)| key != from.screen.display)
        {
            return None; // The OS already completed a same-device crossing.
        }
        let t = self.layout.gap_topology()?;
        let id = oink_display::layout::ScreenId::new(from.screen.device, from.screen.display.clone());
        let s = t.screen(&id)?;
        let gain = t.plane_gain(&id)?;
        let (x, y) = s.physical_scale();
        self.gap_motion(from, (delta.0 * x / gain, delta.1 * y / gain), self.me(), now)
    }

    /// Pass keeps native edge pressure live; a remote Pass is injected as a wall
    /// push by the caller. Swallow means travel consumed the event.
    pub(super) fn gap_motion(
        &mut self,
        from: &ScreenPos,
        hand: (f64, f64),
        origin: DeviceId,
        now: Micros,
    ) -> Option<Disposition> {
        self.gap_sync();
        if !self.gap_allowed() || !hand.0.is_finite() || !hand.1.is_finite() {
            return None;
        }
        let t = self.layout.gap_topology().unwrap();
        let started = self.gap.is_none();
        let (source, head, gain) = if let Some(gap) = &self.gap {
            (&gap.source, gap.head, gap.gain)
        } else {
            let id = oink_display::layout::ScreenId::new(from.screen.device, from.screen.display.clone());
            if !t.is_active(&id) {
                return None;
            }
            let source = t.screen(&id)?;
            let gain = t.plane_gain(&id).unwrap();
            let head = source.local_to_plane(from.local);
            (&source.placement, head, gain)
        };
        let delta = (hand.0 * gain, hand.1 * gain);
        if !delta.0.is_finite() || !delta.1.is_finite() {
            return None;
        }
        if started && source.rect.contains(moved(head, delta, 1.)) {
            return None;
        }
        let source = source.clone();
        let zone = if let Some(gap) = &self.gap {
            gap.zone.clone()
        } else {
            // first_portal also slides along walls; only the actual departure
            // point may select a zone for physical travel.
            self.layout
                .first_portal(from, hand)
                .filter(|exit| {
                    exit.trigger_zone && {
                        let hit = source.local_to_plane(exit.from.local);
                        let actual = moved(head, delta, exit_time(source.rect, head, delta));
                        (hit.x - actual.x).hypot(hit.y - actual.y) < 1e-4
                    }
                })
                .and_then(|exit| {
                    let target = t.screen(&exit.to.screen)?.placement.clone();
                    self.input_ready(target.id.device).then(|| ZoneTravel {
                        entry: target.local_to_plane(exit.to.local),
                        target,
                        side: exit.side,
                        along: exit.trigger_along.unwrap(),
                    })
                })
        }
        .filter(|z| t.is_active(&z.target.id) && self.input_ready(z.target.id.device));
        let mut bounds: Option<Rect> = None;
        for s in t.screens().iter().filter(|s| s.enabled) {
            bounds = Some(match bounds {
                None => s.rect,
                Some(b) => {
                    let (x, y) = (b.x.min(s.rect.x), b.y.min(s.rect.y));
                    Rect::new(
                        x,
                        y,
                        b.right().max(s.rect.right()) - x,
                        b.bottom().max(s.rect.bottom()) - y,
                    )
                }
            });
        }
        let bounds = bounds?;
        let screens: Vec<_> = t
            .screens()
            .iter()
            .filter(|s| t.is_active(&s.id) && self.input_ready(s.id.device))
            .map(|s| s.placement.clone())
            .collect();
        let (head, target, departed, through_zone) = if zone.is_some() {
            plane_motion_zone(head, delta, bounds, &screens, started.then_some(&source), zone.as_ref())
        } else {
            let (head, target, departed) = plane_motion(head, delta, bounds, &screens, started.then_some(&source));
            (head, target, departed, false)
        };
        if !departed {
            return None;
        }
        if started {
            // Hiding/freezing the pointer here would stop the OS's Dock/hot-corner
            // edge pressure. Use the existing portal damping first: native moves
            // pass and remote moves keep their wall deltas until it crosses.
            // That wall delivery was verified on macOS 26/14 ();
            // gap departure parity is covered by the simulator.
            let at = screen_position(&source, moved(source.local_to_plane(from.local), delta, 1.));
            let local_delta = (
                delta.0 * source.local.w / source.rect.w,
                delta.1 * source.local.h / source.rect.h,
            );
            if !self.push_gap_resistance(origin, &at, from, local_delta, now) {
                self.diagnostic(DecisionKind::Refused, DecisionReason::Resistance, Some(&at));
                self.pointer = Some(at);
                return Some(Disposition::Pass);
            }
        }
        if started {
            self.diagnostic(DecisionKind::GapDepart, DecisionReason::GapTravel, Some(from));
        }
        self.gap = Some(GapTravel {
            source: source.clone(),
            head,
            gain,
            zone,
        });
        self.resistance = None;
        self.pointer = Some(self.gap.as_ref().unwrap().position());
        if let Some(target) = target {
            let dst = screen_position(&target, head);
            if target.id.device == self.me() {
                if dst.screen != self.pointer.as_ref().unwrap().screen {
                    self.trail_handoff(&dst, self.epoch, self.trail_pointer());
                }
                self.pointer = Some(dst);
                self.gap_cancel();
            } else {
                if let Some(d) = self.diagnostics.as_ref().filter(|d| d.enabled()) {
                    d.crossing_with_zone(
                        self.layout.clone(),
                        self.epoch + 1,
                        from,
                        Point::new(hand.0, hand.1),
                        false,
                        true,
                        through_zone,
                        &dst,
                        self.departure_evidence(from),
                    );
                }
                self.hand_off(dst, self.trail_pointer());
            }
        } else if started {
            // No screen owns input in the gap, although the group holder remains
            // this device. Returning to it must not revive an interrupted pan.
            self.drop_scroll_remainder();
            self.local_cursor = self.pointer.clone();
            self.local_mouse_hidden = self.mouse_local();
            self.out.push(Output::SetMode(CaptureMode::Remote {
                park: self.park_point(),
            }));
        }
        Some(Disposition::Swallow)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oink_display::layout::{Layout, SizeSource, SpeedMode, Topology};

    fn screen(device: u8, rect: Rect) -> ScreenPlacement {
        ScreenPlacement {
            id: oink_display::layout::ScreenId::new(DeviceId([device; 32]), "screen"),
            name: "screen".into(),
            local: Rect::new(0., 0., 1000., 800.),
            rect,
            size_source: SizeSource::Manual,
            primary: true,
            builtin: false,
        }
    }

    #[test]
    fn all_normals_use_leaving_scale_in_both_speed_modes() {
        for mode in [SpeedMode::Physical, SpeedMode::ScreenProportional] {
            for (start, delta, bounds, expected) in [
                (
                    Point::new(249., 100.),
                    (11., 0.),
                    Rect::new(0., 0., 550., 200.),
                    Point::new(260., 100.),
                ),
                (
                    Point::new(1., 100.),
                    (-11., 0.),
                    Rect::new(-300., 0., 550., 200.),
                    Point::new(-10., 100.),
                ),
                (
                    Point::new(125., 199.),
                    (0., 11.),
                    Rect::new(0., 0., 250., 450.),
                    Point::new(125., 210.),
                ),
                (
                    Point::new(125., 1.),
                    (0., -11.),
                    Rect::new(0., -250., 250., 450.),
                    Point::new(125., -10.),
                ),
            ] {
                let source = screen(1, Rect::new(0., 0., 250., 200.));
                let layout = Layout {
                    screens: vec![source.clone()],
                    ..Layout::default()
                };
                let owners = std::collections::BTreeMap::from([(
                    source.id.device,
                    oink_display::layout::OwnerLayoutPreferences {
                        device_gain: 2.3,
                        speed_mode: mode,
                        ..Default::default()
                    },
                )]);
                let gain = Topology::new(&layout, &owners).plane_gain(&source.id).unwrap();
                let hand = (delta.0 / gain, delta.1 / gain);
                let (p, target, departed) = plane_motion(
                    start,
                    (hand.0 * gain, hand.1 * gain),
                    bounds,
                    std::slice::from_ref(&source),
                    Some(&source),
                );
                assert!(departed && target.is_none());
                assert!((p.x - expected.x).abs() < 1e-8 && (p.y - expected.y).abs() < 1e-8);
            }
        }
    }

    #[test]
    fn wall_slide_uses_the_path_not_the_clamped_endpoint_chord() {
        let target = screen(2, Rect::new(90., 100., 10., 40.));
        let (p, hit, _) = plane_motion(
            Point::new(50., 50.),
            (100., 100.),
            Rect::new(0., 0., 100., 200.),
            &[target],
            None,
        );
        // It touches the target corner at (100,100), then slides tangentially.
        // A shortcut to (100,150) would wrongly enter its interior.
        assert!(hit.is_none());
        assert_eq!(p, Point::new(100., 150.));
    }

    #[test]
    fn grazing_corner_does_not_enter_and_equal_hits_use_stable_ids() {
        let a = screen(1, Rect::new(100., 100., 100., 100.));
        let b = screen(2, a.rect);
        assert!(entry_time(a.rect, Point::new(0., 200.), (200., -200.)).is_none());
        let (_, hit, _) = plane_motion(
            Point::new(0., 150.),
            (500., 0.),
            Rect::new(0., 0., 600., 600.),
            &[b, a.clone()],
            None,
        );
        assert_eq!(hit.unwrap().id, a.id);
    }
    #[test]
    fn fractional_wall_intersections_keep_the_full_tangential_remainder() {
        let bounds = Rect::new(-13.2, 7.6, 100.3, 201.7);
        let p = Point::new(41.3, 29.9);
        for dx in [-371.7, -137.1, 211.3, 458.9] {
            for dy in [-479.3, -161.2, 131.7, 297.2] {
                let (at, hit, _) = plane_motion(p, (dx, dy), bounds, &[], None);
                assert!(hit.is_none());
                let expected = Point::new(
                    (p.x + dx).clamp(bounds.x, bounds.right()),
                    (p.y + dy).clamp(bounds.y, bounds.bottom()),
                );
                assert!(
                    (at.x - expected.x).abs() < 1e-8 && (at.y - expected.y).abs() < 1e-8,
                    "{at:?} != {expected:?}"
                );
            }
        }
    }
}
