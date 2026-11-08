//! Hidden screens retain geometry and focus; local native siblings remain a
//! valid recovery path if an OS or application places the cursor there.
use super::*;
impl Router {
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
