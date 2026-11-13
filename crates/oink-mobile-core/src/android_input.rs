//! Android helper input state. Coordinates are physical pixels in the current
//! display orientation. VirtualMouse feedback always comes from the OS.
use oink_foundation::Point;

pub const POINTER_TOLERANCE: f64 = 1.1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AndroidDisplay {
    pub width: u32,
    pub height: u32,
    pub rotation: u16,
    pub generation: u64,
}
impl AndroidDisplay {
    pub fn valid(self) -> bool {
        self.width > 0 && self.height > 0 && [0, 90, 180, 270].contains(&self.rotation)
    }
    pub fn contains(self, point: Point) -> bool {
        point.x.is_finite()
            && point.y.is_finite()
            && point.x >= 0.0
            && point.y >= 0.0
            && point.x < f64::from(self.width)
            && point.y < f64::from(self.height)
    }
}

/// One outstanding native report at a time. Changing targets replaces only the
/// destination, preserving feedback for the report already submitted.
#[derive(Debug)]
pub struct PointerServo {
    display: AndroidDisplay,
    target: Option<Point>,
    pending: Option<(Point, Point)>,
    gain: f64,
}
impl PointerServo {
    pub fn new(display: AndroidDisplay) -> Option<Self> {
        display.valid().then_some(Self {
            display,
            target: None,
            pending: None,
            gain: 1.0,
        })
    }
    pub fn target(&mut self, point: Point, generation: u64) -> bool {
        if generation != self.display.generation || !self.display.contains(point) {
            return false;
        }
        self.target = Some(point);
        true
    }
    /// Call after the preceding report's settling interval, with a fresh
    /// getCursorPosition result. Never feed a predicted/integrated position here.
    pub fn step(&mut self, actual: Point, generation: u64) -> Option<Point> {
        if generation != self.display.generation || !self.display.contains(actual) {
            self.cancel();
            return None;
        }
        if let Some((before, sent)) = self.pending.take() {
            let length = sent.x * sent.x + sent.y * sent.y;
            let moved = Point::new(actual.x - before.x, actual.y - before.y);
            // InputReader quantizes reports. A zero-motion read cannot estimate
            // gain and must not drive it toward zero. API35 probe used these bounds.
            if length >= 1.0 && moved.x * moved.x + moved.y * moved.y >= 0.25 {
                let measured = ((moved.x * sent.x + moved.y * sent.y) / length).clamp(0.5, 16.0);
                self.gain = (self.gain + measured) * 0.5;
            }
        }
        let target = self.target?;
        let error = Point::new(target.x - actual.x, target.y - actual.y);
        if error.x.abs() <= POINTER_TOLERANCE && error.y.abs() <= POINTER_TOLERANCE {
            self.target = None;
            return None;
        }
        // API35 InputReader drops sub-count reports. A measured high gain can
        // otherwise leave a stationary target more than 2 px away indefinitely.
        // Stop each settled axis, and use at least one count on remaining axes.
        let count = |error: f64| {
            if error.abs() <= POINTER_TOLERANCE {
                0.0
            } else {
                (error.abs() / self.gain).clamp(1.0, 8.0).copysign(error)
            }
        };
        let report = Point::new(count(error.x), count(error.y));
        self.pending = Some((actual, report));
        Some(report)
    }
    pub fn cancel(&mut self) {
        self.target = None;
        self.pending = None;
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KeyboardReport {
    pub modifiers: u8,
    pub usages: [u8; 6],
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyboardError {
    Pending,
    Unsupported,
    Rollover,
}

/// Keys received before InputManager's device-added event are discarded. Focus
/// loss, removal, and rollover clear held state instead of replaying early keys.
#[derive(Debug, Default)]
pub struct AndroidKeyboard {
    focused: bool,
    ready: bool,
    report: KeyboardReport,
}
impl AndroidKeyboard {
    pub fn focus(&mut self, active: bool) {
        self.focused = active;
        self.ready = false;
        self.report = KeyboardReport::default();
    }
    pub fn device_ready(&mut self) {
        self.ready = self.focused;
    }
    pub fn device_removed(&mut self) {
        self.ready = false;
        self.report = KeyboardReport::default();
    }
    pub fn key(&mut self, page: u16, usage: u16, pressed: bool) -> Result<KeyboardReport, KeyboardError> {
        if !self.focused || !self.ready {
            return Err(KeyboardError::Pending);
        }
        if page != 7 || !(4..=0xe7).contains(&usage) {
            return Err(KeyboardError::Unsupported);
        }
        if usage >= 0xe0 {
            let bit = 1 << (usage - 0xe0);
            if pressed {
                self.report.modifiers |= bit;
            } else {
                self.report.modifiers &= !bit;
            }
        } else {
            let usage = usage as u8;
            if pressed && !self.report.usages.contains(&usage) {
                let Some(slot) = self.report.usages.iter_mut().find(|value| **value == 0) else {
                    self.report = KeyboardReport::default();
                    return Err(KeyboardError::Rollover);
                };
                *slot = usage;
            }
            if !pressed {
                for slot in &mut self.report.usages {
                    if *slot == usage {
                        *slot = 0;
                    }
                }
            }
        }
        Ok(self.report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn display() -> AndroidDisplay {
        AndroidDisplay {
            width: 1080,
            height: 2400,
            rotation: 0,
            generation: 7,
        }
    }
    #[test]
    fn stale_display_and_invalid_feedback_cancel_motion() {
        let mut servo = PointerServo::new(display()).unwrap();
        assert!(!servo.target(Point::new(100.0, 100.0), 6));
        assert!(servo.target(Point::new(100.0, 100.0), 7));
        assert!(servo.step(Point::new(0.0, 0.0), 6).is_none());
        assert!(servo.step(Point::new(0.0, 0.0), 7).is_none());
        assert!(!servo.target(Point::new(f64::NAN, 0.0), 7));
    }
    #[test]
    fn feedback_converges_and_follows_replaced_targets() {
        let mut servo = PointerServo::new(display()).unwrap();
        let mut actual = Point::new(100.0, 100.0);
        for target in [
            Point::new(400.0, 600.0),
            Point::new(800.0, 1600.0),
            Point::new(100.0, 100.0),
        ] {
            assert!(servo.target(target, 7));
            for _ in 0..1000 {
                let Some(step) = servo.step(actual, 7) else {
                    break;
                };
                actual.x += step.x * 3.0;
                actual.y += step.y * 3.0;
            }
            assert!((target.x - actual.x).abs() <= POINTER_TOLERANCE);
            assert!((target.y - actual.y).abs() <= POINTER_TOLERANCE);
        }
        // Each tick changes the destination while feedback still belongs to the
        // preceding report. This is a controller regression, not device latency evidence.
        for n in 0..100 {
            let target = Point::new(100.0 + f64::from(n), 100.0);
            servo.target(target, 7);
            if let Some(step) = servo.step(actual, 7) {
                actual.x += step.x * 3.0;
                actual.y += step.y * 3.0;
            }
            assert!((target.x - actual.x).abs() < 4.0);
        }
    }
    #[test]
    fn quantized_reports_do_not_leave_a_stationary_axis_outside_tolerance() {
        let mut servo = PointerServo::new(display()).unwrap();
        servo.gain = 16.0;
        let mut actual = Point::new(200.0, 652.25);
        let target = Point::new(200.0, 650.0);
        servo.target(target, 7);
        for _ in 0..20 {
            let Some(report) = servo.step(actual, 7) else {
                break;
            };
            assert_eq!(report.x, 0.0);
            actual.y += report.y.trunc();
        }
        assert!((actual.y - target.y).abs() <= POINTER_TOLERANCE);
        assert!(servo.step(actual, 7).is_none());
    }
    #[test]
    fn pending_keys_are_never_replayed() {
        let mut keyboard = AndroidKeyboard::default();
        keyboard.focus(true);
        assert_eq!(keyboard.key(7, 4, true), Err(KeyboardError::Pending));
        keyboard.device_ready();
        assert_eq!(keyboard.key(7, 5, true).unwrap().usages, [5, 0, 0, 0, 0, 0]);
        keyboard.device_removed();
        keyboard.device_ready();
        assert_eq!(keyboard.key(7, 4, false).unwrap(), KeyboardReport::default());
        keyboard.focus(false);
        keyboard.device_ready();
        assert_eq!(keyboard.key(7, 4, true), Err(KeyboardError::Pending));
    }
    #[test]
    fn rollover_releases_and_modifiers_keep_their_sides() {
        let mut keyboard = AndroidKeyboard::default();
        keyboard.focus(true);
        keyboard.device_ready();
        assert_eq!(keyboard.key(7, 0xe7, true).unwrap().modifiers, 128);
        for usage in 4..10 {
            keyboard.key(7, usage, true).unwrap();
        }
        assert_eq!(keyboard.key(7, 10, true), Err(KeyboardError::Rollover));
        assert_eq!(keyboard.key(7, 10, false).unwrap(), KeyboardReport::default());
    }
}
