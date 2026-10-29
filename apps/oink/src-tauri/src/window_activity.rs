//! Native visibility transitions and bounded replay of complete state snapshots.
use std::collections::HashMap;

#[derive(Default)]
struct Window {
    visible: bool,
    presented: bool,
    dirty: bool,
}

pub(crate) struct Activity<T> {
    windows: HashMap<String, Window>,
    latest: Option<T>,
}

impl<T> Default for Activity<T> {
    fn default() -> Self {
        Self {
            windows: HashMap::new(),
            latest: None,
        }
    }
}

impl<T: Clone> Activity<T> {
    /// Initial invisible windows must finish their DOM-ready handshake before
    /// their webview is hidden. Once presented, native visibility follows every
    /// minimize/hide and restore transition, never focus or a polling clock.
    pub fn visibility(&mut self, label: &str, visible: bool) -> (Option<bool>, Option<T>) {
        let window = self.windows.entry(label.into()).or_default();
        let changed = window.visible != visible;
        window.visible = visible;
        let native = (changed && (visible || window.presented)).then_some(visible);
        window.presented |= visible;
        let replay = if visible && window.dirty {
            window.dirty = false;
            self.latest.clone()
        } else {
            None
        };
        (native, replay)
    }

    pub fn snapshot(&mut self, value: T) {
        self.latest = Some(value);
        for window in self.windows.values_mut() {
            window.dirty = !window.visible;
        }
    }

    pub fn visible(&self, label: &str) -> bool {
        // Before the first native observation, allow normal startup delivery.
        self.windows.get(label).is_none_or(|window| window.visible)
    }

    pub fn closed(&mut self, label: &str) {
        self.windows.remove(label);
        if self.windows.is_empty() {
            self.latest = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_hidden_dom_can_complete_before_native_hiding() {
        let mut state = Activity::<u32>::default();
        assert!(state.visible("main"));
        assert_eq!(state.visibility("main", false), (None, None));
        state.snapshot(1);
        assert_eq!(state.visibility("main", true), (Some(true), Some(1)));
        assert_eq!(state.visibility("main", true), (None, None));
    }

    #[test]
    fn minimize_coalesces_to_latest_and_restore_replays_once() {
        let mut state = Activity::default();
        state.visibility("main", true);
        assert_eq!(state.visibility("main", false), (Some(false), None));
        for value in 0..1000 {
            state.snapshot(value);
        }
        assert!(!state.visible("main"));
        assert_eq!(state.visibility("main", false), (None, None));
        assert_eq!(state.visibility("main", true), (Some(true), Some(999)));
        assert_eq!(state.visibility("main", true), (None, None));
        state.snapshot(1000);
        assert_eq!(state.visibility("main", false), (Some(false), None));
        assert_eq!(state.visibility("main", true), (Some(true), None));
    }

    #[test]
    fn hidden_main_does_not_pause_panel_or_replay_into_recreated_window() {
        let mut state = Activity::default();
        state.visibility("main", true);
        state.visibility("panel", true);
        state.visibility("main", false);
        state.snapshot(4);
        assert!(state.visible("panel"));
        assert!(!state.visible("main"));
        assert_eq!(state.visibility("panel", true), (None, None));
        state.closed("main");
        assert_eq!(state.visibility("main", true), (Some(true), None));
        state.closed("main");
        state.closed("panel");
        assert!(state.latest.is_none());
    }
}
