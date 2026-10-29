//! Shared input semantics and oink-ui button transitions for both native renderers.
use super::{Click, Frame};
use oink_foundation::{Point, Rect};
use std::time::Instant;

/// Native button state for a visible toast; independent of focus and capture.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Feedback {
    pub id: u64,
    pub hovered: bool,
    pub pressed: bool,
    /// Pointer is over the visible toast card, including Undo.
    pub toast_hovered: bool,
}
#[derive(Clone, Copy, Debug)]
pub(super) enum Input {
    Move(Point),
    Down(Point),
    Up(Point),
    Leave,
}
#[derive(Debug, Default)]
pub(super) struct HitState {
    pub id: u64,
    pub undo: Option<Rect>,
    pub interactive: bool,
    hovered: bool,
    down: Option<bool>,
    toast_hovered: bool,
    mask: Option<Frame>,
}
impl HitState {
    pub fn update(&mut self, id: u64, undo: Option<Rect>, interactive: bool) {
        if self.id != id || !interactive {
            self.hovered = false;
            self.toast_hovered = false;
            self.down = None;
        }
        self.mask = None;
        self.id = id;
        self.undo = undo;
        self.interactive = interactive;
    }
    pub fn update_frame(&mut self, frame: &Frame) {
        self.update(frame.id, frame.undo, frame.interactive);
        self.mask = Some(frame.clone());
    }
    pub fn contains(&self, point: Point) -> bool {
        self.interactive && self.mask.as_ref().is_none_or(|f| f.contains(point))
    }
    pub fn feedback(&self) -> Feedback {
        Feedback {
            id: self.id,
            hovered: self.hovered,
            toast_hovered: self.toast_hovered,
            pressed: self.hovered && self.down == Some(true),
        }
    }
    pub fn input(&mut self, input: Input) -> (Feedback, Option<Click>) {
        if !self.interactive {
            return (self.feedback(), None);
        }
        let mut click = None;
        if let Input::Leave = input {
            self.hovered = false;
            self.toast_hovered = false;
            self.down = None;
        } else {
            let (Input::Move(p) | Input::Down(p) | Input::Up(p)) = input else {
                unreachable!()
            };
            self.toast_hovered = self.contains(p);
            self.hovered = self.toast_hovered && self.undo.is_some_and(|r| r.contains(p));
            if !self.toast_hovered {
                self.down = None;
            }
            match input {
                Input::Down(_) => self.down = self.toast_hovered.then_some(self.hovered),
                Input::Up(_) if self.down.take() == Some(self.hovered) && self.toast_hovered => {
                    click = Some(Click {
                        id: self.id,
                        undo: self.hovered,
                    });
                }
                _ => {}
            }
        }
        (self.feedback(), click)
    }
}

#[derive(Clone, Debug, Default)]
pub(super) struct ButtonAnimation {
    hover: Transition,
    press: Transition,
    hovered: bool,
    pressed: bool,
}
#[derive(Clone, Debug, Default)]
struct Transition {
    from: f64,
    to: f64,
    at: Option<Instant>,
    duration: f64,
    spring: bool,
}
impl Transition {
    fn sample(&self, now: Instant, reduced: bool) -> f64 {
        let t = if reduced {
            1.
        } else {
            self.at.map_or(1., |at| {
                (now.saturating_duration_since(at).as_secs_f64() / self.duration).min(1.)
            })
        };
        let eased = if self.spring {
            super::ease(t, 0.3, 1.7, 0.5, 1.)
        } else {
            super::ease(t, 0.22, 1., 0.36, 1.)
        };
        self.from + (self.to - self.from) * eased
    }
    fn set(&mut self, to: f64, now: Instant, reduced: bool, duration: f64, spring: bool) {
        self.from = self.sample(now, reduced);
        self.to = to;
        self.at = Some(now);
        self.duration = duration;
        self.spring = spring;
    }
    fn active(&self, now: Instant) -> bool {
        self.at
            .is_some_and(|at| now.saturating_duration_since(at).as_secs_f64() < self.duration)
    }
}
impl ButtonAnimation {
    pub fn set(&mut self, hovered: bool, pressed: bool, now: Instant, reduced: bool) -> bool {
        let changed = self.hovered != hovered || self.pressed != pressed;
        if self.hovered != hovered {
            self.hover.set(f64::from(hovered), now, reduced, 0.15, false);
        }
        if self.pressed != pressed {
            self.press.set(
                f64::from(pressed),
                now,
                reduced,
                if pressed { 0.09 } else { 0.542 },
                !pressed,
            );
        }
        self.hovered = hovered;
        self.pressed = pressed;
        changed
    }
    pub fn sample(&self, now: Instant, reduced: bool) -> (f64, f64) {
        (
            self.hover.sample(now, reduced),
            1. - 0.045 * self.press.sample(now, reduced),
        )
    }
    pub fn active(&self, now: Instant) -> bool {
        self.hover.active(now) || self.press.active(now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notification::Click;
    use oink_foundation::{Point, Rect};
    use std::time::{Duration, Instant};

    #[test]
    fn hover_press_drag_cancel_and_reused_windows() {
        let mut hit = HitState::default();
        hit.update(7, Some(Rect::new(20., 0., 20., 20.)), true);
        let button = Point::new(25., 10.);
        let body = Point::new(5., 10.);
        assert_eq!(hit.input(Input::Move(button)).1, None);
        assert!(hit.feedback().hovered);
        hit.input(Input::Down(button));
        assert!(hit.feedback().pressed);
        hit.input(Input::Move(body));
        assert!(!hit.feedback().pressed);
        assert_eq!(hit.input(Input::Up(body)).1, None, "drag outside cancels Undo");
        hit.input(Input::Down(body));
        assert_eq!(hit.input(Input::Up(body)).1, Some(Click { id: 7, undo: false }));
        hit.input(Input::Down(button));
        assert_eq!(hit.input(Input::Up(button)).1, Some(Click { id: 7, undo: true }));
        hit.input(Input::Down(button));
        hit.input(Input::Leave);
        assert!(!hit.feedback().hovered);
        assert!(!hit.feedback().pressed);
        hit.update(8, Some(Rect::new(20., 0., 20., 20.)), true);
        assert_eq!(hit.input(Input::Up(button)).1, None);
        hit.input(Input::Down(button));
        hit.update(8, Some(Rect::new(20., 0., 20., 20.)), false);
        assert_eq!(hit.input(Input::Up(button)).1, None);
        assert!(!hit.feedback().hovered);
    }

    #[test]
    fn button_transition_is_continuous_and_reduced_motion_is_immediate() {
        let now = Instant::now();
        let mut animation = ButtonAnimation::default();
        animation.set(true, false, now, false);
        assert_eq!(animation.sample(now, false), (0., 1.));
        let halfway = animation.sample(now + Duration::from_millis(80), false);
        assert!(halfway.0 > 0. && halfway.0 < 1.);
        assert_eq!(animation.sample(now + Duration::from_secs(1), false), (1., 1.));
        animation.set(true, true, now + Duration::from_secs(1), false);
        assert_eq!(animation.sample(now + Duration::from_secs(2), false), (1., 0.955));
        animation.set(false, false, now + Duration::from_secs(2), false);
        assert_eq!(animation.sample(now + Duration::from_secs(2), true), (0., 1.));
    }
}
