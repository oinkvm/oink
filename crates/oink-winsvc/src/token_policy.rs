//! Elevation is a user choice, not evidence of a service identity.

pub(crate) fn service_account(sid: &[u8]) -> bool {
    // TokenUser supplies a validated SID. NT AUTHORITY service identities include
    // SYSTEM/LocalService/NetworkService and per-service / application-pool SIDs.
    sid.len() >= 12
        && sid[2..8] == [0, 0, 0, 0, 0, 5]
        && (matches!(u32::from_le_bytes(sid[8..12].try_into().unwrap()), 80 | 82)
            || (sid[1] == 1 && matches!(u32::from_le_bytes(sid[8..12].try_into().unwrap()), 18..=20)))
}

pub(crate) fn interactive_user(session: u32, service: bool) -> bool {
    session != 0 && !service
}

pub(crate) fn interactive_ui(uiaccess: u32, session: u32, service: bool) -> bool {
    interactive_user(session, service) && uiaccess == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_user_elevation_modes_are_allowed_but_service_and_backend_tokens_are_not() {
        // Windows TOKEN_ELEVATION_TYPE: Default=1, Full=2, Limited=3.
        // Elevation is deliberately absent from the production authorization seam.
        for (kind, elevated) in [(3, false), (1, false), (1, true), (2, true)] {
            for uiaccess in [0, 1] {
                for session in [0, 1, 8] {
                    for service in [false, true] {
                        assert_eq!(
                            interactive_ui(uiaccess, session, service),
                            uiaccess == 0 && session != 0 && !service,
                            "type={kind}, elevated={elevated}, uiaccess={uiaccess}, session={session}, service={service}"
                        );
                        assert_eq!(interactive_user(session, service), session != 0 && !service);
                    }
                }
            }
        }
    }

    #[test]
    fn service_user_sids_are_not_interactive_accounts() {
        for (rid, count, expected) in [
            (18u32, 1, true),
            (19, 1, true),
            (20, 1, true),
            (80, 6, true),
            (82, 6, true),
            (21, 5, false),
            (32, 2, false),
        ] {
            let mut sid = vec![1, count, 0, 0, 0, 0, 0, 5];
            sid.extend(rid.to_le_bytes());
            sid.resize(8 + usize::from(count) * 4, 0);
            assert_eq!(service_account(&sid), expected, "RID {rid}");
        }
    }
}
