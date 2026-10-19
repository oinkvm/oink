//! Layout projections have their own windows and lifetime, independent of cursor
//! trails, cursor-owner/cover windows and input capture. No cursor APIs are used.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use oink_display::projection::Projection;
use oink_foundation::Rect;

use crate::aim::raster::{Canvas, Color, over};
mod annotation;
use crate::aim::text::{self, Pen, TextRaster};
use annotation::raster;

#[cfg(target_os = "macos")]
#[path = "macos.rs"]
mod platform;
#[cfg(windows)]
#[path = "windows.rs"]
mod platform;

#[cfg(target_os = "macos")]
pub(crate) fn diagnose(health: &crate::diagnostics::Health, mtm: objc2::MainThreadMarker) {
    platform::diagnose(health, mtm);
}

#[derive(Clone, Debug, PartialEq)]
pub struct NamedProjection {
    pub geometry: Projection,
    pub name: String,
    pub pixels_per_unit: f64,
    /// Complete physical monitor bounds in native desktop coordinates.
    pub screen: Rect,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct View {
    pub projections: Vec<NamedProjection>,
    /// Accent resolved by the rendering device, including its effective appearance.
    pub accent: [u8; 3],
    /// First static checkpoint can show the handles without intercepting input.
    pub show_handles: bool,
    pub editable: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DragPhase {
    Begin,
    Update,
    End,
}
#[derive(Clone, Debug)]
pub enum Event {
    Drag {
        projection: Box<Projection>,
        handle: oink_display::projection::Handle,
        delta: f64,
        phase: DragPhase,
    },
    Escape,
}
static EVENTS: Mutex<Vec<Event>> = Mutex::new(Vec::new());
pub(super) static DRAGGING: AtomicBool = AtomicBool::new(false);
pub(super) fn emit(event: Event) {
    let mut events = EVENTS.lock().unwrap_or_else(|e| e.into_inner());
    if matches!(
        event,
        Event::Drag {
            phase: DragPhase::Update,
            ..
        }
    ) && events.last().is_some_and(|e| {
        matches!(
            e,
            Event::Drag {
                phase: DragPhase::Update,
                ..
            }
        )
    }) {
        events.pop();
    }
    if events.len() < 64 {
        events.push(event);
    }
}

#[derive(Clone, Debug)]
pub(super) struct NativeHandle {
    pub projection: Projection,
    pub hit: oink_display::projection::HitRegion,
}

#[derive(Debug, Default)]
struct State {
    view: Option<View>,
    updated: Option<Instant>,
    stop: bool,
}

/// One persistent native rendering thread. Updates replace a single latest frame;
/// an abandoned arranging preview expires even if its end message was lost.
#[derive(Clone, Debug)]
pub struct ProjectionOverlay {
    health: Arc<crate::diagnostics::Health>,
    shared: Arc<(Mutex<State>, Condvar)>,
}

impl ProjectionOverlay {
    pub fn platform() -> Option<Self> {
        let shared = Arc::new((Mutex::new(State::default()), Condvar::new()));
        let health = crate::diagnostics::Health::new(crate::diagnostics::Kind::Projection);
        let task_health = health.clone();
        let task = shared.clone();
        std::thread::Builder::new()
            .name("oink-layout-projection".into())
            .spawn(move || render_thread(task, task_health))
            .map_err(|_| health.failed())
            .ok()?;
        Some(Self { shared, health })
    }

    pub fn update(&self, view: View) {
        self.health
            .intent(u64::from(!view.projections.is_empty()) | (u64::from(view.editable) << 1));
        let (state, cv) = &*self.shared;
        let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
        state.view = Some(view);
        state.updated = Some(Instant::now());
        cv.notify_one();
    }

    pub fn hide(&self) {
        self.update(View::default());
    }

    pub fn take_events(&self) -> Vec<Event> {
        std::mem::take(&mut *EVENTS.lock().unwrap_or_else(|e| e.into_inner()))
    }

    pub fn shutdown(&self) {
        let (state, cv) = &*self.shared;
        state.lock().unwrap_or_else(|e| e.into_inner()).stop = true;
        cv.notify_one();
    }
}

/// Explicit end and lease expiry retain the final pixels for the same fade.
#[derive(Debug)]
struct Fade {
    from: f64,
    to: f64,
    start: Instant,
}
impl Fade {
    fn new(now: Instant) -> Self {
        Self {
            from: 0.0,
            to: 0.0,
            start: now,
        }
    }
    fn value(&self, now: Instant) -> f64 {
        let duration = if self.to > self.from { 0.09 } else { 0.24 };
        let t = (now.saturating_duration_since(self.start).as_secs_f64() / duration).min(1.0);
        self.from + (self.to - self.from) * (1.0 - (1.0 - t).powi(3))
    }
    fn target(&mut self, shown: bool, now: Instant) {
        let to = f64::from(shown);
        if self.to != to {
            self.from = self.value(now);
            self.to = to;
            self.start = now;
        }
    }
}

fn render_thread(shared: Arc<(Mutex<State>, Condvar)>, health: Arc<crate::diagnostics::Health>) {
    use crate::diagnostics::Hide;
    let _worker = health.worker();
    health.pulse();
    let Some(mut backend) = platform::Backend::new(health.clone()) else {
        health.failed();
        return;
    };
    let font = text::system_medium();
    let (state, cv) = &*shared;
    let mut fade = Fade::new(Instant::now());
    let mut frames: Vec<Frame> = Vec::new();
    let mut cached: Vec<(NamedProjection, [u8; 3], Frame)> = Vec::new();
    let mut editable = false;
    let mut escaped = false;
    let mut last_view: Option<View> = None;
    let mut last_dragging = false;
    let mut last_alpha = -1.0;
    loop {
        let (view, expired) = {
            let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
            if s.stop {
                health.hide(Hide::Shutdown);
                backend.hide();
                return;
            }
            let expired = s.updated.is_some_and(|t| t.elapsed() >= Duration::from_secs(2));
            if expired {
                s.updated = None;
            }
            (s.view.take(), expired)
        };
        let now = Instant::now();
        if expired {
            fade.target(false, now);
            editable = false;
            last_view = None;
            health.hide(Hide::LeaseExpired);
        }
        let mut changed = false;
        if let Some(view) = view {
            editable = view.editable;
            let dragging = DRAGGING.load(Ordering::Acquire);
            if last_view.as_ref() != Some(&view) || dragging != last_dragging {
                let mut next_cache = Vec::new();
                let next: Vec<_> = view
                    .projections
                    .iter()
                    .filter_map(|p| {
                        let mut frame = if let Some((_, _, frame)) =
                            cached.iter().find(|(old, rgb, _)| old == p && *rgb == view.accent)
                        {
                            frame.clone()
                        } else {
                            raster(p, view.accent, &*font)?
                        };
                        next_cache.push((p.clone(), view.accent, frame.clone()));
                        if editable {
                            frame.handles = p
                                .geometry
                                .handles()
                                .into_iter()
                                .map(|hit| NativeHandle {
                                    projection: p.geometry.clone(),
                                    hit,
                                })
                                .collect();
                        }
                        Some(frame)
                    })
                    .collect();
                cached = next_cache;
                if next.is_empty() {
                    health.hide(Hide::Explicit);
                }
                fade.target(!next.is_empty(), now);
                if !next.is_empty() {
                    frames = next;
                }
                last_dragging = dragging;
                last_view = Some(view);
                changed = true;
            }
        }
        let alpha = fade.value(now);
        if needs_present(changed, alpha, last_alpha) {
            let presented = if alpha <= 0.0 {
                vec![]
            } else {
                frames.iter().map(|frame| frame.with_alpha(alpha)).collect()
            };
            health.frame(if alpha > 0.0 { frames.len() } else { 0 });
            backend.present(presented, editable);
            #[cfg(not(target_os = "macos"))]
            health.applied();
            last_alpha = alpha;
            if alpha <= 0.0 && fade.to == 0.0 {
                frames.clear();
            }
        }
        if alpha > 0.0 || editable {
            health.pulse();
        }
        backend.pump();
        if editable {
            let down = backend.escape_down();
            if down && !escaped {
                emit(Event::Escape);
            }
            escaped = down;
        } else {
            escaped = false;
        }
        let s = state.lock().unwrap_or_else(|e| e.into_inner());
        if s.view.is_some() || s.stop {
            continue;
        }
        if alpha > 0.0 || editable || fade.to > 0.0 {
            drop(
                cv.wait_timeout(s, Duration::from_millis(16))
                    .unwrap_or_else(|e| e.into_inner()),
            );
        } else {
            drop(cv.wait(s).unwrap_or_else(|e| e.into_inner()));
        }
    }
}

fn needs_present(changed: bool, alpha: f64, last_alpha: f64) -> bool {
    // The last nonzero frame can be below the raster update epsilon. Always
    // submit the terminal empty frame so native panels leave the window list.
    changed || (alpha == 0.0 && last_alpha > 0.0) || (alpha - last_alpha).abs() > 0.0001
}

#[derive(Clone, Debug)]
pub(super) struct Frame {
    pub bounds: Rect,
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<u32>,
    pub handles: Vec<NativeHandle>,
}

impl Frame {
    fn with_alpha(&self, alpha: f64) -> Self {
        // Steady gesture frames are already premultiplied at full opacity.
        // Only the short entry/exit transition needs a per-channel pass.
        if alpha >= 1.0 {
            return self.clone();
        }
        Self {
            bounds: self.bounds,
            width: self.width,
            height: self.height,
            handles: self.handles.clone(),
            pixels: self
                .pixels
                .iter()
                .map(|p| {
                    [0, 8, 16, 24].into_iter().fold(0, |out, s| {
                        out | (((f64::from((p >> s) & 255) * alpha).round() as u32) << s)
                    })
                })
                .collect(),
        }
    }
}
#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    #[test]
    fn fade_submits_empty_frame_even_after_a_tiny_final_alpha() {
        let now = Instant::now();
        let mut fade = Fade::new(now);
        fade.target(true, now);
        let end = now + Duration::from_millis(100);
        fade.target(false, end);
        let last = fade.value(end + Duration::from_millis(230));
        assert!(last > 0.0 && last < 0.0001);
        let hidden = fade.value(end + Duration::from_millis(240));
        assert_eq!(hidden, 0.0);
        assert!(
            needs_present(false, hidden, last),
            "native windows must be ordered out at zero alpha"
        );
        assert!(!needs_present(false, hidden, hidden));
    }

    #[test]
    fn fade_retains_last_frame_and_new_gesture_reverses_without_flashing() {
        let now = Instant::now();
        let mut fade = Fade::new(now);
        fade.target(true, now);
        assert_eq!(fade.value(now), 0.0);
        assert_eq!(fade.value(now + Duration::from_millis(90)), 1.0);
        let end = now + Duration::from_millis(500);
        fade.target(false, end);
        assert_eq!(fade.value(end), 1.0);
        let mid = end + Duration::from_millis(120);
        let alpha = fade.value(mid);
        assert!(alpha > 0.0 && alpha < 1.0);
        fade.target(false, mid);
        assert_eq!(fade.value(mid), alpha);
        fade.target(true, mid);
        assert_eq!(fade.value(mid), alpha);
        assert_eq!(fade.value(mid + Duration::from_millis(90)), 1.0);
        fade.target(false, mid + Duration::from_millis(100));
        assert_eq!(fade.value(mid + Duration::from_millis(340)), 0.0);
        let f = Frame {
            bounds: Rect::new(0.0, 0.0, 1.0, 1.0),
            width: 1,
            height: 1,
            pixels: vec![0x80402010],
            handles: vec![],
        };
        assert_eq!(f.with_alpha(0.5).pixels, vec![0x40201008]);
    }
}
