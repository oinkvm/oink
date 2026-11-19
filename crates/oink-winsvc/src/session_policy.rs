//! Launch eligibility never implies permission to use physical-console input.
pub(crate) struct Session {
    pub id: u32,
    pub physical_console: u32,
    pub active: bool,
    pub protocol: Option<u16>,
}
impl Session {
    pub(crate) fn console(&self) -> bool {
        self.id != 0
            && self.id != u32::MAX
            && self.id == self.physical_console
            && self.active
            && self.protocol == Some(0)
    }
    pub(crate) fn launch_allowed(&self) -> bool {
        self.id != 0 && self.id != u32::MAX && self.active && (self.console() || self.protocol == Some(2))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn active_rdp_can_launch_but_never_obtains_console_authority() {
        let mut s = Session {
            id: 2,
            physical_console: 1,
            active: true,
            protocol: Some(2),
        };
        assert!(s.launch_allowed());
        assert!(!s.console());
        s.physical_console = 2;
        assert!(!s.console(), "console ID alone does not authorize RDP");
        s.protocol = Some(0);
        assert!(s.console());
        assert!(s.launch_allowed());
        s.active = false;
        assert!(!s.console());
        assert!(!s.launch_allowed());
        s.protocol = Some(2);
        assert!(!s.launch_allowed(), "disconnected RDP cannot launch");
    }
    #[test]
    fn unknown_and_invalid_sessions_fail_closed() {
        for id in [0, u32::MAX, 2] {
            let s = Session {
                id,
                physical_console: id,
                active: true,
                protocol: None,
            };
            assert!(!s.console());
            assert!(!s.launch_allowed());
        }
        for id in [0, u32::MAX] {
            let s = Session {
                id,
                physical_console: id,
                active: true,
                protocol: Some(2),
            };
            assert!(!s.launch_allowed());
        }
    }
}
