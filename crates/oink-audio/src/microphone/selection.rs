//! Source selection policy. Native inspection and system-default writes stay outside it.
use super::InputDevice;

#[derive(Clone, Debug)]
pub struct InputCandidate {
    pub input: InputDevice,
    pub physical: bool,
    pub built_in: bool,
    pub forwarding_loopback: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionError {
    Loopback,
    Unavailable,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputSelection {
    pub input: String,
    pub switch_from: Option<String>,
    pub reason: &'static str,
}
#[derive(Default, Debug)]
pub struct DefaultInputHistory {
    pub current: Option<InputDevice>,
    previous_physical: Option<String>,
}
impl DefaultInputHistory {
    pub fn observe(&mut self, current: Option<InputDevice>, candidates: &[InputCandidate]) {
        if let Some(input) = &current
            && candidates.iter().any(|c| c.input.id == input.id && eligible(c))
        {
            self.previous_physical = Some(input.id.clone());
        }
        self.current = current;
    }
    pub fn select(
        &self,
        explicit: Option<&str>,
        candidates: &[InputCandidate],
    ) -> Result<InputSelection, SelectionError> {
        let id = explicit
            .or_else(|| self.current.as_ref().map(|input| input.id.as_str()))
            .ok_or(SelectionError::Unavailable)?;
        if explicit.is_some() && (id.ends_with(super::MAC_INPUT_UID) || id.ends_with("com.across.microphone.input.v1"))
        {
            return Err(SelectionError::Loopback);
        }
        let input = candidates
            .iter()
            .find(|c| c.input.id == id)
            .ok_or(SelectionError::Unavailable)?;
        if !input.forwarding_loopback && !input.input.is_forwarding_loopback() {
            return Ok(InputSelection {
                input: id.into(),
                switch_from: None,
                reason: "selected_input",
            });
        }
        if explicit.is_some() {
            return Err(SelectionError::Loopback);
        }
        let previous = self
            .previous_physical
            .as_deref()
            .and_then(|id| candidates.iter().find(|c| c.input.id == id && eligible(c)));
        let builtin = candidates
            .iter()
            .filter(|c| c.built_in && eligible(c))
            .min_by_key(|c| &c.input.id);
        let first = candidates.iter().filter(|c| eligible(c)).min_by_key(|c| &c.input.id);
        let (replacement, reason) = previous
            .map(|c| (c, "previous_default"))
            .or_else(|| builtin.map(|c| (c, "built_in")))
            .or_else(|| first.map(|c| (c, "first_physical")))
            .ok_or(SelectionError::Loopback)?;
        Ok(InputSelection {
            input: replacement.input.id.clone(),
            switch_from: Some(id.into()),
            reason,
        })
    }
}
fn eligible(candidate: &InputCandidate) -> bool {
    candidate.physical && !candidate.forwarding_loopback && !candidate.input.is_forwarding_loopback()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn candidate(id: &str, physical: bool, built_in: bool) -> InputCandidate {
        InputCandidate {
            input: InputDevice {
                id: id.into(),
                name: id.into(),
            },
            physical,
            built_in,
            forwarding_loopback: false,
        }
    }
    fn loopback() -> InputCandidate {
        candidate(&format!("coreaudio:{}", super::super::MAC_INPUT_UID), false, false)
    }
    #[test]
    fn prefers_previous_default_then_builtin_then_stable_physical_id() {
        let own = loopback();
        let mut candidates = vec![
            own.clone(),
            candidate("usb-z", true, false),
            candidate("builtin", true, true),
            candidate("usb-a", true, false),
        ];
        let mut history = DefaultInputHistory::default();
        history.observe(Some(candidates[1].input.clone()), &candidates);
        history.observe(Some(own.input.clone()), &candidates);
        let choice = history.select(None, &candidates).unwrap();
        assert_eq!(choice.input, "usb-z");
        assert_eq!(choice.reason, "previous_default");
        candidates.remove(1);
        assert_eq!(history.select(None, &candidates).unwrap().input, "builtin");
        candidates.retain(|c| !c.built_in);
        assert_eq!(history.select(None, &candidates).unwrap().input, "usb-a");
        candidates.reverse();
        assert_eq!(history.select(None, &candidates).unwrap().input, "usb-a");
    }
    #[test]
    fn explicit_loopback_and_no_alternative_refuse_without_a_system_write() {
        let own = loopback();
        let candidates = vec![own.clone(), candidate("third-party-virtual", false, false)];
        let mut history = DefaultInputHistory::default();
        history.observe(Some(own.input.clone()), &candidates);
        assert_eq!(history.select(None, &candidates), Err(SelectionError::Loopback));
        let hardware = candidate("hardware", true, true);
        let candidates = [candidates, vec![hardware]].concat();
        assert_eq!(
            history.select(Some(&own.input.id), &candidates),
            Err(SelectionError::Loopback)
        );
        let explicit = history.select(Some("hardware"), &candidates).unwrap();
        assert!(explicit.switch_from.is_none());
    }
    #[test]
    fn default_change_reselects_hardware_without_remembering_the_loopback() {
        let own = loopback();
        let hardware = candidate("hardware", true, true);
        let candidates = vec![own.clone(), hardware.clone()];
        let mut history = DefaultInputHistory::default();
        history.observe(Some(own.input), &candidates);
        assert!(history.select(None, &candidates).unwrap().switch_from.is_some());
        history.observe(Some(hardware.input), &candidates);
        let selected = history.select(None, &candidates).unwrap();
        assert_eq!(selected.input, "hardware");
        assert!(selected.switch_from.is_none());
    }
}
