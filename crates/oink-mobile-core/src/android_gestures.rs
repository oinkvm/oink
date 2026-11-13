//! Completed desktop Dock gestures on an Android target. The desktop wire has
//! semantic progress, not raw contacts or a finger count. The Android target
//! currently uses completed system actions (native touchpad navigation still
//! needs OEM-specific end-to-end verification).
use oink_foundation::input::{Gesture, GesturePhase, SwipeAxis};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SystemGesture {
    Home,
    Recents,
    SwitchNext,
    SwitchPrevious,
    Notifications,
}

/// Act only once on an explicitly completed gesture. No action on Changed,
/// cancellation, an orphan Ended or after focus/display/connection loss.
#[derive(Debug, Default)]
pub struct AndroidGestures {
    axis: Option<SwipeAxis>,
}
impl AndroidGestures {
    pub fn cancel(&mut self) {
        self.axis = None;
    }
    pub fn apply(&mut self, gesture: Gesture) -> Option<SystemGesture> {
        let Gesture::DockSwipe {
            axis,
            phase,
            progress,
            velocity,
        } = gesture
        else {
            self.cancel();
            return None;
        };
        if !progress.is_finite() || !velocity.is_finite() {
            self.cancel();
            return None;
        }
        match phase {
            GesturePhase::Began => {
                self.axis = Some(axis);
                None
            }
            GesturePhase::Changed => {
                if self.axis != Some(axis) {
                    self.cancel();
                }
                None
            }
            GesturePhase::Cancelled => {
                self.cancel();
                None
            }
            GesturePhase::Ended => {
                let active = self.axis.take();
                if active != Some(axis) || progress == 0.0 {
                    return None;
                }
                Some(match axis {
                    SwipeAxis::Horizontal if progress > 0.0 => SystemGesture::SwitchNext,
                    SwipeAxis::Horizontal => SystemGesture::SwitchPrevious,
                    SwipeAxis::Vertical if progress < 0.0 => SystemGesture::Recents,
                    SwipeAxis::Vertical => SystemGesture::Notifications,
                    SwipeAxis::Pinch => SystemGesture::Home,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn swipe(axis: SwipeAxis, phase: GesturePhase, progress: f64) -> Gesture {
        Gesture::DockSwipe {
            axis,
            phase,
            progress,
            velocity: 0.0,
        }
    }
    #[test]
    fn completed_gestures_map_once_and_keep_direction() {
        for (axis, progress, expected) in [
            (SwipeAxis::Horizontal, 0.7, SystemGesture::SwitchNext),
            (SwipeAxis::Horizontal, -0.7, SystemGesture::SwitchPrevious),
            (SwipeAxis::Vertical, -0.7, SystemGesture::Recents),
            (SwipeAxis::Vertical, 0.7, SystemGesture::Notifications),
            (SwipeAxis::Pinch, -0.7, SystemGesture::Home),
            (SwipeAxis::Pinch, 0.7, SystemGesture::Home),
        ] {
            let mut gestures = AndroidGestures::default();
            assert_eq!(gestures.apply(swipe(axis, GesturePhase::Began, 0.1)), None);
            assert_eq!(gestures.apply(swipe(axis, GesturePhase::Changed, progress)), None);
            assert_eq!(
                gestures.apply(swipe(axis, GesturePhase::Ended, progress)),
                Some(expected)
            );
            assert_eq!(gestures.apply(swipe(axis, GesturePhase::Ended, progress)), None);
        }
    }
    #[test]
    fn cancellation_focus_loss_malformed_and_unrecognised_sequences_do_nothing() {
        use GesturePhase::*;
        use SwipeAxis::*;
        let mut gestures = AndroidGestures::default();
        for phase in [Cancelled, Changed] {
            gestures.apply(swipe(Horizontal, Began, 0.1));
            gestures.apply(swipe(Vertical, phase, 0.7));
            assert_eq!(gestures.apply(swipe(Horizontal, Ended, 0.7)), None);
        }
        for progress in [f64::NAN, f64::INFINITY, 0.0] {
            gestures.apply(swipe(Horizontal, Began, 0.1));
            assert_eq!(gestures.apply(swipe(Horizontal, Ended, progress)), None);
        }
        gestures.apply(swipe(Horizontal, Began, 0.1));
        gestures.cancel();
        assert_eq!(gestures.apply(swipe(Horizontal, Ended, 0.7)), None);
        gestures.apply(swipe(Horizontal, Began, 0.1));
        assert_eq!(gestures.apply(Gesture::SmartMagnify), None);
        assert_eq!(gestures.apply(swipe(Horizontal, Ended, 0.7)), None);
    }
}
