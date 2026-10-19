//! Interactive traffic is application-paced (audio codec clock / input events).
//! Wi-Fi delivery/ACK bursts must not shrink its window into another stall.
//! This is deliberately not an Internet congestion controller: the finite
//! window bounds outstanding data, but neither loss nor ECN makes it back off.
//! Keep bulk transfers on their separately paced, congestion-controlled path.
use quinn::congestion::{Controller, ControllerFactory};
use std::{any::Any, sync::Arc, time::Instant};

/// 8 MiB permits about 22 seconds at 3 Mbit/s without ACK-driven admission.
/// This is an in-flight guard, not an application queue or a bitrate target.
/// Quinn still enforces flow control, path validation, socket readiness, its
/// RTT/window pacer, loss detection, PTO and reliable stream retransmission.
pub(crate) const INTERACTIVE_WINDOW: u64 = 8 * 1024 * 1024;

#[derive(Debug, Default)]
pub(crate) struct FixedWindowConfig;

impl ControllerFactory for FixedWindowConfig {
    fn build(self: Arc<Self>, _now: Instant, _current_mtu: u16) -> Box<dyn Controller> {
        Box::new(FixedWindow)
    }
}

#[derive(Clone)]
struct FixedWindow;

impl Controller for FixedWindow {
    fn on_congestion_event(&mut self, _now: Instant, _sent: Instant, _persistent: bool, _lost_bytes: u64) {
        // Quinn has already recorded loss/ECN and schedules reliable recovery
        // independently. Only changing the congestion window is suppressed.
    }

    fn on_mtu_update(&mut self, _new_mtu: u16) {}

    fn window(&self) -> u64 {
        INTERACTIVE_WINDOW
    }

    fn initial_window(&self) -> u64 {
        INTERACTIVE_WINDOW
    }

    fn clone_box(&self) -> Box<dyn Controller> {
        Box::new(self.clone())
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loss_ecn_persistent_congestion_and_path_changes_do_not_reduce_guard() {
        let now = Instant::now();
        let mut controller = Arc::new(FixedWindowConfig).build(now, 1200);
        assert_eq!(controller.initial_window(), INTERACTIVE_WINDOW);
        for (persistent, lost_bytes) in [(false, 1200), (false, 0), (true, INTERACTIVE_WINDOW)] {
            controller.on_sent(now, INTERACTIVE_WINDOW, 10);
            controller.on_congestion_event(now, now, persistent, lost_bytes);
            controller.on_end_acks(now, 0, false, Some(10));
            controller.on_mtu_update(1400);
            assert_eq!(controller.window(), INTERACTIVE_WINDOW);
            assert_eq!(controller.clone_box().window(), INTERACTIVE_WINDOW);
        }
        // Stay below Quinn's u32::MAX threshold that would disable its pacer.
        assert!(controller.window() < u64::from(u32::MAX));
    }
}
