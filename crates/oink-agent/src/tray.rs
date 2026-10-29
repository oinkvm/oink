//! Native tray presentation of this device's accepted card facts.
use crate::device_cards::*;
mod artwork;
pub use artwork::{TrayPaint, svg};
#[cfg(test)]
mod render_tests;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TrayState {
    #[default]
    Default,
    Cursor,
    Locked,
    Game,
    ScreenOff,
    Paused,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TrayLook {
    pub state: TrayState,
    pub solid: bool,
}

impl TrayLook {
    pub fn of_cards(view: &DeviceCardsView) -> Self {
        let Some(card) = view.cards.iter().find(|card| card.is_self) else {
            return Self::default();
        };
        let status = card.status.as_ref();
        let cursor = status.map(|s| s.cursor.icon).unwrap_or(CursorIcon::Unknown);
        // Configured policy survives transient resource faults and pause overlays.
        let screen_off = status.is_some_and(|s| s.screen.configured_mode == Some(SharingMode::Local));
        let game = status.is_some_and(|s| s.game.active == Some(true));
        Self {
            state: if card.pause.as_ref().is_some_and(|p| p.paused) {
                TrayState::Paused
            } else if screen_off {
                TrayState::ScreenOff
            } else if game {
                TrayState::Game
            } else if cursor == CursorIcon::Locked {
                TrayState::Locked
            } else if cursor == CursorIcon::Local {
                TrayState::Cursor
            } else {
                TrayState::Default
            },
            // Unknown/independent focus is not evidence of group focus ownership.
            solid: !screen_off && (matches!(cursor, CursorIcon::Local | CursorIcon::Locked) || game),
        }
    }
}

/// Keep queued native updates ordered across concurrent snapshot delivery and
/// backend restarts. Repeated cards can advance revision without repainting.
#[derive(Debug, Default)]
pub struct TrayPresentation {
    pub look: TrayLook,
    version: Option<(String, u64)>,
}

impl TrayPresentation {
    pub fn update(&mut self, view: &DeviceCardsView) -> bool {
        if self
            .version
            .as_ref()
            .is_some_and(|(stream, revision)| stream == &view.stream && *revision >= view.revision.0)
        {
            return false;
        }
        self.version = Some((view.stream.clone(), view.revision.0));
        let next = TrayLook::of_cards(view);
        let changed = next != self.look;
        self.look = next;
        changed
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn mac_images(look: TrayLook) -> [&'static [u8]; 3] {
    macro_rules! images {
        ($name:literal) => {
            [
                include_bytes!(concat!("tray/images/mac-", $name, "-16.png")).as_slice(),
                include_bytes!(concat!("tray/images/mac-", $name, "-32.png")).as_slice(),
                include_bytes!(concat!("tray/images/mac-", $name, "-48.png")).as_slice(),
            ]
        };
    }
    match (look.state, look.solid) {
        (TrayState::Default, false) => images!("default-outline"),
        (TrayState::Default, true) => images!("default-solid"),
        (TrayState::Cursor, false) => images!("cursor-outline"),
        (TrayState::Cursor, true) => images!("cursor-solid"),
        (TrayState::Locked, false) => images!("locked-outline"),
        (TrayState::Locked, true) => images!("locked-solid"),
        (TrayState::Game, false) => images!("game-outline"),
        (TrayState::Game, true) => images!("game-solid"),
        (TrayState::ScreenOff, false) => images!("screenOff-outline"),
        (TrayState::ScreenOff, true) => images!("screenOff-solid"),
        (TrayState::Paused, false) => images!("paused-outline"),
        (TrayState::Paused, true) => images!("paused-solid"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cards() -> DeviceCardsView {
        let mut projector = Projector::new().unwrap();
        let inputs = CardInputs {
            group: None,
            members: vec![CardMember {
                device: oink_foundation::DeviceId([1; 32]).into(),
                name: "This device".into(),
                os: "macos".into(),
                is_self: true,
                ready: true,
                offline_reason: OfflineKind::Disconnected,
                last_seen_ms: None,
                rtt_ms: None,
                live_boot: Some([1; 16]),
                facts: None,
                capabilities: vec![],
            }],
            focus: FocusObservation::default(),
            exit_baseline: false,
            microphone_resolution: Default::default(),
            local_exit_selection_ready: false,
            selected_exit: None,
            exit_claim: None,
            emergency_delivery: None,
        };
        (*projector.update(&inputs, 0).unwrap()).clone()
    }

    #[test]
    fn queued_tray_updates_ignore_old_revisions_and_deduplicate_paints() {
        let mut presentation = TrayPresentation::default();
        let mut view = cards();
        assert!(!presentation.update(&view));
        view.revision.0 += 1;
        view.cards[0].status.as_mut().unwrap().cursor.icon = CursorIcon::Local;
        assert!(presentation.update(&view));
        assert!(!presentation.update(&view));
        view.revision.0 -= 1;
        view.cards[0].status.as_mut().unwrap().cursor.icon = CursorIcon::Remote;
        assert!(!presentation.update(&view));
        assert_eq!(presentation.look.state, TrayState::Cursor);
        view.stream.push_str("-restarted");
        assert!(presentation.update(&view));
        assert_eq!(presentation.look, TrayLook::default());
    }

    #[test]
    fn tray_priority_and_pointer_fill_follow_local_facts() {
        for paused in [false, true] {
            for disabled in [false, true] {
                for active in [None, Some(false), Some(true)] {
                    for cursor in [
                        CursorIcon::Remote,
                        CursorIcon::Local,
                        CursorIcon::Locked,
                        CursorIcon::Unknown,
                    ] {
                        let mut view = cards();
                        let card = &mut view.cards[0];
                        card.pause = Some(LocalPauseCard {
                            action: CardAction::unavailable(CardActionUnavailable::Busy),
                            paused,
                            reason: PauseReason::LocalUser,
                            revision: DecimalU64(1),
                            can_resume: true,
                        });
                        let status = card.status.as_mut().unwrap();
                        status.cursor.icon = cursor;
                        status.screen.configured_mode = Some(if disabled {
                            SharingMode::Local
                        } else {
                            SharingMode::Shared
                        });
                        // Fault overlays must not erase configured screen policy.
                        status.screen.icon = SharingIcon::Fault;
                        status.game.active = active;
                        status.game.preference = Some(GamePreference::On);
                        let expected = TrayLook {
                            state: if paused {
                                TrayState::Paused
                            } else if disabled {
                                TrayState::ScreenOff
                            } else if active == Some(true) {
                                TrayState::Game
                            } else if cursor == CursorIcon::Locked {
                                TrayState::Locked
                            } else if cursor == CursorIcon::Local {
                                TrayState::Cursor
                            } else {
                                TrayState::Default
                            },
                            solid: !disabled
                                && (matches!(cursor, CursorIcon::Local | CursorIcon::Locked) || active == Some(true)),
                        };
                        assert_eq!(
                            TrayLook::of_cards(&view),
                            expected,
                            "paused={paused}, disabled={disabled}, active={active:?}, cursor={cursor:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn tray_ignores_peer_state_and_does_not_invent_unknown_focus() {
        let mut view = cards();
        assert_eq!(TrayLook::of_cards(&view), TrayLook::default());
        let mut peer = view.cards[0].clone();
        peer.is_self = false;
        peer.status.as_mut().unwrap().cursor.icon = CursorIcon::Locked;
        peer.status.as_mut().unwrap().game.active = Some(true);
        view.cards.insert(0, peer);
        assert_eq!(TrayLook::of_cards(&view), TrayLook::default());
        view.cards.retain(|c| !c.is_self);
        assert_eq!(TrayLook::of_cards(&view), TrayLook::default());
    }
}
