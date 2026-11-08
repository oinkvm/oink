//! Ticket gate for macOS's asynchronous WebKit presentation barrier.
use std::collections::HashMap;

#[derive(Default)]
pub(crate) struct PaintGate {
    next: u64,
    pending: HashMap<String, (u64, bool)>,
}
impl PaintGate {
    pub(crate) fn begin(&mut self, label: &str) -> Option<u64> {
        if self.pending.contains_key(label) {
            return None;
        }
        self.next = self.next.checked_add(1).expect("paint ticket exhausted");
        self.pending.insert(label.into(), (self.next, false));
        Some(self.next)
    }
    pub(crate) fn pending(&self, label: &str) -> bool {
        self.pending.contains_key(label)
    }
    pub(crate) fn unhidden(&mut self, label: &str, ticket: u64) {
        if let Some((current, unhidden)) = self.pending.get_mut(label)
            && *current == ticket
        {
            *unhidden = true;
        }
    }
    pub(crate) fn finish(&mut self, label: &str, ticket: u64) -> bool {
        if self.pending.get(label) != Some(&(ticket, true)) {
            return false;
        }
        self.pending.remove(label);
        true
    }
    pub(crate) fn cancel(&mut self, label: &str) {
        self.pending.remove(label);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reopen_requests_coalesce_until_the_paint_completes() {
        let mut gate = PaintGate::default();
        let ticket = gate.begin("main").unwrap();
        assert!(gate.pending("main"));
        for _ in 0..3 {
            assert!(gate.begin("main").is_none());
        }
        gate.unhidden("main", ticket);
        assert!(gate.finish("main", ticket));
        assert!(!gate.finish("main", ticket));
        assert!(!gate.pending("main"));
        assert_ne!(gate.begin("main"), Some(ticket));
    }
    #[test]
    fn first_open_and_every_reopen_require_unhide_then_a_drawn_frame() {
        let mut gate = PaintGate::default();
        for _ in 0..5 {
            let ticket = gate.begin("main").unwrap();
            assert!(!gate.finish("main", ticket), "must not reveal a still-hidden WebView");
            gate.unhidden("main", ticket);
            assert!(gate.pending("main"), "unhiding alone must not reveal the window");
            assert!(
                gate.finish("main", ticket),
                "only WebKit's drawn-frame completion reveals"
            );
        }
    }
    #[test]
    fn close_cancels_old_paint_even_if_the_label_is_reused() {
        let mut gate = PaintGate::default();
        let old = gate.begin("main").unwrap();
        gate.cancel("main");
        let current = gate.begin("main").unwrap();
        assert!(!gate.finish("main", old));
        assert!(gate.pending("main"));
        gate.unhidden("main", current);
        assert!(gate.finish("main", current));
    }
}
