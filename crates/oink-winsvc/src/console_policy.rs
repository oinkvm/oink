//! Pure authorization policy. Observations come from WTS/token/local SAM, never clients.
use crate::prelogin_route::Identity;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Account {
    NoUser,
    Wsi(Vec<u8>),
    User(Vec<u8>),
    Unknown,
}

pub(crate) fn allows(origin: u32, active: u32, account: &Account, owner: &[u8]) -> bool {
    origin != 0
        && origin != u32::MAX
        && active != 0
        && active != u32::MAX
        && match account {
            Account::NoUser | Account::Wsi(_) => true,
            Account::User(sid) => origin == active && sid == owner,
            Account::Unknown => false,
        }
}

pub(crate) fn wsi_sid(token: &[u8], local_sam: &[u8], normal_account: bool) -> bool {
    // Exact local SAM SID, not a globally assumed RID or caller's name.
    normal_account
        && token == local_sam
        && token.len() == 28
        && token[..8] == [1, 5, 0, 0, 0, 0, 0, 5]
        && token[8..12] == 21u32.to_le_bytes()
}

pub(crate) fn user_source(
    expected: Identity,
    actual: Identity,
    expected_sid: &[u8],
    sid: &[u8],
    uiaccess: bool,
    protected: bool,
    alive: bool,
) -> bool {
    expected.accepts(
        actual,
        uiaccess && expected_sid == sid && !crate::token_policy::service_account(sid),
        protected,
        alive,
    )
}

#[derive(Default)]
pub(crate) struct ConsoleOwner(pub Option<u32>);
impl ConsoleOwner {
    pub(crate) fn observe(&mut self, active: u32, account: &Account) {
        if active == 0 || active == u32::MAX {
            return;
        }
        match account {
            Account::User(_) => self.0 = Some(active),
            Account::Unknown => self.0 = None,
            Account::NoUser | Account::Wsi(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sid(rid: u32) -> Vec<u8> {
        let mut result = vec![1, 5, 0, 0, 0, 0, 0, 5];
        for n in [21u32, 11, 22, 33, rid] {
            result.extend(n.to_le_bytes());
        }
        result
    }
    #[test]
    fn only_local_sam_wsi_sid_is_whitelisted_not_name_rid_or_arbitrary_system() {
        let wsi = sid(1042);
        assert!(wsi_sid(&wsi, &wsi, true));
        assert!(!wsi_sid(&sid(1001), &wsi, true));
        assert!(!wsi_sid(&wsi, &wsi, false));
        let mut domain = wsi.clone();
        domain[12] ^= 1;
        assert!(!wsi_sid(&domain, &wsi, true));
        for unknown in [vec![], vec![0; 28], vec![1, 1, 0, 0, 0, 0, 0, 5, 18, 0, 0, 0]] {
            assert!(!wsi_sid(&unknown, &unknown, true));
        }
    }
    #[test]
    fn cross_session_is_only_for_an_allowlisted_temporary_console() {
        let owner = sid(1000);
        for account in [Account::NoUser, Account::Wsi(sid(1042))] {
            assert!(allows(1, 2, &account, &owner));
        }
        assert!(allows(1, 1, &Account::User(owner.clone()), &owner));
        for account in [Account::Unknown, Account::User(sid(1002)), Account::User(owner.clone())] {
            assert!(!allows(1, 2, &account, &owner));
        }
        assert!(!allows(1, 1, &Account::User(sid(1002)), &owner));
        for invalid in [0, u32::MAX] {
            assert!(!allows(1, invalid, &Account::NoUser, &owner));
        }
    }
    #[test]
    fn fast_user_switch_revokes_previous_origin_even_without_a_backend() {
        let mut last = ConsoleOwner::default();
        last.observe(1, &Account::User(sid(1000)));
        assert_eq!(last.0, Some(1));
        last.observe(u32::MAX, &Account::Unknown);
        last.observe(2, &Account::Wsi(sid(1042)));
        assert_eq!(last.0, Some(1));
        last.observe(3, &Account::User(sid(1002)));
        last.observe(2, &Account::NoUser);
        assert_eq!(last.0, Some(3));
        last.observe(4, &Account::Unknown);
        assert_eq!(last.0, None);
    }
    #[test]
    fn uiaccess_and_protected_path_do_not_replace_broker_child_identity() {
        let child = Identity {
            pid: 42,
            session: 1,
            created: 20,
            parent: 7,
            parent_created: 10,
        };
        let owner = sid(1000);
        assert!(user_source(child, child, &owner, &owner, true, true, true));
        for (uiaccess, protected, alive) in [(false, true, true), (true, false, true), (true, true, false)] {
            assert!(!user_source(child, child, &owner, &owner, uiaccess, protected, alive));
        }
        assert!(!user_source(child, child, &owner, &sid(1002), true, true, true));
        for other in [
            Identity { pid: 43, ..child },
            Identity { created: 21, ..child },
            Identity { session: 2, ..child },
            Identity { parent: 8, ..child },
            Identity {
                parent_created: 11,
                ..child
            },
        ] {
            assert!(!user_source(child, other, &owner, &owner, true, true, true));
        }
    }
}
