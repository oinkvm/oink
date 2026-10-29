//! Backend-owned desktop toasts. The queue and rasterizer never query input or
//! create native windows; only the optional surface worker touches the desktop.
use oink_foundation::{Point, Rect, display::DisplayInfo};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

mod color;
mod interaction;
pub use interaction::Feedback;
#[cfg(target_os = "macos")]
mod macos;
mod raster;
#[cfg(windows)]
mod windows;
#[cfg(target_os = "macos")]
use macos as platform;
#[cfg(windows)]
use windows as platform;

#[cfg(target_os = "macos")]
pub fn scale_for(_display: &DisplayInfo) -> f64 {
    1.0
}
#[cfg(windows)]
pub fn scale_for(display: &DisplayInfo) -> f64 {
    platform::scale_for(display)
}

pub const LIFETIME: Duration = Duration::from_secs(6);
pub const GAP: f64 = 10.0;
pub const INSET: f64 = 20.0;

#[derive(Clone, Debug, PartialEq)]
pub struct Toast {
    pub title: String,
    pub description: String,
    pub undo: Option<String>,
    pub persistent: bool,
    pub display: DisplayInfo,
    pub scale: f64,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Style {
    pub dark: bool,
    pub accent: [u8; 3],
}
impl Default for Style {
    fn default() -> Self {
        Self {
            dark: false,
            accent: [255, 107, 142],
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Click {
    pub id: u64,
    pub undo: bool,
}
#[derive(Clone, Debug)]
pub struct Frame {
    pub id: u64,
    pub bounds: Rect,
    /// Card rectangle in native units relative to the padded surface.
    pub card: Rect,
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<u32>,
    /// Hit area in native units relative to the top-left of the window.
    pub undo: Option<Rect>,
    pub interactive: bool,
}
impl Frame {
    /// Native-unit hit test of the visible card, including antialiased
    /// edges. Fully transparent corners never owns desktop input.
    pub fn contains(&self, point: Point) -> bool {
        if !self.interactive || point.x < 0. || point.y < 0. || point.x >= self.bounds.w || point.y >= self.bounds.h {
            return false;
        }
        let x = (point.x * self.width as f64 / self.bounds.w) as usize;
        let y = (point.y * self.height as f64 / self.bounds.h) as usize;
        self.pixels[y * self.width + x] >> 24 != 0
    }
}
#[derive(Clone, Debug)]
struct Entry {
    id: u64,
    toast: Toast,
    born: Instant,
    leaving: Option<Instant>,
    button: interaction::ButtonAnimation,
}
#[derive(Debug, Default)]
pub struct Queue {
    entries: Vec<Entry>,
    next: u64,
}
impl Queue {
    pub fn push(&mut self, toast: Toast, now: Instant) -> u64 {
        self.next += 1;
        self.entries.push(Entry {
            id: self.next,
            toast,
            born: now,
            leaving: None,
            button: interaction::ButtonAnimation::default(),
        });
        self.next
    }
    /// Replace a visible notice in its stack slot with a fresh lifetime and ID.
    /// Late clicks on the retired notice cannot act on the replacement.
    pub fn replace(&mut self, id: u64, mut toast: Toast, now: Instant) -> u64 {
        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.id == id) {
            self.next += 1;
            toast.display = entry.toast.display.clone();
            toast.scale = entry.toast.scale;
            *entry = Entry {
                id: self.next,
                toast,
                born: now,
                leaving: None,
                button: interaction::ButtonAnimation::default(),
            };
            self.next
        } else {
            self.push(toast, now)
        }
    }
    pub fn set_displays(&mut self, displays: &[(DisplayInfo, f64)]) {
        let info: Vec<_> = displays.iter().map(|(d, _)| d.clone()).collect();
        for entry in &mut self.entries {
            if let Some(display) = select_display(
                &info,
                Some(&entry.toast.display.key),
                entry.toast.display.bounds.center(),
            ) {
                entry.toast.display = display.clone();
                entry.toast.scale = displays
                    .iter()
                    .find(|(d, _)| d.key == display.key)
                    .expect("selected display")
                    .1;
            }
        }
    }
    fn apply_scales(&mut self, scales: Vec<(u64, f64)>) -> bool {
        let mut changed = false;
        for (id, scale) in scales {
            if !scale.is_finite() || scale < 0.5 {
                continue;
            }
            if let Some(e) = self.entries.iter_mut().find(|e| e.id == id) {
                changed |= e.toast.scale != scale;
                e.toast.scale = scale;
            }
        }
        changed
    }
    /// Retire a state-caused notice immediately, without an exit frame or timer.
    pub fn remove(&mut self, id: u64) {
        self.entries.retain(|entry| entry.id != id);
    }
    pub fn dismiss(&mut self, id: u64, now: Instant) {
        if let Some(e) = self.entries.iter_mut().find(|e| e.id == id) {
            e.leaving.get_or_insert(now);
        }
    }
    pub fn click(&mut self, click: Click, now: Instant) -> Option<Click> {
        let e = self
            .entries
            .iter_mut()
            .find(|e| e.id == click.id && e.leaving.is_none())?;
        e.leaving = Some(now);
        Some(Click {
            undo: click.undo && e.toast.undo.is_some(),
            ..click
        })
    }
    fn feedback(&mut self, feedback: Feedback, now: Instant, reduced: bool) -> bool {
        self.entries
            .iter_mut()
            .find(|e| e.id == feedback.id && e.leaving.is_none())
            .is_some_and(|e| e.button.set(feedback.hovered, feedback.pressed, now, reduced))
    }
    pub fn clear(&mut self) {
        self.entries.clear();
    }
    pub fn tick(&mut self, now: Instant, reduced: bool) {
        for e in &mut self.entries {
            if !e.toast.persistent && now.saturating_duration_since(e.born) >= LIFETIME {
                e.leaving.get_or_insert(e.born + LIFETIME);
            }
        }
        self.entries.retain(|e| {
            e.leaving
                .is_none_or(|at| !reduced && now.saturating_duration_since(at).as_secs_f64() < 0.17)
        });
    }
    pub fn frames(
        &self,
        now: Instant,
        style: Style,
        reduced: bool,
        text: &dyn crate::aim::text::TextRaster,
    ) -> Vec<Frame> {
        let mut offsets = std::collections::HashMap::<String, f64>::new();
        self.entries
            .iter()
            .filter_map(|e| {
                let d = &e.toast.display;
                let scale = e.toast.scale.max(0.5);
                let width = (360.0 * scale).min((d.bounds.w - 32.0 * scale).max(1.0));
                let density = d.pixels_per_unit();
                let layout = raster::layout(&e.toast, width, scale * density, text);
                let height = (layout.height * scale * density).ceil() / density;
                let offset = offsets.entry(d.key.clone()).or_insert(INSET * scale);
                let y = d.bounds.y + *offset;
                *offset += height + GAP * scale;
                if y + height > d.bounds.bottom() {
                    return None;
                }
                let enter = (now.saturating_duration_since(e.born).as_secs_f64() / 0.542).min(1.0);
                let leave = e
                    .leaving
                    .map(|t| (now.saturating_duration_since(t).as_secs_f64() / 0.17).min(1.0));
                let (alpha, dx, dy, shrink) = if reduced {
                    (1.0, 0.0, 0.0, 1.0)
                } else if let Some(t) = leave {
                    {
                        let t = ease(t, 0.55, 0.0, 1.0, 0.45);
                        (1.0 - t, 56.0 * t, 0.0, 1.0 - 0.08 * t)
                    }
                } else {
                    let spring = ease(enter, 0.3, 1.7, 0.5, 1.0);
                    (
                        spring.clamp(0.0, 1.0),
                        0.0,
                        -32.0 * (1.0 - spring),
                        0.84 + 0.16 * spring,
                    )
                };
                // Rasterize at the final device-pixel size, including the motion
                // transform. Resizing a finished glyph mask blurs hinted text.
                let (mut pixels, w, h) = raster::paint(
                    &e.toast,
                    style,
                    &layout,
                    scale * density * shrink,
                    text,
                    e.button.sample(now, reduced),
                );
                if alpha < 1.0 {
                    for p in &mut pixels {
                        let mut out = 0;
                        for shift in [0, 8, 16, 24] {
                            out |= (((((*p >> shift) & 255) as f64) * alpha).round() as u32) << shift;
                        }
                        *p = out;
                    }
                }
                let bounds = Rect::new(
                    d.bounds.right() - INSET * scale - width + dx * scale + width * (1.0 - shrink) / 2.0,
                    y + dy * scale + height * (1.0 - shrink) / 2.0,
                    w as f64 / density,
                    h as f64 / density,
                );
                Some(Frame {
                    id: e.id,
                    bounds,
                    card: Rect::new(0., 0., w as f64 / density, h as f64 / density),
                    width: w,
                    height: h,
                    pixels,
                    undo: e.toast.undo.as_ref().map(|_| {
                        Rect::new(
                            (width - 88.0 * scale) * shrink,
                            (height / 2.0 - 16.0 * scale) * shrink,
                            72.0 * scale * shrink,
                            32.0 * scale * shrink,
                        )
                    }),
                    interactive: e.leaving.is_none(),
                })
            })
            .collect()
    }
}

/// No integer conversion before containment: (-0.2, y) belongs to a display to
/// the left of the origin, including Retina sub-point cursor positions.
pub fn select_display<'a>(
    displays: &'a [DisplayInfo],
    requested: Option<&str>,
    pointer: Point,
) -> Option<&'a DisplayInfo> {
    requested
        .and_then(|key| displays.iter().find(|d| d.key == key))
        .or_else(|| displays.iter().find(|d| d.bounds.contains(pointer)))
        .or_else(|| {
            displays.iter().min_by(|a, b| {
                let distance = |d: &DisplayInfo| {
                    let x = pointer.x.clamp(d.bounds.x, d.bounds.right());
                    let y = pointer.y.clamp(d.bounds.y, d.bounds.bottom());
                    (x - pointer.x).hypot(y - pointer.y)
                };
                distance(a).total_cmp(&distance(b))
            })
        })
}

/// Native implementations are created only on the surface worker, behind this
/// seam. Headless tests never instantiate AppKit windows or desktop HWNDs.
pub trait Surface {
    fn refresh_preferences(&mut self) {}
    fn ready(&self) -> bool {
        true
    }
    fn present(&mut self, frames: Vec<Frame>);
    fn clicks(&mut self) -> Vec<Click>;
    fn reduced_motion(&self) -> bool;
    /// Native DPI changes for existing toasts, even without a geometry change.
    fn scales(&mut self) -> Vec<(u64, f64)> {
        Vec::new()
    }

    /// Drain native hover/press changes. Headless surfaces need no input source.
    fn feedback(&mut self) -> Vec<Feedback> {
        Vec::new()
    }
}
#[derive(Debug, Default)]
struct State {
    queue: Queue,
    style: Style,
    stop: bool,
    revision: u64,
}
type Shared = Arc<(Mutex<State>, Condvar)>;
#[derive(Debug)]
struct Handle(Shared);
impl Drop for Handle {
    fn drop(&mut self) {
        let (m, cv) = &*self.0;
        m.lock().unwrap_or_else(|e| e.into_inner()).stop = true;
        cv.notify_one();
    }
}
#[derive(Clone, Debug)]
pub struct NotificationOverlay(Arc<Handle>);
impl NotificationOverlay {
    pub fn headless() -> Self {
        Self(Arc::new(Handle(Arc::new((
            Mutex::new(State::default()),
            Condvar::new(),
        )))))
    }
    pub fn platform(clicked: impl Fn(Click) + Send + 'static) -> Option<Self> {
        Self::with_surface(
            || platform::Backend::new().map(|s| Box::new(s) as Box<dyn Surface>),
            clicked,
        )
    }
    pub fn with_surface(
        factory: impl FnOnce() -> Option<Box<dyn Surface>> + Send + 'static,
        clicked: impl Fn(Click) + Send + 'static,
    ) -> Option<Self> {
        let this = Self::headless();
        let shared = this.0.0.clone();
        std::thread::Builder::new()
            .name("oink-notifications".into())
            .spawn(move || {
                let Some(mut surface) = factory() else { return };
                let text = crate::aim::text::system_toast();
                let mut last_revision = u64::MAX;
                let mut last_reduced = None;
                let mut was_animated = false;
                let mut preference_revision = u64::MAX;
                let mut visible = false;
                loop {
                    let (m, cv) = &*shared;
                    let mut state = m.lock().unwrap_or_else(|e| e.into_inner());
                    while state.queue.entries.is_empty() && !state.stop {
                        if visible {
                            // Clear before parking, including a clear/remove racing
                            // with the previous unlocked native presentation.
                            drop(state);
                            surface.present(Vec::new());
                            visible = false;
                            state = m.lock().unwrap_or_else(|e| e.into_inner());
                        } else {
                            state = cv.wait(state).unwrap_or_else(|e| e.into_inner());
                        }
                    }
                    if state.stop {
                        if visible {
                            surface.present(Vec::new());
                        }
                        return;
                    }
                    drop(state);
                    // Only live native windows need input/DPI polling. Empty or
                    // fully clipped queues must not enqueue main-thread work.
                    let (events, feedback, scales) = if visible {
                        (surface.clicks(), surface.feedback(), surface.scales())
                    } else {
                        (Vec::new(), Vec::new(), Vec::new())
                    };
                    let mut state = m.lock().unwrap_or_else(|e| e.into_inner());
                    if state.stop || state.queue.entries.is_empty() {
                        continue;
                    }
                    if preference_revision != state.revision {
                        preference_revision = state.revision;
                        surface.refresh_preferences();
                    }
                    if !surface.ready() {
                        drop(
                            cv.wait_timeout(state, Duration::from_millis(16))
                                .unwrap_or_else(|e| e.into_inner()),
                        );
                        continue;
                    }
                    let now = Instant::now();
                    let reduced = surface.reduced_motion();
                    let events: Vec<_> = events.into_iter().filter_map(|e| state.queue.click(e, now)).collect();
                    let old = state.queue.entries.len();
                    state.queue.tick(now, reduced);
                    let mut changed = old != state.queue.entries.len() || last_reduced != Some(reduced);
                    changed |= state.queue.apply_scales(scales);
                    for feedback in feedback {
                        changed |= state.queue.feedback(feedback, now, reduced);
                    }
                    last_reduced = Some(reduced);
                    let animated = !reduced
                        && state.queue.entries.iter().any(|e| {
                            e.leaving.is_some()
                                || e.button.active(now)
                                || now.saturating_duration_since(e.born).as_secs_f64() < 0.662
                        });
                    // Draw the settled endpoint once even if the worker wakes
                    // after a transition's deadline (including a delayed frame).
                    let frames = if changed
                        || animated
                        || was_animated
                        || state.revision != last_revision
                        || !events.is_empty()
                        || state.queue.entries.iter().any(|e| e.leaving.is_some())
                    {
                        Some(state.queue.frames(now, state.style, reduced, &*text))
                    } else {
                        None
                    };
                    was_animated = animated;
                    // Expiry/removal can change the frame even without a caller update.
                    last_revision = state.revision;
                    drop(state);
                    if let Some(frames) = frames {
                        visible = !frames.is_empty();
                        surface.present(frames);
                    }
                    for event in events {
                        clicked(event);
                    }
                    let state = m.lock().unwrap_or_else(|e| e.into_inner());
                    if state.stop || state.revision != last_revision || state.queue.entries.is_empty() {
                        continue;
                    }
                    let wait = if visible {
                        Some(Duration::from_millis(16))
                    } else {
                        // Clipped entries still expire on time, but persistent
                        // invisible entries sleep until a queue/display update.
                        state
                            .queue
                            .entries
                            .iter()
                            .filter_map(|entry| {
                                entry
                                    .leaving
                                    .map(|at| at + Duration::from_millis(170))
                                    .or_else(|| (!entry.toast.persistent).then_some(entry.born + LIFETIME))
                            })
                            .min()
                            .map(|at| at.saturating_duration_since(Instant::now()))
                    };
                    if let Some(wait) = wait {
                        drop(cv.wait_timeout(state, wait).unwrap_or_else(|e| e.into_inner()));
                    } else {
                        drop(cv.wait(state).unwrap_or_else(|e| e.into_inner()));
                    }
                }
            })
            .ok()?;
        Some(this)
    }
    fn update(&self, f: impl FnOnce(&mut State)) {
        let (m, cv) = &*self.0.0;
        let mut s = m.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut s);
        s.revision += 1;
        cv.notify_one();
    }
    pub fn push(&self, toast: Toast) -> u64 {
        let mut id = 0;
        self.update(|s| id = s.queue.push(toast, Instant::now()));
        id
    }
    pub fn replace(&self, id: u64, toast: Toast) -> u64 {
        let mut replacement = 0;
        self.update(|s| replacement = s.queue.replace(id, toast, Instant::now()));
        replacement
    }
    pub fn remove(&self, id: u64) {
        self.update(|s| s.queue.remove(id));
    }
    pub fn dismiss(&self, id: u64) {
        self.update(|s| s.queue.dismiss(id, Instant::now()));
    }
    pub fn set_displays(&self, displays: &[DisplayInfo]) {
        let displays: Vec<_> = displays.iter().map(|d| (d.clone(), scale_for(d))).collect();
        self.update(|s| s.queue.set_displays(&displays));
    }
    pub fn set_style(&self, style: Style) {
        self.update(|s| s.style = style);
    }
    pub fn clear(&self) {
        self.update(|s| s.queue.clear());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn display(key: &str, x: f64) -> DisplayInfo {
        DisplayInfo {
            key: key.into(),
            bounds: Rect::new(x, 0., 1000., 800.),
            name: String::new(),
            pixel_width: 1000,
            pixel_height: 800,
            reference_extent: None,
            rotation_deg: 0,
            is_primary: false,
            is_builtin: false,
            is_virtual: false,
        }
    }
    fn toast(persistent: bool) -> Toast {
        Toast {
            title: "Game mode enabled".into(),
            description: String::new(),
            undo: Some("Undo".into()),
            persistent,
            display: display("left", -1000.),
            scale: 1.0,
        }
    }
    #[test]
    fn backing_density_and_windows_scale_produce_the_same_physical_raster() {
        let now = Instant::now();
        let mut mac = Queue::default();
        let mut win = Queue::default();
        let mut t = toast(true);
        t.display.pixel_width *= 2;
        t.display.pixel_height *= 2;
        mac.push(t.clone(), now);
        t.display.bounds.w *= 2.;
        t.display.bounds.h *= 2.;
        t.scale = 2.;
        win.push(t, now);
        for ms in [80, 1000] {
            let frames = |q: &Queue| {
                q.frames(
                    now + Duration::from_millis(ms),
                    Style::default(),
                    false,
                    &crate::aim::text::StrokeText,
                )
            };
            let m = frames(&mac).remove(0);
            let w = frames(&win).remove(0);
            assert_eq!(m.pixels, w.pixels);
            assert_eq!((m.width, m.height), (w.width, w.height));
            assert_eq!(m.bounds.w * 2., w.bounds.w);
            assert_eq!(m.bounds.h * 2., w.bounds.h);
        }
    }

    #[test]
    fn mixed_dpi_and_scale_only_changes_repaint_in_physical_pixels() {
        let now = Instant::now();
        let mut q = Queue::default();
        let mut ids = Vec::new();
        for (index, scale) in [1., 1.25, 1.5, 2.].into_iter().enumerate() {
            let mut t = toast(true);
            t.display = display(&format!("monitor-{index}"), index as f64 * 1000. - 2000.);
            t.scale = scale;
            ids.push(q.push(t, now));
        }
        let frames = |q: &Queue| q.frames(now, Style::default(), true, &crate::aim::text::StrokeText);
        let before = frames(&q);
        for (f, scale) in before.iter().zip([1., 1.25, 1.5, 2.]) {
            assert_eq!(f.width, (360_f64 * scale).ceil() as usize);
            assert_eq!(f.bounds.w, f.width as f64);
            assert_eq!(f.bounds.h, f.height as f64);
            assert_eq!(f.card.w, 360. * scale);
        }
        // Same monitor rectangles; only the native DPI notification changes.
        assert!(q.apply_scales(vec![(ids[1], 2.)]));
        assert!(!q.apply_scales(vec![(ids[1], 2.)]));
        let after = frames(&q);
        assert_eq!(after[1].width, 720);
        assert_eq!(after[0].pixels, before[0].pixels);
        assert_eq!(after[2].pixels, before[2].pixels);
        // A disconnected display routes to a surviving display and adopts its DPI.
        q.set_displays(&[(display("replacement", -1000.), 1.5)]);
        assert!(frames(&q).iter().all(|f| f.width == 540));
    }

    #[test]
    fn animated_text_is_rasterized_at_final_physical_scale() {
        #[derive(Debug, Default)]
        struct Recording(std::sync::Mutex<Vec<f64>>);
        impl crate::aim::text::TextRaster for Recording {
            fn line(&self, text: &str, cap: f64, bold: bool) -> crate::aim::text::Mask {
                self.0.lock().unwrap().push(cap);
                crate::aim::text::TextRaster::line(&crate::aim::text::StrokeText, text, cap, bold)
            }
        }
        for scale in [1., 1.25, 1.5, 2.] {
            let now = Instant::now();
            let mut q = Queue::default();
            let mut t = toast(true);
            t.scale = scale;
            t.undo = None;
            q.push(t, now);
            let font = Recording::default();
            let elapsed = 0.08;
            q.frames(now + Duration::from_secs_f64(elapsed), Style::default(), false, &font);
            let zoom = 0.84 + 0.16 * ease(elapsed / 0.542, 0.3, 1.7, 0.5, 1.);
            let final_cap = 14. * 0.72 * scale * zoom;
            assert!(
                font.0.lock().unwrap().iter().any(|cap| (cap - final_cap).abs() < 1e-9),
                "glyphs must be rasterized at final device size, not resized after rendering"
            );
        }
    }

    #[test]
    fn replacement_reuses_slot_with_fresh_identity_and_lifetime() {
        let now = Instant::now();
        let mut queue = Queue::default();
        let first = queue.push(toast(false), now);
        let other = queue.push(toast(true), now);
        let later = now + LIFETIME - Duration::from_secs(1);
        queue.dismiss(first, later);
        let mut release = toast(false);
        release.title = "Pointer lock released".into();
        release.undo = None;
        release.display = display("other", 0.);
        let replacement = queue.replace(first, release, later);
        assert_ne!(replacement, first);
        assert_eq!(queue.entries.len(), 2);
        assert_eq!(queue.entries[0].id, replacement);
        assert_eq!(queue.entries[1].id, other);
        assert_eq!(queue.entries[0].toast.display.key, "left");
        assert!(queue.entries[0].toast.undo.is_none());
        assert!(queue.click(Click { id: first, undo: true }, later).is_none());
        queue.tick(now + LIFETIME, true);
        assert_eq!(queue.entries.len(), 2);
        queue.tick(later + LIFETIME, true);
        assert_eq!(queue.entries.len(), 1);
        assert_eq!(queue.entries[0].id, other);
        // An already expired original is a new timed toast, with no stale slot.
        let fresh = queue.replace(first, toast(false), later + LIFETIME);
        assert_eq!(queue.entries[1].id, fresh);
    }

    #[test]
    fn replacement_and_removal_are_immediate_without_an_animation_tick() {
        let now = Instant::now();
        let mut queue = Queue::default();
        let mut current = queue.push(toast(true), now);
        let other = queue.push(toast(true), now);
        for title in ["Off", "On", "Off"] {
            let old = current;
            let mut next = toast(true);
            next.title = title.into();
            current = queue.replace(old, next, now);
            assert_eq!(queue.entries.iter().map(|e| e.id).collect::<Vec<_>>(), [current, other]);
            assert!(queue.entries.iter().all(|e| e.leaving.is_none()));
            for reduced in [false, true] {
                let frames = queue.frames(now, Style::default(), reduced, &crate::aim::text::StrokeText);
                assert_eq!(frames.iter().map(|f| f.id).collect::<Vec<_>>(), [current, other]);
            }
            assert!(queue.click(Click { id: old, undo: true }, now).is_none());
        }
        queue.remove(current);
        assert_eq!(queue.entries.iter().map(|e| e.id).collect::<Vec<_>>(), [other]);
        assert!(
            queue
                .click(
                    Click {
                        id: current,
                        undo: true
                    },
                    now
                )
                .is_none()
        );
        let action = Click { id: other, undo: true };
        assert_eq!(queue.click(action, now), Some(action));
    }

    #[test]
    fn overlay_replaces_and_removes_the_old_identity_before_returning() {
        let overlay = NotificationOverlay::headless();
        let old = overlay.push(toast(true));
        let other = overlay.push(toast(true));
        let next = overlay.replace(old, toast(true));
        {
            let mut state = overlay.0.0.0.lock().unwrap();
            assert_eq!(
                state.queue.entries.iter().map(|e| e.id).collect::<Vec<_>>(),
                [next, other]
            );
            assert!(
                state
                    .queue
                    .click(Click { id: old, undo: true }, Instant::now())
                    .is_none()
            );
            let click = Click { id: next, undo: true };
            assert_eq!(state.queue.click(click, Instant::now()), Some(click));
        }
        // Even an Undo-triggered exit frame is removed synchronously on restore.
        overlay.remove(next);
        let state = overlay.0.0.0.lock().unwrap();
        assert_eq!(state.queue.entries.iter().map(|e| e.id).collect::<Vec<_>>(), [other]);
    }

    #[test]
    fn body_hover_has_no_badge_or_animation_at_display_scales() {
        use interaction::{HitState, Input};
        let now = Instant::now();
        let settled = now + Duration::from_secs(1);
        let font = crate::aim::text::StrokeText;
        for dark in [false, true] {
            for ppu in [1, 2] {
                for scale in [0.5, 1., 1.25, 2.] {
                    let mut toast = toast(true);
                    toast.scale = scale;
                    toast.display.pixel_width *= ppu;
                    toast.display.pixel_height *= ppu;
                    let mut q = Queue::default();
                    let id = q.push(toast, now);
                    let style = Style {
                        dark,
                        ..Style::default()
                    };
                    let idle = q.frames(settled, style, false, &font).remove(0);
                    assert_eq!(idle.width, (360. * scale * ppu as f64) as usize);
                    assert_eq!(idle.card.y, 0.);
                    assert_eq!(idle.bounds.x, -20. * scale - 360. * scale);
                    assert_eq!(idle.bounds.y, INSET * scale);
                    let body = Point::new(idle.card.w / 2., idle.card.h / 2.);
                    let corner = Point::new(0., 0.);
                    assert!(idle.contains(body));
                    assert!(!idle.contains(corner));
                    let mut hit = HitState::default();
                    hit.update_frame(&idle);
                    let (feedback, _) = hit.input(Input::Move(body));
                    assert!(!q.feedback(feedback, settled, false));
                    for millis in [0, 60, 150, 250] {
                        let frame = q
                            .frames(settled + Duration::from_millis(millis), style, false, &font)
                            .remove(0);
                        assert_eq!(frame.pixels, idle.pixels);
                        assert_eq!(frame.bounds, idle.bounds);
                    }
                    hit.input(Input::Down(body));
                    hit.input(Input::Move(corner));
                    assert!(hit.input(Input::Up(corner)).1.is_none());
                    for (point, undo) in [(body, false), (idle.undo.unwrap().center(), true)] {
                        hit.input(Input::Down(point));
                        let click = hit.input(Input::Up(point)).1.unwrap();
                        assert_eq!(click, Click { id, undo });
                        let mut dismiss = Queue {
                            entries: q.entries.clone(),
                            next: q.next,
                        };
                        assert_eq!(dismiss.click(click, settled), Some(click));
                        assert!(!dismiss.frames(settled, style, true, &font)[0].interactive);
                        assert!(dismiss.click(click, settled).is_none());
                        dismiss.tick(settled, true);
                        assert!(dismiss.entries.is_empty());
                    }
                }
            }
        }
    }

    #[test]
    fn button_feedback_repaints_only_button_and_preserves_hit_geometry() {
        let now = Instant::now();
        let mut q = Queue::default();
        let id = q.push(toast(true), now);
        let font = crate::aim::text::StrokeText;
        for dark in [false, true] {
            let style = Style {
                dark,
                ..Style::default()
            };
            q.feedback(
                Feedback {
                    id,
                    toast_hovered: false,
                    hovered: false,
                    pressed: false,
                },
                now,
                true,
            );
            let idle = q.frames(now, style, true, &font).remove(0);
            assert!(q.feedback(
                Feedback {
                    id,
                    toast_hovered: true,
                    hovered: true,
                    pressed: false
                },
                now,
                true
            ));
            let hover = q.frames(now, style, true, &font).remove(0);
            assert_ne!(idle.pixels, hover.pixels);
            assert_eq!(idle.undo, hover.undo);
            assert_eq!(idle.bounds, hover.bounds);
            let hit = idle.undo.unwrap();
            for y in 0..idle.height {
                for x in 0..idle.width {
                    if !hit.contains(Point::new(x as f64, y as f64)) {
                        assert_eq!(idle.pixels[y * idle.width + x], hover.pixels[y * idle.width + x]);
                    }
                }
            }
            q.feedback(
                Feedback {
                    id,
                    toast_hovered: true,
                    hovered: true,
                    pressed: true,
                },
                now,
                true,
            );
            let pressed = q.frames(now, style, true, &font).remove(0);
            assert_ne!(pressed.pixels, hover.pixels);
            assert_eq!(
                pressed.undo, idle.undo,
                "visual press scaling must not shrink the hit target"
            );
        }
        q.dismiss(id, now);
        assert!(!q.feedback(
            Feedback {
                id,
                toast_hovered: true,
                hovered: true,
                pressed: true
            },
            now,
            true
        ));
        assert!(!q.frames(now, Style::default(), true, &font)[0].interactive);
    }
    #[test]
    fn negative_subpoint_and_nearest_selection() {
        let ds = [display("left", -1000.), display("right", 0.)];
        assert_eq!(select_display(&ds, None, Point::new(-0.2, 20.)).unwrap().key, "left");
        assert_eq!(select_display(&ds, None, Point::new(-2000., 20.)).unwrap().key, "left");
        assert_eq!(
            select_display(&ds, Some("right"), Point::new(-0.2, 20.)).unwrap().key,
            "right"
        );
        assert!(select_display(&[], None, Point::new(0., 0.)).is_none());
    }
    #[test]
    fn timed_lock_toast_expires_at_normal_lifetime_with_exit_animation() {
        let now = Instant::now();
        for reduced in [false, true] {
            let mut q = Queue::default();
            let mut lock = toast(false);
            lock.title = "Pointer locked".into();
            q.push(lock, now);
            q.tick(now + LIFETIME - Duration::from_millis(1), reduced);
            assert_eq!(q.entries.len(), 1);
            assert!(q.entries[0].leaving.is_none());
            q.tick(now + LIFETIME, reduced);
            if !reduced {
                assert_eq!(q.entries[0].leaving, Some(now + LIFETIME));
                q.tick(now + LIFETIME + Duration::from_millis(170), reduced);
            }
            assert!(q.entries.is_empty());
        }
    }
    #[test]
    fn expiry_persistence_stacking_and_undo_once() {
        let now = Instant::now();
        let mut q = Queue::default();
        let a = q.push(toast(false), now);
        let b = q.push(toast(true), now);
        let frames = q.frames(now, Style::default(), true, &crate::aim::text::StrokeText);
        assert_eq!(frames[0].bounds.x, -380.);
        assert!(frames[1].bounds.y + frames[1].card.y > frames[0].bounds.y + frames[0].card.bottom());
        q.tick(now + LIFETIME, true);
        assert_eq!(q.entries.len(), 1);
        assert_eq!(q.entries[0].id, b);
        assert_eq!(
            q.click(Click { id: b, undo: true }, now),
            Some(Click { id: b, undo: true })
        );
        assert_eq!(q.click(Click { id: b, undo: true }, now), None);
        assert_eq!(q.click(Click { id: a, undo: true }, now), None);
        q.tick(now, true);
        assert!(q.entries.is_empty());
    }
    #[test]
    fn undo_hit_area_matches_the_button_on_a_multiline_toast() {
        let now = Instant::now();
        let mut queue = Queue::default();
        let mut t = toast(true);
        t.description = "Extra description below the title".into();
        queue.push(t, now);
        let frame = queue
            .frames(now, Style::default(), true, &crate::aim::text::StrokeText)
            .remove(0);
        let hit = frame.undo.unwrap();
        assert_eq!(hit.h, 32.0);
        assert!(!hit.contains(Point::new(hit.x + 10., hit.y - 1.)));
        assert!(hit.contains(Point::new(hit.x + 10., hit.y + 16.)));
    }
    #[test]
    fn body_dismiss_is_not_undo_and_reduced_motion_has_no_fade() {
        let now = Instant::now();
        let mut q = Queue::default();
        let id = q.push(toast(true), now);
        let frame = q
            .frames(now, Style::default(), true, &crate::aim::text::StrokeText)
            .remove(0);
        assert_eq!(frame.bounds.x, -380.);
        assert!(frame.pixels.iter().any(|p| p >> 24 == 255));
        assert!(!q.click(Click { id, undo: false }, now).unwrap().undo);
        q.tick(now, true);
        assert!(q.entries.is_empty());
    }
}

fn ease(t: f64, x1: f64, y1: f64, x2: f64, y2: f64) -> f64 {
    if t <= 0.0 {
        return 0.0;
    }
    if t >= 1.0 {
        return 1.0;
    }
    let curve = |s: f64, a: f64, b: f64| 3. * (1. - s).powi(2) * s * a + 3. * (1. - s) * s * s * b + s.powi(3);
    let (mut lo, mut hi) = (0., 1.);
    for _ in 0..16 {
        let mid = (lo + hi) / 2.;
        if curve(mid, x1, x2) < t {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    curve((lo + hi) / 2., y1, y2)
}

#[cfg(test)]
mod surface_tests {
    use super::*;
    use std::sync::mpsc::{self, Receiver, Sender};

    struct Observed {
        events: Sender<&'static str>,
        release: Option<Receiver<()>>,
    }
    impl Surface for Observed {
        fn refresh_preferences(&mut self) {
            self.events.send("preferences").unwrap();
        }
        fn present(&mut self, frames: Vec<Frame>) {
            self.events
                .send(if frames.is_empty() { "clear" } else { "show" })
                .unwrap();
            if !frames.is_empty()
                && let Some(release) = self.release.take()
            {
                release.recv_timeout(Duration::from_secs(2)).unwrap();
            }
        }
        fn clicks(&mut self) -> Vec<Click> {
            self.events.send("poll").unwrap();
            Vec::new()
        }
        fn feedback(&mut self) -> Vec<Feedback> {
            self.events.send("feedback").unwrap();
            Vec::new()
        }
        fn reduced_motion(&self) -> bool {
            true
        }
    }
    fn notice(height: f64) -> Toast {
        Toast {
            title: "Test".into(),
            description: String::new(),
            undo: None,
            persistent: true,
            display: DisplayInfo {
                key: "synthetic".into(),
                name: String::new(),
                bounds: Rect::new(0., 0., 1000., height),
                pixel_width: 1000,
                pixel_height: height as u32,
                reference_extent: None,
                rotation_deg: 0,
                is_primary: true,
                is_builtin: false,
                is_virtual: false,
            },
            scale: 1.,
        }
    }
    fn until(rx: &Receiver<&'static str>, expected: &str) {
        loop {
            if rx.recv_timeout(Duration::from_secs(2)).unwrap() == expected {
                return;
            }
        }
    }
    #[test]
    fn empty_and_clipped_surfaces_park_and_wake_on_updates() {
        let (tx, rx) = mpsc::channel();
        let overlay = NotificationOverlay::with_surface(
            move || {
                Some(Box::new(Observed {
                    events: tx,
                    release: None,
                }))
            },
            |_| {},
        )
        .unwrap();
        overlay.set_style(Style::default());
        overlay.set_displays(&[notice(800.).display]);
        assert!(rx.recv_timeout(Duration::from_millis(80)).is_err());
        overlay.push(notice(1.));
        until(&rx, "clear");
        assert!(rx.recv_timeout(Duration::from_millis(80)).is_err());
        overlay.set_displays(&[notice(800.).display]);
        until(&rx, "show");
        until(&rx, "feedback");
        overlay.clear();
        until(&rx, "clear");
        assert!(rx.recv_timeout(Duration::from_millis(80)).is_err());
        overlay.push(notice(800.));
        until(&rx, "show");
        overlay.clear();
        until(&rx, "clear");
    }
    #[test]
    fn clear_during_native_present_is_not_lost_before_parking() {
        let (tx, rx) = mpsc::channel();
        let (release, blocked) = mpsc::channel();
        let overlay = NotificationOverlay::with_surface(
            move || {
                Some(Box::new(Observed {
                    events: tx,
                    release: Some(blocked),
                }))
            },
            |_| {},
        )
        .unwrap();
        overlay.push(notice(800.));
        until(&rx, "show");
        overlay.clear();
        release.send(()).unwrap();
        until(&rx, "clear");
        assert!(rx.recv_timeout(Duration::from_millis(80)).is_err());
    }

    #[test]
    fn surface_seam_delivers_undo_once_without_creating_native_windows() {
        struct Fake {
            id: Option<u64>,
        }
        impl Surface for Fake {
            fn present(&mut self, frames: Vec<Frame>) {
                if let Some(frame) = frames.first() {
                    self.id = Some(frame.id);
                }
            }
            fn clicks(&mut self) -> Vec<Click> {
                self.id
                    .take()
                    .map(|id| vec![Click { id, undo: true }, Click { id, undo: true }])
                    .unwrap_or_default()
            }
            fn reduced_motion(&self) -> bool {
                true
            }
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let overlay = NotificationOverlay::with_surface(
            || Some(Box::new(Fake { id: None })),
            move |click| {
                let _ = tx.send(click);
            },
        )
        .unwrap();
        let d = DisplayInfo {
            key: "synthetic".into(),
            name: String::new(),
            bounds: Rect::new(-1000., 0., 1000., 800.),
            pixel_width: 1000,
            pixel_height: 800,
            reference_extent: None,
            rotation_deg: 0,
            is_primary: true,
            is_builtin: false,
            is_virtual: false,
        };
        let id = overlay.push(Toast {
            title: "Test".into(),
            description: String::new(),
            undo: Some("Undo".into()),
            persistent: true,
            display: d,
            scale: 1.,
        });
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            Click { id, undo: true }
        );
        assert!(rx.try_recv().is_err());
    }
}

#[cfg(test)]
mod geometry_tests {
    use super::*;
    #[test]
    fn motion_endpoints_are_exact_and_negative_y_is_not_truncated() {
        let mut d = DisplayInfo {
            key: "above".into(),
            name: String::new(),
            bounds: Rect::new(0., -800., 1000., 800.),
            pixel_width: 1000,
            pixel_height: 800,
            reference_extent: None,
            rotation_deg: 0,
            is_primary: true,
            is_builtin: false,
            is_virtual: false,
        };
        assert_eq!(
            select_display(&[d.clone()], None, Point::new(40., -0.2)).unwrap().key,
            "above"
        );
        let now = Instant::now();
        let mut q = Queue::default();
        q.push(
            Toast {
                title: "Test".into(),
                description: String::new(),
                undo: None,
                persistent: true,
                display: d.clone(),
                scale: 1.,
            },
            now,
        );
        let frame = q
            .frames(
                now + Duration::from_secs(1),
                Style::default(),
                false,
                &crate::aim::text::StrokeText,
            )
            .remove(0);
        assert_eq!(frame.bounds.x, 620.);
        assert_eq!(frame.card.w, 360.);
        assert_eq!(frame.bounds.y + frame.card.y, -780.);
        d.key = "replacement".into();
        d.bounds.x = -1000.;
        q.set_displays(&[(d, 1.0)]);
        let frame = q
            .frames(
                now + Duration::from_secs(2),
                Style::default(),
                true,
                &crate::aim::text::StrokeText,
            )
            .remove(0);
        assert_eq!(frame.bounds.x, -380.);
    }
}
