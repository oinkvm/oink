//! Capture decisions use the bounded typed input queue, not tracing: the native
//! tap owner deliberately has NoSubscriber so formatting never delays input.
//! Only transitions are queued. Bundle IDs are copied on the ordinary observer;
//! screen/device hashing, strings and wire projection belong to the drain.
use super::{Event, Recorder, handoff::Screen, input_trace::InputTrace};
use oink_foundation::{DeviceId, display::DisplayInfo};
use oink_input::pointer_capture::{CaptureEvidence, CaptureSample};
use oink_routing::{GameMode, router::GameModeStatus};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Application {
    pid: u64,
    bytes: [u8; 256],
    len: u16,
}
impl Application {
    pub(crate) fn new(pid: u64, bundle: &str) -> Option<Self> {
        if bundle.is_empty()
            || bundle.len() > 256
            || !bundle.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        {
            return None;
        }
        let mut bytes = [0; 256];
        bytes[..bundle.len()].copy_from_slice(bundle.as_bytes());
        Some(Self {
            pid,
            bytes,
            len: bundle.len() as u16,
        })
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Raw {
    holder: DeviceId,
    epoch: u64,
    sample_scope: Option<(u64, DeviceId)>,
    mode: GameMode,
    active: bool,
    detected: bool,
    passive: bool,
    reason: &'static str,
    sample: Option<CaptureSample>,
    application: Option<Application>,
    display: Option<Screen>,
    evidence: CaptureEvidence,
}
#[derive(Clone, Copy, PartialEq, Eq)]
struct Key {
    scope: (u64, DeviceId),
    mode: GameMode,
    active: bool,
    detected: bool,
    passive: bool,
    reason: &'static str,
    foreground: Option<u64>,
    hidden: Option<bool>,
    application_hidden: Option<bool>,
    confined: bool,
    warp_locked: bool,
    enter_pending: bool,
    exit_pending: bool,
    display: Option<Screen>,
    application: Option<Application>,
}
impl Raw {
    pub(crate) fn new(
        status: &GameModeStatus,
        sample: Option<CaptureSample>,
        application: Option<Application>,
        local: DeviceId,
        displays: &[DisplayInfo],
        evidence: CaptureEvidence,
        passive: bool,
    ) -> Self {
        let display = sample
            .and_then(|s| displays.iter().find(|d| d.bounds.contains(s.position)))
            .and_then(|d| Screen::new(local, &d.key));
        Self {
            holder: status.holder,
            epoch: status.epoch,
            sample_scope: Some((status.epoch, status.holder)),
            mode: status.mode,
            active: status.active,
            detected: status.detected,
            passive,
            reason: if status.detected && !passive {
                "hidden_during_remote_absolute_motion"
            } else {
                evidence.reason
            },
            sample,
            application: application.filter(|a| sample.is_some_and(|s| s.foreground == a.pid)),
            display,
            evidence,
        }
    }
    fn key(&self) -> Key {
        Key {
            scope: (self.epoch, self.holder),
            mode: self.mode,
            active: self.active,
            detected: self.detected,
            passive: self.passive,
            reason: self.reason,
            foreground: self.sample.map(|s| s.foreground),
            hidden: self.sample.map(|s| s.hidden),
            application_hidden: self.sample.and_then(|s| s.application_hidden),
            confined: self.sample.is_some_and(|s| s.confined),
            warp_locked: self.evidence.warp_locked,
            enter_pending: self.evidence.enter_pending,
            exit_pending: self.evidence.exit_pending,
            display: self.display,
            application: self.application,
        }
    }
    pub(super) fn export(self, recorder: &Recorder) -> Event {
        let mut fields = vec![
            ("epoch".into(), self.epoch as f64),
            ("active".into(), f64::from(self.active)),
            ("detected".into(), f64::from(self.detected)),
            ("passive_detected".into(), f64::from(self.passive)),
            ("sample_available".into(), f64::from(self.sample.is_some())),
            ("warp_lock".into(), f64::from(self.evidence.warp_locked)),
            (
                "locked".into(),
                f64::from(self.evidence.warp_locked || self.passive || self.sample.is_some_and(|s| s.confined)),
            ),
            ("stationary_us".into(), self.evidence.stationary_us as f64),
            ("movement_samples".into(), f64::from(self.evidence.movement_samples)),
            ("enter_pending".into(), f64::from(self.evidence.enter_pending)),
            ("exit_pending".into(), f64::from(self.evidence.exit_pending)),
        ];
        let mut labels = vec![
            ("reason".into(), self.reason.into()),
            ("holder".into(), recorder.anonymizer().device(self.holder)),
            (
                "mode".into(),
                match self.mode {
                    GameMode::Auto => "auto",
                    GameMode::On => "on",
                    GameMode::Off => "off",
                }
                .into(),
            ),
        ];
        if let Some(s) = self.sample {
            fields.extend([
                ("hidden".into(), f64::from(s.hidden)),
                ("confined".into(), f64::from(s.confined)),
                ("foreground_pid".into(), s.foreground as f64),
                ("motion_serial".into(), f64::from(s.motion_serial)),
                ("x".into(), s.position.x),
                ("y".into(), s.position.y),
            ]);
            if let Some(hidden) = s.application_hidden {
                fields.push(("application_hidden".into(), f64::from(hidden)));
            }
        }
        if let Some((epoch, holder)) = self.sample_scope {
            fields.push(("sample_epoch".into(), epoch as f64));
            labels.push(("sample_holder".into(), recorder.anonymizer().device(holder)));
        }
        if let Some(app) = self.application {
            labels.push((
                "foreground_bundle_id".into(),
                std::str::from_utf8(&app.bytes[..app.len as usize]).unwrap_or("").into(),
            ));
        }
        if let Some(display) = self.display {
            labels.push(("screen".into(), display.alias(recorder)));
        }
        // Reuse the existing structured log wire vocabulary; no subscriber,
        // generic Debug payload, new message kind or schema version is needed.
        Event::Log {
            module: "oink_pointer_capture".into(),
            line: 0,
            level: 2,
            fields,
            labels,
        }
    }
}

#[derive(Default)]
pub(crate) struct State {
    current: Option<Raw>,
    recorded: Option<Key>,
}
impl State {
    pub(crate) fn observe(&mut self, raw: Raw, trace: Option<&InputTrace>) {
        let key = raw.key();
        self.current = Some(raw);
        if self.recorded == Some(key) {
            return;
        }
        self.recorded = Some(key);
        if let Some(trace) = trace {
            trace.pointer_capture(raw);
        }
    }
    pub(crate) fn game(&mut self, status: &GameModeStatus, paused: bool, trace: Option<&InputTrace>) {
        let Some(mut raw) = self.current else {
            return;
        };
        if (raw.epoch, raw.holder, raw.mode, raw.active, raw.detected)
            == (status.epoch, status.holder, status.mode, status.active, status.detected)
        {
            return;
        }
        raw.reason = if (raw.epoch, raw.holder) != (status.epoch, status.holder) {
            "focus_changed"
        } else if paused {
            "paused"
        } else if raw.mode != status.mode {
            match status.mode {
                GameMode::Auto => "auto_enabled",
                GameMode::On => "manual_on",
                GameMode::Off => "manual_off",
            }
        } else if status.detected {
            "capture_entered"
        } else {
            "capture_exited"
        };
        raw.holder = status.holder;
        raw.epoch = status.epoch;
        raw.mode = status.mode;
        raw.active = status.active;
        raw.detected = status.detected;
        self.observe(raw, trace);
    }
}
