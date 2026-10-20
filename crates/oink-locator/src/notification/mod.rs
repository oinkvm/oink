//! Backend-owned desktop toasts. The queue and rasterizer never query input or
//! create native windows; only the optional surface worker touches the desktop.
use oink_foundation::{Point, Rect, display::DisplayInfo};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

mod color;
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
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<u32>,
    /// Hit area in native units relative to the top-left of the window.
    pub undo: Option<Rect>,
    pub interactive: bool,
}
#[derive(Clone, Debug)]
struct Entry {
    id: u64,
    toast: Toast,
    born: Instant,
    leaving: Option<Instant>,
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
        });
        self.next
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
                let (mut pixels, w, h) = raster::paint(&e.toast, style, width, scale * d.pixels_per_unit(), text);
                let height = h as f64 / d.pixels_per_unit();
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
                    width * shrink,
                    height * shrink,
                );
                let (pixels, w, h) = raster::resize(pixels, w, h, shrink);
                Some(Frame {
                    id: e.id,
                    bounds,
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
                let mut preference_revision = u64::MAX;
                loop {
                    let (m, cv) = &*shared;
                    let events = surface.clicks();
                    let mut state = m.lock().unwrap_or_else(|e| e.into_inner());
                    if state.stop {
                        surface.present(Vec::new());
                        return;
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
                    let changed = old != state.queue.entries.len() || last_reduced != Some(reduced);
                    last_reduced = Some(reduced);
                    let animated = !reduced
                        && state.queue.entries.iter().any(|e| {
                            e.leaving.is_some() || now.saturating_duration_since(e.born).as_secs_f64() < 0.662
                        });
                    let frames = if changed
                        || animated
                        || state.revision != last_revision
                        || !events.is_empty()
                        || state.queue.entries.iter().any(|e| e.leaving.is_some())
                    {
                        Some(state.queue.frames(now, state.style, reduced, &*text))
                    } else {
                        None
                    };
                    // Expiry/removal can change the frame even without a caller update.
                    last_revision = state.revision;
                    let empty = state.queue.entries.is_empty();
                    drop(state);
                    if let Some(frames) = frames {
                        surface.present(frames);
                    } else if empty {
                        surface.present(Vec::new());
                    }
                    for event in events {
                        clicked(event);
                    }
                    let state = m.lock().unwrap_or_else(|e| e.into_inner());
                    if state.stop {
                        continue;
                    }
                    if state.queue.entries.is_empty() {
                        drop(cv.wait(state).unwrap_or_else(|e| e.into_inner()));
                    } else {
                        drop(
                            cv.wait_timeout(state, Duration::from_millis(16))
                                .unwrap_or_else(|e| e.into_inner()),
                        );
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
    fn expiry_persistence_stacking_and_undo_once() {
        let now = Instant::now();
        let mut q = Queue::default();
        let a = q.push(toast(false), now);
        let b = q.push(toast(true), now);
        let frames = q.frames(now, Style::default(), true, &crate::aim::text::StrokeText);
        assert_eq!(frames[0].bounds.x, -380.);
        assert!(frames[1].bounds.y > frames[0].bounds.bottom());
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
        assert_eq!(frame.bounds.w, 360.);
        assert_eq!(frame.bounds.y, -780.);
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
