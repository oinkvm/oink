//! Desktop notifications run independently of the UI and owner publications.
use super::*;
use crate::api::{AgentApi, Notification, NotificationLevel, NotificationReceipt, ScreenKey};
use oink_foundation::{Point, session::Boot};
use oink_locator::notification::{self, Click, NotificationOverlay, Style, Toast};
use oink_protocol::v1::{
    Message, RawValue as V, dto,
    schema::{WireDto, WireValue},
};
use std::collections::HashMap;

const CAPABILITY: &str = "oink.notify/1";
const MICROPHONE_CAPABILITY: &str = "oink.notify.microphone/1";
/// A rendering host keeps one slot per state family, independent of origin device.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Family {
    Game,
    Pointer,
    Keyboard,
    Mouse,
    Microphone,
}
impl Family {
    fn of(kind: u8) -> Option<Self> {
        match kind {
            1 | 2 => Some(Self::Game),
            3 | 6 => Some(Self::Pointer),
            4 => Some(Self::Keyboard),
            5 => Some(Self::Mouse),
            7 | 8 => Some(Self::Microphone),
            _ => None,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Undo {
    GameOff,
    PointerUnlock,
    KeyboardUnlock,
    MouseEnable,
}
#[derive(Clone, Debug)]
struct Pending {
    level: NotificationLevel,
    action: Option<Undo>,
    destination: DeviceId,
    boot: Option<Boot>,
    surface: Option<u64>,
    expires: Option<Instant>,
}
#[derive(Clone, Debug)]
struct Origin {
    level: NotificationLevel,
    expires: Option<Instant>,
    peer: DeviceId,
    boot: Option<Boot>,
    id: u64,
}
#[derive(Clone, Debug)]
struct FamilySlot {
    surface: u64,
    origin: Origin,
}
#[derive(Debug, Default)]
pub(super) struct Notifications {
    stopped: bool,
    microphone_requests: HashMap<String, bool>,
    overlay: Option<NotificationOverlay>,
    next: u64,
    pending: HashMap<u64, Pending>,
    origins: HashMap<u64, Origin>,
    last_local: Point,
    game: bool,
    pointer: bool,
    keyboard: bool,
    mouse: bool,
    game_id: Option<u64>,
    pointer_id: Option<u64>,
    // Retain bounded slot identities through clicks/expiry so a replacement can
    // retire a still-exiting frame without stacking or restarting its exit.
    families: HashMap<Family, FamilySlot>,
    keyboard_id: Option<u64>,
    mouse_id: Option<u64>,
}
impl Notifications {
    fn prune(&mut self, now: Instant) {
        self.pending.retain(|_, p| p.expires.is_none_or(|at| at > now));
        self.origins.retain(|_, o| o.expires.is_none_or(|at| at > now));
    }
}

impl Node {
    pub(crate) fn track_microphone_notice(&self, id: &str, muted: bool) {
        lock(&self.inner.notifications)
            .microphone_requests
            .insert(id.into(), muted);
    }
    pub(crate) fn complete_microphone_notice(&self, id: &str, result: &crate::device_cards::DeviceControlResult) {
        let muted = lock(&self.inner.notifications).microphone_requests.remove(id);
        if let Some(muted) = muted
            && matches!(
                result,
                crate::device_cards::DeviceControlResult::Applied | crate::device_cards::DeviceControlResult::NoChange
            )
        {
            let kind = if muted { 7 } else { 8 };
            let request = Notification {
                level: NotificationLevel::Info,
                title: copy(self.notification_language(), kind).into(),
                description: String::new(),
                persistent: false,
                screen: None,
            };
            if let Err(error) = self.raise_notification(request, None, kind) {
                tracing::warn!(%error, "shared microphone notification failed");
            }
        }
    }
    fn notification_overlay(&self, state: &mut Notifications) -> NotificationOverlay {
        state
            .overlay
            .get_or_insert_with(|| {
                if self.inner.native_surfaces {
                    let weak = Arc::downgrade(&self.inner);
                    if let Some(overlay) = NotificationOverlay::platform(move |click| {
                        if let Some(inner) = weak.upgrade() {
                            let node = Node { inner };
                            let runtime = node.inner.rt.clone();
                            runtime.spawn_blocking(move || node.notification_click(click));
                        }
                    }) {
                        return overlay;
                    }
                }
                NotificationOverlay::headless()
            })
            .clone()
    }
    pub(super) fn update_notification_displays(&self, displays: &[DisplayInfo]) {
        if let Some(overlay) = &lock(&self.inner.notifications).overlay {
            overlay.set_displays(displays);
        }
    }
    pub(crate) fn apply_notification_preferences(&self) {
        let settings = self.settings();
        let mut state = lock(&self.inner.notifications);
        if let Some(overlay) = &state.overlay {
            let dark = match settings.theme_mode {
                crate::config::ThemeMode::Dark => true,
                crate::config::ThemeMode::Light => false,
                _ => self.inner.system_dark.load(Ordering::Acquire),
            };
            overlay.set_style(Style {
                dark,
                accent: settings.theme_accent.resolved(settings.theme_mode, dark).rgb(),
            });
        }
        let hidden: Vec<_> = state
            .origins
            .iter()
            .filter_map(|(id, o)| (!settings.notification_level.allows(o.level)).then_some(*id))
            .collect();
        for id in hidden {
            state.origins.remove(&id);
            if let Some(overlay) = &state.overlay {
                overlay.dismiss(id);
            }
        }
        let hidden: Vec<_> = state
            .pending
            .iter()
            .filter_map(|(id, p)| (!settings.notification_level.allows(p.level)).then_some(*id))
            .collect();
        drop(state);
        for id in hidden {
            self.dismiss_notification(id);
        }
    }
    pub(super) fn shutdown_notifications(&self) {
        let pending = {
            let mut s = lock(&self.inner.notifications);
            s.stopped = true;
            s.pending.keys().copied().collect::<Vec<_>>()
        };
        for id in pending {
            self.dismiss_notification(id);
        }
        let mut s = lock(&self.inner.notifications);
        s.origins.clear();
        s.overlay.take();
    }
    fn default_notification_screen(&self) -> Option<ScreenKey> {
        let status = self.input_status().ok()?;
        if status.pointer_focus {
            return None;
        }
        let holder = status.group_focus.holder?;
        if holder == self.id() {
            return None;
        }
        Some(ScreenKey {
            device: holder.into(),
            display: String::new(),
        })
    }
    fn owner_notification_screen(&self) -> api::ApiResult<ScreenKey> {
        let displays = self.current_displays().map_err(|error| api::ApiError::Internal {
            message: error.to_string(),
        })?;
        // A parked cursor on a non-focus owner is not the group pointer. Built-in
        // state changes stay on this owner; use its primary screen in that case.
        let pointer = self
            .input_status()
            .ok()
            .filter(|status| status.pointer_focus)
            .and_then(|_| self.input_backend().cursor_position().ok());
        let display = pointer
            .and_then(|pointer| displays.iter().find(|display| display.bounds.contains(pointer)))
            .or_else(|| displays.iter().find(|display| display.is_primary))
            .or_else(|| displays.first())
            .ok_or_else(|| api::ApiError::Unavailable {
                message: "no local notification screen".into(),
            })?;
        Ok(ScreenKey {
            device: self.id().into(),
            display: display.key.clone(),
        })
    }
    /// Source filtering happens before any native creation, screen lookup or send.
    pub(crate) fn raise_notification(
        &self,
        mut request: Notification,
        action: Option<Undo>,
        kind: u8,
    ) -> api::ApiResult<Option<NotificationReceipt>> {
        if lock(&self.inner.notifications).stopped {
            return Err(api::ApiError::Unavailable {
                message: "notifications stopped".into(),
            });
        }
        if !self.settings().notification_level.allows(request.level) {
            return Ok(None);
        }
        if request.title.len() > 512 || request.description.len() > 2048 || request.title.trim().is_empty() {
            return Err(api::ApiError::InvalidArgument {
                message: "invalid notification text".into(),
            });
        }
        if request
            .screen
            .as_ref()
            .is_some_and(|s| s.display.is_empty() || s.display.len() > 512)
        {
            return Err(api::ApiError::InvalidArgument {
                message: "invalid notification screen".into(),
            });
        }
        if kind != 0 && !matches!(kind, 7 | 8) {
            // Keyboard events are local and pointer/game events identify their
            // owner below. Never route these notices through the generic API's
            // group-pointer default, even when another member requested the edit.
            request.screen = Some(self.owner_notification_screen()?);
        } else if request.screen.is_none() {
            request.screen = self.default_notification_screen();
        }
        let group = self.inner.group.get();
        // Microphone success follows current pointer focus. Unsupported peers
        // remain silent rather than showing a duplicate on the pressing device.
        if matches!(kind, 7 | 8)
            && let Some(screen) = request.screen.as_ref().filter(|s| s.device.0 != self.id())
            && !group
                .and_then(|g| g.peer_agreement(screen.device.0))
                .is_some_and(|a| a.contains(CAPABILITY) && a.contains(MICROPHONE_CAPABILITY))
        {
            return Ok(None);
        }
        let remote = request
            .screen
            .as_ref()
            .filter(|s| s.device.0 != self.id())
            .filter(|s| {
                group
                    .and_then(|g| g.peer_agreement(s.device.0))
                    .is_some_and(|a| a.contains(CAPABILITY))
            })
            .cloned();
        let id = {
            let mut state = lock(&self.inner.notifications);
            state.prune(Instant::now());
            state.next += 1;
            state.next
        };
        if let Some(screen) = remote {
            let mut raw: oink_protocol::v1::RawMap = [
                (1, id.write()),
                (2, V::Text(screen.display.clone())),
                (3, level_number(request.level).write()),
                (4, V::Text(request.title.clone())),
                (5, V::Text(request.description.clone())),
                (6, request.persistent.write()),
                (7, action.is_some().write()),
                (8, if matches!(kind, 7 | 8) { 0u8 } else { kind }.write()),
            ]
            .into();
            if matches!(kind, 7 | 8) {
                raw.insert(9, (kind == 7).write());
            }
            let message =
                Message::NotifyShow(
                    dto::NotifyShow::from_raw(raw).map_err(|_| api::ApiError::InvalidArgument {
                        message: "invalid notification".into(),
                    })?,
                );
            let pending = Pending {
                level: request.level,
                action,
                destination: screen.device.0,
                boot: group.and_then(|g| g.peer_boot(screen.device.0)),
                surface: None,
                expires: (!request.persistent).then(|| Instant::now() + notification::LIFETIME),
            };
            lock(&self.inner.notifications).pending.insert(id, pending);
            if group.is_some_and(|g| g.send_device_control(screen.device.0, message).is_ok()) {
                return Ok(Some(NotificationReceipt {
                    id: id.to_string(),
                    screen: (!screen.display.is_empty()).then_some(screen),
                    remote: true,
                }));
            }
            lock(&self.inner.notifications).pending.remove(&id);
        }
        if matches!(kind, 7 | 8) && request.screen.as_ref().is_some_and(|s| s.device.0 != self.id()) {
            return Ok(None);
        }
        let displays = self.current_displays().map_err(|error| api::ApiError::Internal {
            message: error.to_string(),
        })?;
        let mut state = lock(&self.inner.notifications);
        if self.input_status().ok().is_none_or(|s| s.pointer_focus)
            && let Ok(pos) = self.input_backend().cursor_position()
        {
            state.last_local = pos;
        }
        let requested = request
            .screen
            .as_ref()
            .filter(|s| s.device.0 == self.id())
            .map(|s| s.display.as_str());
        let Some(display) = notification::select_display(&displays, requested, state.last_local).cloned() else {
            return Err(api::ApiError::Unavailable {
                message: "no local notification screen".into(),
            });
        };
        let screen = ScreenKey {
            device: self.id().into(),
            display: display.key.clone(),
        };
        let toast = Toast {
            title: request.title,
            description: request.description,
            undo: action.map(|_| copy(self.notification_language(), 0).into()),
            persistent: request.persistent,
            scale: notification::scale_for(&display),
            display,
        };
        let origin = Origin {
            level: request.level,
            expires: (!request.persistent).then(|| Instant::now() + notification::LIFETIME),
            peer: self.id(),
            boot: None,
            id,
        };
        let (local, retired) = self.insert_notification(&mut state, toast, kind, origin);
        state.pending.insert(
            id,
            Pending {
                level: request.level,
                action,
                destination: self.id(),
                boot: None,
                surface: Some(local),
                expires: (!request.persistent).then(|| Instant::now() + notification::LIFETIME),
            },
        );
        drop(state);
        self.retire_notification_origin(retired);
        self.apply_notification_preferences();
        Ok(Some(NotificationReceipt {
            id: id.to_string(),
            screen: Some(screen),
            remote: false,
        }))
    }
    fn insert_notification(
        &self,
        state: &mut Notifications,
        toast: Toast,
        kind: u8,
        origin: Origin,
    ) -> (u64, Option<Origin>) {
        let family = Family::of(kind);
        let previous = family.and_then(|family| state.families.get(&family).cloned());
        if let Some(previous) = &previous {
            state.origins.remove(&previous.surface);
            if previous.origin.peer == self.id() {
                // A click may have removed the route while its owner edit waits
                // on a transaction. Replacement must retire that pending Undo too.
                state.pending.remove(&previous.origin.id);
            }
        }
        let overlay = self.notification_overlay(state);
        let local = match &previous {
            Some(previous) => overlay.replace(previous.surface, toast),
            None => overlay.push(toast),
        };
        if let Some(family) = family {
            state.families.insert(
                family,
                FamilySlot {
                    surface: local,
                    origin: origin.clone(),
                },
            );
        }
        state.origins.insert(local, origin);
        (local, previous.map(|slot| slot.origin))
    }
    fn retire_notification_origin(&self, origin: Option<Origin>) {
        if let Some(origin) = origin.filter(|origin| origin.peer != self.id()) {
            // A replacement is a dismissal, never an Undo. Retire the sender's
            // pending action over the existing authenticated action channel.
            self.complete_notification_origin(origin, false);
        }
    }
    fn notification_language(&self) -> crate::config::UiLanguage {
        self.settings().ui_language.resolve(&system_language())
    }
    fn notification_click(&self, click: Click) {
        let origin = { lock(&self.inner.notifications).origins.remove(&click.id) };
        let Some(origin) = origin else { return };
        self.complete_notification_origin(origin, click.undo);
    }
    fn complete_notification_origin(&self, origin: Origin, undo: bool) {
        if origin.peer == self.id() {
            self.perform_notification_action(origin.id, undo, self.id());
        } else if let Some(group) = self.inner.group.get()
            && group.peer_boot(origin.peer) == origin.boot
            && group
                .peer_agreement(origin.peer)
                .is_some_and(|a| a.contains(CAPABILITY))
        {
            let message = dto::NotifyAction::from_raw([(1, origin.id.write()), (2, undo.write())].into())
                .expect("typed notification action");
            let _ = group.send_device_control(origin.peer, Message::NotifyAction(message));
        }
    }
    fn perform_notification_action(&self, id: u64, undo: bool, from: DeviceId) {
        let owner_edit = {
            let state = lock(&self.inner.notifications);
            undo && state.pending.get(&id).is_some_and(|pending| {
                matches!(
                    pending.action,
                    Some(Undo::GameOff | Undo::KeyboardUnlock | Undo::MouseEnable)
                )
            })
        };
        if owner_edit {
            // Input transitions can raise a banner before the originating owner
            // edit publishes Facts. Fence that transaction before resolving Undo's
            // normal CAS, then release the gate so its owner worker can acquire it.
            // Resolve the pending action afterward: a release during the fence may
            // already have invalidated this banner's action.
            drop(self.owner_gate());
        }
        let pending = {
            let mut state = lock(&self.inner.notifications);
            if !state.pending.get(&id).is_some_and(|p| {
                p.destination == from
                    && (from == self.id() || self.inner.group.get().and_then(|g| g.peer_boot(from)) == p.boot)
            }) {
                return;
            }
            state.pending.remove(&id)
        };
        let Some(pending) = pending else { return };
        if !undo || pending.expires.is_some_and(|at| Instant::now() >= at) {
            return;
        }
        let result = match pending.action {
            Some(Undo::GameOff) => {
                self.edit_device_option(self.id().into(), dto::LocalOption::GameModePreference, 2u32.write())
            }
            Some(Undo::PointerUnlock) => self.set_focus_locked(false),
            Some(Undo::KeyboardUnlock) => self.set_keyboard_disabled(self.id().into(), false),
            Some(Undo::MouseEnable) => {
                self.edit_device_option(self.id().into(), dto::LocalOption::MouseMode, 0u32.write())
            }
            None => Ok(()),
        };
        if let Err(error) = result {
            tracing::warn!(?error, "notification undo failed");
        }
    }
    fn dismiss_notification(&self, id: u64) {
        self.dismiss_notification_with_motion(id, false);
    }
    fn dismiss_notification_immediately(&self, id: u64) {
        self.dismiss_notification_with_motion(id, true);
    }
    fn dismiss_notification_with_motion(&self, id: u64, immediate: bool) {
        let pending = { lock(&self.inner.notifications).pending.remove(&id) };
        let Some(p) = pending else {
            if immediate {
                let mut state = lock(&self.inner.notifications);
                // Undo consumes Pending before its authoritative restore event.
                // Keep the family slot long enough to remove that exiting frame,
                // but never remove a newer notice from another owner.
                if let Some(surface) = state
                    .families
                    .values()
                    .find(|slot| slot.origin.peer == self.id() && slot.origin.id == id)
                    .map(|slot| slot.surface)
                {
                    state.origins.remove(&surface);
                    if let Some(overlay) = &state.overlay {
                        overlay.remove(surface);
                    }
                }
            }
            return;
        };
        if let Some(local) = p.surface {
            let mut state = lock(&self.inner.notifications);
            state.origins.remove(&local);
            if let Some(overlay) = &state.overlay {
                if immediate {
                    overlay.remove(local);
                } else {
                    overlay.dismiss(local);
                }
            }
        } else if let Some(group) = self.inner.group.get()
            && group.peer_boot(p.destination) == p.boot
            && group
                .peer_agreement(p.destination)
                .is_some_and(|a| a.contains(CAPABILITY))
        {
            let message = dto::NotifyDismiss::from_raw([(1, id.write())].into()).expect("typed notification dismissal");
            let _ = group.send_device_control(p.destination, Message::NotifyDismiss(message));
        }
    }
    pub(crate) fn receive_notification(&self, peer: DeviceId, message: Message) {
        if lock(&self.inner.notifications).stopped {
            return;
        }
        let Some(group) = self.inner.group.get() else { return };
        if !group.peer_agreement(peer).is_some_and(|a| a.contains(CAPABILITY)) {
            return;
        }
        match message {
            Message::NotifyAction(m) => {
                if let (Ok(id), Ok(undo)) = (m.id(), m.undo()) {
                    self.perform_notification_action(id, undo, peer);
                }
            }
            Message::NotifyDismiss(m) => {
                if let Ok(id) = m.id() {
                    let mut state = lock(&self.inner.notifications);
                    let locals: Vec<_> = state
                        .origins
                        .iter()
                        .filter_map(|(local, o)| (o.peer == peer && o.id == id).then_some(*local))
                        .collect();
                    for local in locals {
                        state.origins.remove(&local);
                        if let Some(overlay) = &state.overlay {
                            if state.families.values().any(|slot| slot.surface == local) {
                                overlay.remove(local);
                            } else {
                                overlay.dismiss(local);
                            }
                        }
                    }
                }
            }
            Message::NotifyShow(m) => {
                let read = || -> oink_protocol::v1::Result<_> {
                    Ok((
                        m.id()?,
                        m.display()?.0,
                        m.level()?,
                        m.title()?.0,
                        m.description()?.0,
                        m.persistent()?,
                        m.undo()?,
                        m.kind()?,
                        m.microphone_muted()?,
                    ))
                };
                let Ok((id, key, level, mut title, description, persistent, undo, mut kind, microphone_muted)) = read()
                else {
                    return;
                };
                if let Some(muted) = microphone_muted {
                    if !group
                        .peer_agreement(peer)
                        .is_some_and(|a| a.contains(MICROPHONE_CAPABILITY))
                    {
                        return;
                    }
                    kind = if muted { 7 } else { 8 };
                }
                let level = match level {
                    0 => NotificationLevel::Notice,
                    1 => NotificationLevel::Info,
                    _ => NotificationLevel::Debug,
                };
                if !self.settings().notification_level.allows(level) {
                    return;
                }
                let Ok(displays) = self.current_displays() else { return };
                let pointer = self.input_backend().cursor_position().unwrap_or_default();
                let Some(display) =
                    notification::select_display(&displays, (!key.is_empty()).then_some(key.as_str()), pointer)
                        .cloned()
                else {
                    return;
                };
                let language = self.notification_language();
                if kind > 0 {
                    title = copy(language, kind).into();
                }
                let mut state = lock(&self.inner.notifications);
                state.prune(Instant::now());
                if state
                    .origins
                    .values()
                    .any(|o| o.peer == peer && o.id == id && o.boot == group.peer_boot(peer))
                {
                    return;
                }
                let toast = Toast {
                    title,
                    description,
                    undo: undo.then(|| copy(language, 0).into()),
                    persistent,
                    scale: notification::scale_for(&display),
                    display,
                };
                let origin = Origin {
                    level,
                    expires: (!persistent).then(|| Instant::now() + notification::LIFETIME),
                    peer,
                    boot: group.peer_boot(peer),
                    id,
                };
                let (_, retired) = self.insert_notification(&mut state, toast, kind, origin);
                drop(state);
                self.retire_notification_origin(retired);
                self.apply_notification_preferences();
            }
            _ => {}
        }
    }
    /// Pointer metadata can use a latest snapshot; notification transitions
    /// must use the ordered status events, including unlock/relock in one turn.
    pub(super) fn sync_notifications(&self) {
        let Ok(status) = self.input_status() else { return };
        let mut state = lock(&self.inner.notifications);
        state.prune(Instant::now());
        if let Some(position) = status
            .focus_info
            .as_ref()
            .and_then(|f| f.pointer.as_ref())
            .filter(|p| p.screen.device == self.id())
        {
            state.last_local = position.local;
        }
    }
    pub(super) fn notification_status(&self, event: &StatusEvent) {
        let (kind, on) = match event {
            StatusEvent::LocalGameMode(active) => (1, *active),
            StatusEvent::LocalPointerLock(locked) => (3, *locked),
            StatusEvent::KeyboardDisabled(disabled) => (4, *disabled),
            StatusEvent::MouseDisabled(disabled) => (5, *disabled),
            _ => return,
        };
        let dismiss = {
            let mut state = lock(&self.inner.notifications);
            if state.stopped {
                return;
            }
            let current = match kind {
                1 => &mut state.game,
                3 => &mut state.pointer,
                5 => &mut state.mouse,
                _ => &mut state.keyboard,
            };
            // LocalGameMode is already an authoritative, deduplicated owner
            // transition. Its first event can be an exit from startup On, even
            // though this notification observer never saw an entry event.
            if *current == on && kind != 1 {
                return;
            }
            *current = on;
            // A state-caused banner lives only while that authoritative cause
            // holds. Retire its pending action and native/remote surface together;
            // keeping only a disabled Undo would leave a misleading stale banner.
            if !on {
                match kind {
                    1 => state.game_id.take(),
                    3 => state.pointer_id.take(),
                    5 => state.mouse_id.take(),
                    _ => state.keyboard_id.take(),
                }
            } else {
                None
            }
        };
        if !on && !matches!(kind, 1 | 3) {
            if let Some(id) = dismiss {
                self.dismiss_notification_immediately(id);
            }
            return;
        }
        // Kind 6 is owner-local, like kind 5: it never changes the frozen wire
        // notification kind vocabulary (owner notices select a local screen).
        let notice = match (kind, on) {
            (1, false) => 2,
            (3, false) => 6,
            _ => kind,
        };
        let action = match notice {
            1 => Some(Undo::GameOff),
            3 => Some(Undo::PointerUnlock),
            4 => Some(Undo::KeyboardUnlock),
            5 => Some(Undo::MouseEnable),
            _ => None,
        };
        let request = Notification {
            level: if matches!(notice, 1 | 2) {
                NotificationLevel::Info
            } else {
                NotificationLevel::Notice
            },
            title: copy(self.notification_language(), notice).into(),
            description: disable_hint(self.notification_language(), notice, cfg!(target_os = "macos")),
            screen: None,
            persistent: matches!(notice, 4 | 5),
        };
        let result = self.raise_notification(request, action, notice);
        if !matches!(result, Ok(Some(_)))
            && let Some(id) = dismiss
        {
            self.dismiss_notification_immediately(id);
        }
        if let Ok(Some(receipt)) = result
            && let Ok(id) = receipt.id.parse()
        {
            let mut state = lock(&self.inner.notifications);
            match notice {
                1 => state.game_id = Some(id),
                3 => state.pointer_id = Some(id),
                4 => state.keyboard_id = Some(id),
                5 => state.mouse_id = Some(id),
                _ => {}
            }
        }
    }
}

fn level_number(level: NotificationLevel) -> u8 {
    match level {
        NotificationLevel::Notice => 0,
        NotificationLevel::Info => 1,
        NotificationLevel::Debug => 2,
    }
}
// Generate on the source owner: a forwarded toast must name that owner's keys.
// Older peers already carry description verbatim; their empty descriptions stay empty.
fn disable_hint(language: crate::config::UiLanguage, kind: u8, mac: bool) -> String {
    use crate::config::UiLanguage::*;
    if !matches!(kind, 4 | 5) {
        return String::new();
    }
    let chord = if mac {
        "Control+Option+Shift+U"
    } else {
        "Ctrl+Alt+Shift+U"
    };
    match language {
        SimplifiedChinese => format!("按住 {chord} 1 秒解除"),
        Japanese => format!("{chord} を1秒間押し続けると解除"),
        Korean => format!("{chord} 키를 1초 동안 눌러 해제"),
        _ => format!("Hold {chord} for 1 second to re-enable"),
    }
}

fn copy(language: crate::config::UiLanguage, kind: u8) -> &'static str {
    use crate::config::UiLanguage::*;
    let index = usize::from(kind.min(8));
    match language {
        SimplifiedChinese => [
            "撤销",
            "已进入游戏模式",
            "已退出游戏模式",
            "已锁定指针",
            "已禁用本机键盘",
            "已禁用本机鼠标",
            "指针锁定已解除",
            "共享麦克风已静音",
            "共享麦克风已解除静音",
        ][index],
        Japanese => [
            "元に戻す",
            "ゲームモードが有効になりました",
            "ゲームモードが無効になりました",
            "ポインターをロックしました",
            "このデバイスのキーボードを無効にしました",
            "このデバイスのマウスを無効にしました",
            "ポインターのロックを解除しました",
            "共有マイクをミュートしました",
            "共有マイクのミュートを解除しました",
        ][index],
        Korean => [
            "실행 취소",
            "게임 모드가 켜졌습니다",
            "게임 모드가 꺼졌습니다",
            "포인터가 잠겼습니다",
            "이 기기의 키보드가 비활성화되었습니다",
            "이 기기의 마우스가 비활성화되었습니다",
            "포인터 잠금이 해제되었습니다",
            "공유 마이크가 음소거되었습니다",
            "공유 마이크의 음소거가 해제되었습니다",
        ][index],
        _ => [
            "Undo",
            "Entered game mode",
            "Left game mode",
            "Pointer locked",
            "This device's keyboard disabled",
            "This device's mouse disabled",
            "Pointer lock released",
            "Shared microphone muted",
            "Shared microphone unmuted",
        ][index],
    }
}
#[cfg(target_os = "macos")]
fn system_language() -> String {
    objc2_foundation::NSLocale::preferredLanguages()
        .firstObject()
        .map_or_else(String::new, |s| s.to_string())
}
#[cfg(windows)]
fn system_language() -> String {
    use windows::{
        Win32::Globalization::{GetUserPreferredUILanguages, MUI_LANGUAGE_NAME},
        core::PWSTR,
    };
    let (mut count, mut length) = (0, 0);
    // SAFETY: sizing query writes only count/length and does not access a buffer.
    unsafe {
        if GetUserPreferredUILanguages(MUI_LANGUAGE_NAME, &mut count, None, &mut length).is_err() {
            return String::new();
        }
    }
    let mut buffer = vec![0u16; length as usize];
    // SAFETY: UTF-16 output buffer has the capacity reported by the sizing call.
    unsafe {
        if GetUserPreferredUILanguages(
            MUI_LANGUAGE_NAME,
            &mut count,
            Some(PWSTR(buffer.as_mut_ptr())),
            &mut length,
        )
        .is_err()
        {
            return String::new();
        }
    }
    String::from_utf16_lossy(buffer.split(|ch| *ch == 0).next().unwrap_or(&[]))
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    use oink_test_support::input::FakeInput;
    #[test]
    fn disable_toast_hint_names_source_keys_in_all_languages() {
        use crate::config::UiLanguage::*;
        for (language, expected) in [
            (SimplifiedChinese, "按住 Ctrl+Alt+Shift+U 1 秒解除"),
            (English, "Hold Ctrl+Alt+Shift+U for 1 second to re-enable"),
            (Japanese, "Ctrl+Alt+Shift+U を1秒間押し続けると解除"),
            (Korean, "Ctrl+Alt+Shift+U 키를 1초 동안 눌러 해제"),
        ] {
            for kind in [4, 5] {
                assert_eq!(disable_hint(language, kind, false), expected);
                assert_eq!(
                    disable_hint(language, kind, true),
                    expected.replace("Ctrl+Alt", "Control+Option")
                );
            }
            for kind in [0, 1, 2, 3] {
                assert!(disable_hint(language, kind, false).is_empty());
            }
        }
    }
    pub(super) fn options(home: &std::path::Path) -> NodeOptions {
        let input = Arc::new(FakeInput::new());
        input.set_cursor(Point::new(-0.2, 40.));
        let display = |key: &str, x: f64| DisplayInfo {
            key: key.into(),
            name: key.into(),
            bounds: oink_foundation::Rect::new(x, 0., 1000., 800.),
            pixel_width: 2000,
            pixel_height: 1600,
            reference_extent: None,
            rotation_deg: 0,
            is_primary: x == 0.,
            is_builtin: false,
            is_virtual: false,
        };
        NodeOptions {
            net: oink_test_support::network_config(home.to_path_buf(), "synthetic", 0),
            input,
            displays: vec![display("left", -1000.), display("right", 0.)],
            display_backend: None,
            clipboard: None,
            mode_switch_in_callback: true,
            capture_reports_key_repeat: false,
            cursor_trail: false,
        }
    }
    fn request() -> Notification {
        Notification {
            level: NotificationLevel::Notice,
            title: "Notification".into(),
            description: String::new(),
            screen: None,
            persistent: false,
        }
    }
    fn game_notice_router(node: &Node) -> (oink_routing::Router, DeviceId) {
        use oink_routing::{Os, Router, RouterConfig, ScreenId, SimpleLayout};
        let peer = DeviceId([7; 32]);
        let peer_session = [8; 16];
        let mut layout = SimpleLayout::new();
        let bounds = oink_foundation::Rect::new(0., 0., 1000., 800.);
        layout.add_screen(ScreenId::new(node.id(), "right"), bounds, bounds);
        layout.add_screen(
            ScreenId::new(peer, "screen"),
            bounds,
            oink_foundation::Rect::new(1000., 0., 1000., 800.),
        );
        let mut router = Router::new(RouterConfig::new(node.boot(), "device", Os::Other), Arc::new(layout));
        router.attach_input(
            Boot {
                device_id: peer,
                nonce: peer_session,
            },
            peer_session,
            true,
            true,
            Default::default(),
            0,
        );
        (router, peer)
    }

    fn game_notice_focus_away(router: &mut oink_routing::Router, peer: DeviceId) {
        use oink_routing::{
            Packet,
            control::{ControlMsg, FocusScope},
        };
        router.input_message(
            peer,
            Packet::Control(ControlMsg::FocusState {
                lock_state: None,
                epoch: router.focus().epoch + 1,
                holder: peer,
                scope: Some(FocusScope {
                    members: vec![(router.device(), router.routing_session()), (peer, [8; 16])],
                }),
                session: Some([8; 16]),
            }),
            1,
        );
        assert_eq!(router.focus().holder, peer);
    }

    fn game_notice_outputs(node: &Node, router: &mut oink_routing::Router) {
        for output in router.drain_outputs() {
            if let oink_routing::Output::Status(event) = output {
                node.notification_status(&event);
            }
        }
    }

    #[tokio::test]
    async fn microphone_receipts_replace_one_transient_info_family() {
        use crate::device_cards::{DeviceControlFailure, DeviceControlResult as Result};
        let home = tempfile::tempdir().unwrap();
        let node = Node::prepare(options(home.path())).unwrap();
        node.track_microphone_notice("mute", true);
        assert!(lock(&node.inner.notifications).pending.is_empty());
        node.complete_microphone_notice("mute", &Result::Applied);
        let first = lock(&node.inner.notifications).families[&Family::Microphone].surface;
        node.track_microphone_notice("unmute", false);
        node.complete_microphone_notice("unmute", &Result::Applied);
        let state = lock(&node.inner.notifications);
        assert_eq!(state.pending.len(), 1);
        assert_eq!(state.origins.len(), 1);
        assert_ne!(state.families[&Family::Microphone].surface, first);
        let pending = state.pending.values().next().unwrap();
        assert_eq!(pending.level, NotificationLevel::Info);
        assert!(pending.expires.is_some());
        assert!(pending.action.is_none());
        let id = state.families[&Family::Microphone].origin.id;
        drop(state);
        node.complete_microphone_notice("unmute", &Result::Applied);
        node.track_microphone_notice("failed", true);
        node.complete_microphone_notice("failed", &Result::refused(DeviceControlFailure::InvalidValue));
        assert_eq!(
            lock(&node.inner.notifications).families[&Family::Microphone].origin.id,
            id
        );
        assert!(lock(&node.inner.notifications).microphone_requests.is_empty());
    }

    #[test]
    fn microphone_notice_copy_covers_all_languages() {
        use crate::config::UiLanguage::*;
        for (language, muted, unmuted) in [
            (English, "Shared microphone muted", "Shared microphone unmuted"),
            (SimplifiedChinese, "共享麦克风已静音", "共享麦克风已解除静音"),
            (
                Japanese,
                "共有マイクをミュートしました",
                "共有マイクのミュートを解除しました",
            ),
            (
                Korean,
                "공유 마이크가 음소거되었습니다",
                "공유 마이크의 음소거가 해제되었습니다",
            ),
        ] {
            assert_eq!(copy(language, 7), muted);
            assert_eq!(copy(language, 8), unmuted);
        }
    }

    #[tokio::test]
    async fn replacement_retires_an_undo_even_after_its_click_route_was_taken() {
        let home = tempfile::tempdir().unwrap();
        let node = Node::prepare(options(home.path())).unwrap();
        node.notification_status(&StatusEvent::LocalPointerLock(true));
        let (id, surface) = {
            let mut state = lock(&node.inner.notifications);
            let id = state.pointer_id.unwrap();
            let surface = state.pending[&id].surface.unwrap();
            // A click has been dequeued, but its owner transaction is still pending.
            state.origins.remove(&surface);
            (id, surface)
        };
        node.notification_status(&StatusEvent::LocalPointerLock(false));
        let state = lock(&node.inner.notifications);
        assert!(!state.pending.contains_key(&id));
        assert!(!state.origins.contains_key(&surface));
        assert_eq!(state.pending.len(), 1);
        assert!(state.pending.values().next().unwrap().action.is_none());
    }

    #[tokio::test]
    async fn different_families_and_custom_notices_remain_independent() {
        let home = tempfile::tempdir().unwrap();
        let node = Node::prepare(options(home.path())).unwrap();
        for event in [
            StatusEvent::LocalGameMode(true),
            StatusEvent::LocalPointerLock(true),
            StatusEvent::KeyboardDisabled(true),
            StatusEvent::MouseDisabled(true),
        ] {
            node.notification_status(&event);
        }
        assert_eq!(lock(&node.inner.notifications).origins.len(), 4);
        node.notify(request()).unwrap();
        node.notify(request()).unwrap();
        node.notification_status(&StatusEvent::LocalGameMode(false));
        node.notification_status(&StatusEvent::LocalPointerLock(false));
        assert_eq!(lock(&node.inner.notifications).origins.len(), 6);
        node.notification_status(&StatusEvent::KeyboardDisabled(false));
        node.notification_status(&StatusEvent::MouseDisabled(false));
        assert_eq!(lock(&node.inner.notifications).origins.len(), 4);
    }

    #[tokio::test]
    async fn state_families_replace_in_both_directions_without_retaining_old_undo() {
        for game in [false, true] {
            let home = tempfile::tempdir().unwrap();
            let node = Node::prepare(options(home.path())).unwrap();
            let event = |on| {
                if game {
                    StatusEvent::LocalGameMode(on)
                } else {
                    StatusEvent::LocalPointerLock(on)
                }
            };
            let mut retired = Vec::new();
            for on in [true, false, true, false, true] {
                node.notification_status(&event(on));
                let state = lock(&node.inner.notifications);
                assert_eq!(state.pending.len(), 1, "only the latest family notice keeps an action");
                assert_eq!(
                    state.origins.len(),
                    1,
                    "only the latest family notice keeps a click route"
                );
                let (&surface, origin) = state.origins.iter().next().unwrap();
                assert!(retired.iter().all(|id| !state.origins.contains_key(id)));
                assert_eq!(state.pending[&origin.id].action.is_some(), on);
                retired.push(surface);
            }
        }
    }

    #[tokio::test]
    async fn game_banners_ignore_pointer_departure_and_return() {
        use oink_routing::{GameMode, Packet, control::ControlMsg};
        for auto in [false, true] {
            let home = tempfile::tempdir().unwrap();
            let node = Node::prepare(options(home.path())).unwrap();
            let (mut router, peer) = game_notice_router(&node);
            game_notice_outputs(&node, &mut router);
            if auto {
                router.set_pointer_captured(true);
            } else {
                router.set_game_mode(GameMode::On);
            }
            game_notice_outputs(&node, &mut router);
            let entry = lock(&node.inner.notifications).game_id.unwrap();
            let count = lock(&node.inner.notifications).next;
            game_notice_focus_away(&mut router, peer);
            game_notice_outputs(&node, &mut router);
            {
                let state = lock(&node.inner.notifications);
                assert_eq!(state.next, count, "pointer departure must not raise a game-exit banner");
                assert_eq!(state.game_id, Some(entry));
            }
            // The other holder's real game transitions are not ours either.
            for active in [true, false] {
                router.input_message(
                    peer,
                    Packet::Control(ControlMsg::GameMode {
                        epoch: router.focus().epoch,
                        generation: 1,
                        mode: GameMode::On,
                        detected: false,
                        active,
                        cursor_hiding_cause_available: None,
                    }),
                    2,
                );
                game_notice_outputs(&node, &mut router);
                assert_eq!(lock(&node.inner.notifications).next, count);
            }
            router.return_pointer_home();
            assert_eq!(router.focus().holder, node.id());
            assert!(router.game_mode().active);
            game_notice_outputs(&node, &mut router);
            assert_eq!(
                lock(&node.inner.notifications).next,
                count,
                "pointer return must not raise a game-entry banner"
            );
            if auto {
                router.set_pointer_captured(false);
            } else {
                router.toggle_game_mode();
            }
            game_notice_outputs(&node, &mut router);
            let state = lock(&node.inner.notifications);
            assert_eq!(state.next, count + 1, "actual game exit still raises one banner");
            assert!(state.game_id.is_none());
            assert_eq!(state.pending.values().next().unwrap().level, NotificationLevel::Info);
        }
    }

    #[tokio::test]
    async fn game_banners_follow_settings_edits_while_pointer_is_away() {
        use oink_routing::GameMode;
        let home = tempfile::tempdir().unwrap();
        let node = Node::prepare(options(home.path())).unwrap();
        let (mut router, peer) = game_notice_router(&node);
        game_notice_focus_away(&mut router, peer);
        game_notice_outputs(&node, &mut router);
        let count = lock(&node.inner.notifications).next;
        let mut settings = router.config().settings.clone();
        settings.game_mode_preference = GameMode::On;
        router.set_settings(settings.clone());
        settings.game_mode_preference = GameMode::Off;
        router.set_settings(settings.clone());
        // Both edits must remain ordered even though the focus snapshot never
        // changes and the final owner state equals the initial inactive state.
        game_notice_outputs(&node, &mut router);
        assert_eq!(lock(&node.inner.notifications).next, count + 2);
        assert!(!lock(&node.inner.notifications).game);
        router.set_settings(settings); // repeated Off is not another exit
        router.return_pointer_home();
        game_notice_outputs(&node, &mut router);
        assert_eq!(lock(&node.inner.notifications).next, count + 2);
    }

    #[tokio::test]
    async fn game_exit_banner_does_not_require_an_observed_entry() {
        use oink_routing::{GameMode, Os, Router, RouterConfig, SimpleLayout};
        let home = tempfile::tempdir().unwrap();
        let node = Node::prepare(options(home.path())).unwrap();
        let mut config = RouterConfig::new(node.boot(), "device", Os::Other);
        config.settings.game_mode_preference = GameMode::On;
        let mut router = Router::new(config, Arc::new(SimpleLayout::new()));
        router.toggle_game_mode();
        game_notice_outputs(&node, &mut router);
        let state = lock(&node.inner.notifications);
        assert_eq!(state.next, 1);
        assert!(!state.game);
        assert_eq!(state.pending.values().next().unwrap().action, None);
    }

    #[tokio::test]
    async fn ipc_filtering_selection_and_device_local_persistence() {
        let home = tempfile::tempdir().unwrap();
        let node = Node::prepare(options(home.path())).unwrap();
        let request: api::Request = serde_json::from_value(
            serde_json::json!({"method":"notify","params":{"notification":{"level":"notice","title":"Test"}}}),
        )
        .unwrap();
        let reply = api::dispatch(&node, request).unwrap();
        let api::Reply::Notification(Some(receipt)) = reply else {
            panic!("notification receipt")
        };
        assert_eq!(receipt.screen.as_ref().unwrap().display, "left");
        assert!(!receipt.remote);
        let before = lock(&node.inner.config).owner().clone();
        node.set_ui_preferences(serde_json::from_value(serde_json::json!({"notification_level":"disabled"})).unwrap())
            .unwrap();
        assert!(node.notify(self::request()).unwrap().is_none());
        assert_eq!(node.settings().notification_level, api::NotificationSetting::Disabled);
        assert_eq!(
            AgentConfig::load_from(home.path()).unwrap().settings.notification_level,
            api::NotificationSetting::Disabled
        );
        assert_eq!(before, lock(&node.inner.config).owner().clone());
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn game_undo_persists_off_and_publishes_live_owner_state() {
        use crate::snapshot_tests::until;
        let home = tempfile::tempdir().unwrap();
        let node = Node::start(options(home.path())).await.unwrap();
        let n = node.clone();
        tokio::task::spawn_blocking(move || {
            n.edit_device_option(n.id().into(), dto::LocalOption::GameModePreference, 1u32.write())
        })
        .await
        .unwrap()
        .unwrap();
        // Use the authoritative owner notice. Raising a competing notice here
        // races the async input status event, which correctly retires its Undo.
        until("game enabled notice", || {
            lock(&node.inner.notifications).game_id.is_some()
        })
        .await;
        let id = lock(&node.inner.notifications).game_id.unwrap();
        let n = node.clone();
        tokio::task::spawn_blocking(move || n.perform_notification_action(id, true, n.id()))
            .await
            .unwrap();
        assert_eq!(node.settings().game_mode, crate::config::GameModePreference::Off);
        assert_eq!(
            AgentConfig::load_from(home.path()).unwrap().settings.game_mode,
            crate::config::GameModePreference::Off
        );
        let facts = node.link_config().unwrap().local_facts();
        let pref = facts
            .preferences()
            .unwrap()
            .unwrap()
            .0
            .into_iter()
            .find(|p| p.option() == Ok(dto::LocalOption::GameModePreference))
            .unwrap();
        assert_eq!(pref.value().unwrap(), 2u32.write());
        node.shutdown().await;
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pointer_lock_toast_expires_without_unlocking_or_leaving_a_live_undo() {
        use crate::snapshot_tests::until;
        let home = tempfile::tempdir().unwrap();
        let node = Node::start(options(home.path())).await.unwrap();
        node.set_focus_locked(true).unwrap();
        until("timed pointer lock notice", || {
            lock(&node.inner.notifications).pointer_id.is_some()
                && node.input_status().unwrap().locked_device == Some(node.id())
        })
        .await;
        let id = {
            let mut state = lock(&node.inner.notifications);
            let id = state.pointer_id.unwrap();
            let expiry = state.pending[&id]
                .expires
                .expect("lock notice must use normal toast lifetime");
            state.prune(expiry - Duration::from_nanos(1));
            assert!(state.pending.contains_key(&id));
            state.prune(expiry);
            assert!(!state.pending.contains_key(&id));
            assert!(state.origins.is_empty());
            id
        };
        node.perform_notification_action(id, true, node.id());
        assert_eq!(node.input_status().unwrap().locked_device, Some(node.id()));
        node.shutdown().await;
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lock_body_dismiss_and_undo_have_distinct_effects() {
        use crate::snapshot_tests::until;
        let home = tempfile::tempdir().unwrap();
        let node = Node::start(options(home.path())).await.unwrap();
        node.set_focus_locked(true).unwrap();
        // Ordered notification transitions can arrive before the input owner
        // publishes its latest status. Observe both before testing click effects.
        until("pointer lock notice", || {
            lock(&node.inner.notifications).pointer_id.is_some()
                && node.input_status().unwrap().locked_device == Some(node.id())
        })
        .await;
        let id = lock(&node.inner.notifications).pointer_id.unwrap();
        assert!(
            lock(&node.inner.notifications).pending[&id].expires.is_some(),
            "pointer lock toast must expire"
        );
        let surface = lock(&node.inner.notifications).pending[&id].surface.unwrap();
        node.notification_click(Click {
            id: surface,
            undo: false,
        });
        assert_eq!(node.input_status().unwrap().locked_device, Some(node.id()));
        node.set_focus_locked(false).unwrap();
        until("pointer released", || {
            node.input_status().unwrap().locked_device.is_none()
        })
        .await;
        node.set_focus_locked(true).unwrap();
        until("new pointer lock notice", || {
            lock(&node.inner.notifications)
                .pointer_id
                .is_some_and(|next| next != id)
                && node.input_status().unwrap().locked_device == Some(node.id())
        })
        .await;
        let id = lock(&node.inner.notifications).pointer_id.unwrap();
        let surface = lock(&node.inner.notifications).pending[&id].surface.unwrap();
        node.notification_click(Click {
            id: surface,
            undo: true,
        });
        until("pointer undo", || node.input_status().unwrap().locked_device.is_none()).await;
        let n = node.clone();
        tokio::task::spawn_blocking(move || n.set_keyboard_disabled(n.id().into(), true))
            .await
            .unwrap()
            .unwrap();
        until("keyboard lock notice", || {
            lock(&node.inner.notifications).keyboard_id.is_some()
        })
        .await;
        let id = lock(&node.inner.notifications).keyboard_id.unwrap();
        let surface = lock(&node.inner.notifications).pending[&id].surface.unwrap();
        let n = node.clone();
        tokio::task::spawn_blocking(move || n.set_keyboard_disabled(n.id().into(), false))
            .await
            .unwrap()
            .unwrap();
        until("dismiss released keyboard banner", || {
            let state = lock(&node.inner.notifications);
            !state.pending.contains_key(&id) && !state.origins.contains_key(&surface)
        })
        .await;
        node.notification_click(Click {
            id: surface,
            undo: true,
        });
        assert!(node.settings().routing.keyboard_mode != oink_foundation::input::InputMode::Disabled);
        node.shutdown().await;
    }
    #[tokio::test]
    async fn pointer_notice_survives_focus_departure_and_return() {
        let home = tempfile::tempdir().unwrap();
        let node = Node::prepare(options(home.path())).unwrap();
        let (mut router, peer) = game_notice_router(&node);
        game_notice_focus_away(&mut router, peer);
        router.return_pointer_home();
        router.set_locked(true);
        game_notice_outputs(&node, &mut router);
        let id = lock(&node.inner.notifications).pointer_id.unwrap();
        let expiry = lock(&node.inner.notifications).pending[&id].expires;
        router.move_pointer_to(peer).unwrap();
        game_notice_outputs(&node, &mut router);
        assert!(lock(&node.inner.notifications).pending.contains_key(&id));
        router.return_pointer_home();
        game_notice_outputs(&node, &mut router);
        let state = lock(&node.inner.notifications);
        assert_eq!(state.pointer_id, Some(id));
        assert_eq!(state.pending[&id].expires, expiry);
    }

    #[tokio::test]
    async fn pointer_release_is_timed_deduplicated_and_retires_old_undo() {
        for expired in [false, true] {
            let home = tempfile::tempdir().unwrap();
            let node = Node::prepare(options(home.path())).unwrap();
            node.notification_status(&StatusEvent::LocalPointerLock(true));
            let id = lock(&node.inner.notifications).pointer_id.unwrap();
            let old_surface = lock(&node.inner.notifications).families[&Family::Pointer].surface;
            if expired {
                let mut state = lock(&node.inner.notifications);
                let expiry = state.pending[&id].expires.unwrap();
                state.prune(expiry);
            }
            node.notification_status(&StatusEvent::LocalPointerLock(false));
            node.notification_status(&StatusEvent::LocalPointerLock(false));
            node.perform_notification_action(id, true, node.id());
            let state = lock(&node.inner.notifications);
            assert_eq!(state.pending.len(), 1);
            assert_eq!(state.origins.len(), 1);
            assert!(!state.pending.contains_key(&id));
            assert!(!state.origins.contains_key(&old_surface));
            assert!(state.pointer_id.is_none());
            assert_ne!(state.families[&Family::Pointer].surface, old_surface);
            let release = state.pending.values().next().unwrap();
            assert_eq!(release.level, NotificationLevel::Notice);
            assert_eq!(release.destination, node.id());
            assert!(release.action.is_none());
            assert!(release.expires.is_some());
        }
    }

    #[test]
    fn pointer_release_copy_is_localized() {
        use crate::config::UiLanguage::*;
        for (language, expected) in [
            (SimplifiedChinese, "指针锁定已解除"),
            (English, "Pointer lock released"),
            (Japanese, "ポインターのロックを解除しました"),
            (Korean, "포인터 잠금이 해제되었습니다"),
        ] {
            assert_eq!(copy(language, 6), expected);
        }
    }

    #[tokio::test]
    async fn ordered_cause_release_dismisses_only_its_banner_and_stale_undo() {
        let home = tempfile::tempdir().unwrap();
        let node = Node::prepare(options(home.path())).unwrap();
        let keyboard = StatusEvent::KeyboardDisabled(true);
        let pointer = StatusEvent::LocalPointerLock(true);
        node.notification_status(&keyboard);
        node.notification_status(&pointer);
        let keyboard_id = lock(&node.inner.notifications).keyboard_id.unwrap();
        let keyboard_surface = lock(&node.inner.notifications).pending[&keyboard_id].surface.unwrap();
        let pointer_id = lock(&node.inner.notifications).pointer_id.unwrap();
        node.notification_status(&StatusEvent::KeyboardDisabled(false));
        {
            let state = lock(&node.inner.notifications);
            assert!(
                !state.pending.contains_key(&keyboard_id),
                "released cause removes pending banner"
            );
            assert!(
                !state.origins.contains_key(&keyboard_surface),
                "released cause removes visible origin"
            );
            assert!(state.pending.contains_key(&pointer_id), "unrelated cause stays visible");
        }
        node.notification_status(&keyboard);
        let next = lock(&node.inner.notifications).keyboard_id.unwrap();
        assert_ne!(next, keyboard_id);
        node.notification_click(Click {
            id: keyboard_surface,
            undo: true,
        });
        assert!(lock(&node.inner.notifications).pending.contains_key(&next));
        node.notification_status(&StatusEvent::LocalPointerLock(false));
        assert!(!lock(&node.inner.notifications).pending.contains_key(&pointer_id));
        node.notification_status(&StatusEvent::KeyboardDisabled(false));
        let state = lock(&node.inner.notifications);
        assert_eq!(state.pending.len(), 1, "only the timed pointer-release notice remains");
        assert_eq!(state.origins.len(), 1);
        let release = state.pending.values().next().unwrap();
        assert!(release.action.is_none());
        assert!(release.expires.is_some());
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mouse_undo_restores_and_persists_shared_source_mode() {
        use crate::snapshot_tests::until;
        let home = tempfile::tempdir().unwrap();
        let node = Node::start(options(home.path())).await.unwrap();
        let n = node.clone();
        tokio::task::spawn_blocking(move || {
            n.edit_device_option(n.id().into(), dto::LocalOption::MouseMode, 2u32.write())
        })
        .await
        .unwrap()
        .unwrap();
        until("mouse disabled notice", || {
            lock(&node.inner.notifications).mouse_id.is_some()
        })
        .await;
        let id = lock(&node.inner.notifications).mouse_id.unwrap();
        let n = node.clone();
        tokio::task::spawn_blocking(move || n.perform_notification_action(id, true, n.id()))
            .await
            .unwrap();
        until("mouse enabled notice removed", || {
            lock(&node.inner.notifications).mouse_id.is_none()
        })
        .await;
        assert_eq!(
            node.settings().routing.mouse_mode,
            oink_foundation::input::InputMode::Shared
        );
        assert_eq!(
            AgentConfig::load_from(home.path()).unwrap().settings.routing.mouse_mode,
            oink_foundation::input::InputMode::Shared
        );
        node.shutdown().await;
    }
    #[tokio::test]
    async fn mouse_cause_release_dismisses_banner_and_does_not_resurrect_stale_action() {
        let home = tempfile::tempdir().unwrap();
        let node = Node::prepare(options(home.path())).unwrap();
        node.notification_status(&StatusEvent::MouseDisabled(true));
        let (id, surface) = {
            let state = lock(&node.inner.notifications);
            assert_eq!(state.pending.len(), 1);
            let (&id, pending) = state.pending.iter().next().unwrap();
            (id, pending.surface.unwrap())
        };
        node.notification_status(&StatusEvent::MouseDisabled(true));
        assert_eq!(lock(&node.inner.notifications).pending.len(), 1);
        node.notification_status(&StatusEvent::KeyboardDisabled(true));
        node.notification_status(&StatusEvent::MouseDisabled(false));
        {
            let state = lock(&node.inner.notifications);
            assert!(!state.pending.contains_key(&id));
            assert!(!state.origins.contains_key(&surface));
            assert_eq!(state.pending.len(), 1);
        }
        node.notification_status(&StatusEvent::MouseDisabled(true));
        node.notification_click(Click {
            id: surface,
            undo: true,
        });
        assert_eq!(lock(&node.inner.notifications).pending.len(), 2);
        node.notification_status(&StatusEvent::MouseDisabled(false));
        node.notification_status(&StatusEvent::KeyboardDisabled(false));
        assert!(lock(&node.inner.notifications).pending.is_empty());
    }
    #[tokio::test]
    async fn unavailable_remote_falls_back_to_last_local_screen() {
        let home = tempfile::tempdir().unwrap();
        let node = Node::prepare(options(home.path())).unwrap();
        let mut req = request();
        req.screen = Some(ScreenKey {
            device: DeviceId([9; 32]).into(),
            display: "remote".into(),
        });
        let receipt = node.notify(req).unwrap().unwrap();
        assert!(!receipt.remote);
        assert_eq!(receipt.screen.as_ref().unwrap().display, "left");
    }
}

#[cfg(all(test, debug_assertions))]
mod remote_tests {
    use super::*;
    use crate::snapshot_tests::{Displays, pair, start_with_clip, until};
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn negotiated_remote_delivery_and_released_peer_fallback() {
        for supported in [true, false] {
            let home_a = tempfile::tempdir().unwrap();
            let home_b = tempfile::tempdir().unwrap();
            let a_config = oink_test_support::network_config(home_a.path().to_path_buf(), "synthetic-a", 0);
            let b_config = oink_test_support::network_config(home_b.path().to_path_buf(), "synthetic-b", 0);
            let a = start_with_clip(&a_config, &Displays::new("a"), None).await;
            let mut options = NodeOptions {
                net: b_config,
                input: Arc::new(oink_test_support::input::FakeInput::new()),
                displays: Vec::new(),
                display_backend: Some(Displays::new("b")),
                clipboard: None,
                mode_switch_in_callback: true,
                capture_reports_key_repeat: false,
                cursor_trail: false,
            };
            options.net.bind_ip = "127.0.0.1".parse().unwrap();
            let b = Node::prepare(options).unwrap();
            let mut profile = Node::offer_profile();
            if !supported {
                profile.capabilities.retain(|cap| cap != CAPABILITY);
            }
            b.start_sharing_with_profile(profile).await.unwrap();
            pair(&a, &b).await;
            until("notification agreement", || {
                a.link_config().unwrap().peer_agreement(b.id()).is_some()
            })
            .await;
            assert_eq!(
                a.link_config()
                    .unwrap()
                    .peer_agreement(b.id())
                    .unwrap()
                    .contains(CAPABILITY),
                supported
            );
            let request = Notification {
                level: NotificationLevel::Notice,
                title: "Remote notice".into(),
                description: String::new(),
                screen: Some(ScreenKey {
                    device: b.id().into(),
                    display: "b".into(),
                }),
                persistent: true,
            };
            let receipt = a.notify(request).unwrap().unwrap();
            assert_eq!(receipt.remote, supported);
            if supported {
                until("remote toast received", || {
                    lock(&b.inner.notifications).origins.values().any(|o| o.peer == a.id())
                })
                .await;
                let id = receipt.id.parse().unwrap();
                a.dismiss_notification(id);
                until("remote toast dismissed", || {
                    lock(&b.inner.notifications).origins.is_empty()
                })
                .await;
            } else {
                assert_eq!(receipt.screen.as_ref().unwrap().device.0, a.id());
                assert!(lock(&b.inner.notifications).origins.is_empty());
                let message = Message::NotifyDismiss(dto::NotifyDismiss::from_raw([(1, 1u64.write())].into()).unwrap());
                assert!(a.link_config().unwrap().send_device_control(b.id(), message).is_err());
                assert!(lock(&b.inner.notifications).origins.is_empty());
            }
            if supported {
                // Retain coverage of the existing remote-action wire path for
                // custom requests and payloads from earlier notification builds.
                let n = a.clone();
                tokio::task::spawn_blocking(move || n.set_keyboard_disabled(n.id().into(), true))
                    .await
                    .unwrap()
                    .unwrap();
                let request = Notification {
                    level: NotificationLevel::Notice,
                    title: "Keyboard disabled".into(),
                    description: String::new(),
                    screen: Some(ScreenKey {
                        device: b.id().into(),
                        display: "b".into(),
                    }),
                    persistent: true,
                };
                let receipt = a
                    .raise_notification(request, Some(Undo::KeyboardUnlock), 0)
                    .unwrap()
                    .unwrap();
                let source_id = receipt.id.parse::<u64>().unwrap();
                until("remote undo toast", || {
                    lock(&b.inner.notifications)
                        .origins
                        .values()
                        .any(|o| o.peer == a.id() && o.id == source_id)
                })
                .await;
                let local = *lock(&b.inner.notifications)
                    .origins
                    .iter()
                    .find(|(_, o)| o.peer == a.id() && o.id == source_id)
                    .unwrap()
                    .0;
                b.notification_click(Click { id: local, undo: true });
                until("remote keyboard undo", || {
                    a.settings().routing.keyboard_mode != oink_foundation::input::InputMode::Disabled
                })
                .await;
                assert!(
                    !(AgentConfig::load_from(a.data_dir())
                        .unwrap()
                        .settings
                        .routing
                        .keyboard_mode
                        == oink_foundation::input::InputMode::Disabled)
                );
            }
            a.shutdown().await;
            b.shutdown().await;
        }
    }
}

#[cfg(all(test, debug_assertions))]
mod owner_tests {
    use super::*;
    use crate::snapshot_tests::{common, pair, until};
    use oink_foundation::input::{CapturedEvent, InputEvent, KeyCode, SourceKind};
    use oink_routing::LayoutQuery;
    use oink_test_support::input::FakeInput;

    async fn owners(notify_supported: bool) -> (tempfile::TempDir, tempfile::TempDir, Node, Node) {
        owners_with_microphone(notify_supported, true).await
    }
    async fn owners_with_microphone(
        notify_supported: bool,
        microphone_supported: bool,
    ) -> (tempfile::TempDir, tempfile::TempDir, Node, Node) {
        let home_a = tempfile::tempdir().unwrap();
        let home_b = tempfile::tempdir().unwrap();
        let input_a = Arc::new(FakeInput::new());
        input_a.set_cursor(Point::new(200., 200.));
        let mut options_a = super::tests::options(home_a.path());
        options_a.input = input_a.clone();
        let a = Node::start(options_a).await.unwrap();
        let b = Node::prepare(super::tests::options(home_b.path())).unwrap();
        let mut profile = Node::offer_profile();
        if !notify_supported {
            profile.capabilities.retain(|cap| cap != CAPABILITY);
        }
        if !microphone_supported {
            profile.capabilities.retain(|cap| cap != MICROPHONE_CAPABILITY);
        }
        b.start_sharing_with_profile(profile).await.unwrap();
        pair(&a, &b).await;
        common::active(&[&a, &b]).await;
        until("both owner screens admitted", || {
            [&a, &b].into_iter().all(|node| {
                let config = node.link_config().unwrap();
                let layout = config.router_layout().load();
                [a.id(), b.id()]
                    .into_iter()
                    .all(|device| layout.topology.contains_device(device))
            })
        })
        .await;
        for pressed in [true, false] {
            input_a.feed(&CapturedEvent {
                physical: true,
                event: InputEvent::Key {
                    code: KeyCode::keyboard(4),
                    pressed,
                },
                source: SourceKind::Keyboard,
                time_us: 0,
                cursor: None,
            });
        }
        until("requester owns pointer, affected owner does not", || {
            a.input_status().unwrap().pointer_focus
                && !b.input_status().unwrap().pointer_focus
                && b.input_status().unwrap().group_focus.holder == Some(a.id())
        })
        .await;
        assert_eq!(
            a.link_config()
                .unwrap()
                .peer_agreement(b.id())
                .unwrap()
                .contains(CAPABILITY),
            notify_supported
        );
        (home_a, home_b, a, b)
    }

    #[tokio::test]
    async fn microphone_notice_follows_focus_and_replaces_across_origins() {
        use crate::device_cards::DeviceControlResult;
        let (_ha, _hb, focus, pressing) = owners(true).await;
        pressing.track_microphone_notice("mute", true);
        pressing.complete_microphone_notice("mute", &DeviceControlResult::Applied);
        until("microphone notice reaches focus", || {
            lock(&focus.inner.notifications)
                .families
                .contains_key(&Family::Microphone)
        })
        .await;
        assert!(lock(&pressing.inner.notifications).origins.is_empty());
        let surface = lock(&focus.inner.notifications).families[&Family::Microphone].surface;
        focus.track_microphone_notice("unmute", false);
        focus.complete_microphone_notice("unmute", &DeviceControlResult::Applied);
        {
            let state = lock(&focus.inner.notifications);
            assert_eq!(state.origins.len(), 1);
            assert_ne!(state.families[&Family::Microphone].surface, surface);
            assert_eq!(state.families[&Family::Microphone].origin.peer, focus.id());
        }
        until("replacement retires source route", || {
            lock(&pressing.inner.notifications).pending.is_empty()
        })
        .await;
        focus.shutdown().await;
        pressing.shutdown().await;
    }

    #[tokio::test]
    async fn microphone_notice_is_silent_with_older_notify_peer() {
        use crate::device_cards::DeviceControlResult;
        let (_ha, _hb, focus, pressing) = owners_with_microphone(true, false).await;
        assert!(
            !pressing
                .link_config()
                .unwrap()
                .peer_agreement(focus.id())
                .unwrap()
                .contains(MICROPHONE_CAPABILITY)
        );
        pressing.track_microphone_notice("mute", true);
        pressing.complete_microphone_notice("mute", &DeviceControlResult::Applied);
        assert!(lock(&pressing.inner.notifications).pending.is_empty());
        assert!(lock(&pressing.inner.notifications).origins.is_empty());
        assert!(lock(&focus.inner.notifications).origins.is_empty());
        focus.shutdown().await;
        pressing.shutdown().await;
    }

    fn assert_only_owner(requester: &Node, owner: &Node, id: u64, undo: Undo) -> u64 {
        let state = lock(&owner.inner.notifications);
        let pending = &state.pending[&id];
        assert_eq!(
            pending.destination,
            owner.id(),
            "builtin must stay on the affected owner, even when its pointer is remote"
        );
        assert_eq!(pending.action, Some(undo));
        assert_eq!(state.pending.len(), 1, "one notice per transition");
        assert_eq!(state.origins.len(), 1, "one owner-local banner");
        let surface = pending.surface.expect("owner-local native surface");
        assert_eq!(state.origins[&surface].peer, owner.id());
        drop(state);
        let requester = lock(&requester.inner.notifications);
        assert!(requester.pending.is_empty(), "requester must not raise a duplicate");
        assert!(
            requester.origins.is_empty(),
            "owner must not forward its builtin to requester"
        );
        surface
    }

    async fn call(node: &Node, request: api::Request) {
        let node = node.clone();
        tokio::task::spawn_blocking(move || api::dispatch(&node, request))
            .await
            .unwrap()
            .unwrap();
    }

    fn send_legacy_keyboard_notice(source: &Node, host: &Node) -> u64 {
        // Exercise the existing incoming typed-notification protocol. Current
        // builtins render on their owner; older senders can target another host.
        let group = source.link_config().unwrap();
        let id = {
            let mut state = lock(&source.inner.notifications);
            state.next += 1;
            let id = state.next;
            state.pending.insert(
                id,
                Pending {
                    level: NotificationLevel::Notice,
                    action: Some(Undo::KeyboardUnlock),
                    destination: host.id(),
                    boot: group.peer_boot(host.id()),
                    surface: None,
                    expires: None,
                },
            );
            id
        };
        let message = dto::NotifyShow::from_raw(
            [
                (1, id.write()),
                (2, V::Text("right".into())),
                (3, 0u8.write()),
                (4, V::Text("Keyboard disabled".into())),
                (5, V::Text(String::new())),
                (6, true.write()),
                (7, true.write()),
                (8, 4u8.write()),
            ]
            .into(),
        )
        .unwrap();
        group
            .send_device_control(host.id(), Message::NotifyShow(message))
            .unwrap();
        id
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn family_replacement_ignores_origin_and_routes_only_the_visible_undo() {
        let (_home_a, _home_b, source, host) = owners(true).await;
        for node in [&source, &host] {
            call(
                node,
                api::Request::SetKeyboardDisabled {
                    device: node.id().into(),
                    disabled: true,
                },
            )
            .await;
        }
        until("local keyboard notices", || {
            [&source, &host]
                .into_iter()
                .all(|node| lock(&node.inner.notifications).keyboard_id.is_some())
        })
        .await;
        let old_local = lock(&host.inner.notifications).families[&Family::Keyboard].surface;
        let remote_id = send_legacy_keyboard_notice(&source, &host);
        until("remote family replacement", || {
            let state = lock(&host.inner.notifications);
            state
                .origins
                .values()
                .any(|origin| origin.peer == source.id() && origin.id == remote_id)
        })
        .await;
        let remote_surface = {
            let state = lock(&host.inner.notifications);
            assert_eq!(state.origins.len(), 1);
            assert!(!state.origins.contains_key(&old_local));
            state.families[&Family::Keyboard].surface
        };
        host.notification_click(Click {
            id: old_local,
            undo: true,
        });
        assert_eq!(
            host.settings().routing.keyboard_mode,
            oink_foundation::input::InputMode::Disabled
        );
        call(
            &host,
            api::Request::SetKeyboardDisabled {
                device: host.id().into(),
                disabled: false,
            },
        )
        .await;
        until("local restore observed", || {
            lock(&host.inner.notifications).keyboard_id.is_none()
        })
        .await;
        assert!(
            lock(&host.inner.notifications).origins.contains_key(&remote_surface),
            "an old owner's restore must not remove a newer remote notice"
        );
        call(
            &host,
            api::Request::SetKeyboardDisabled {
                device: host.id().into(),
                disabled: true,
            },
        )
        .await;
        until("local replaces remote and retires its pending action", || {
            let state = lock(&host.inner.notifications);
            state.keyboard_id.is_some()
                && !state.origins.contains_key(&remote_surface)
                && !lock(&source.inner.notifications).pending.contains_key(&remote_id)
        })
        .await;
        host.notification_click(Click {
            id: remote_surface,
            undo: true,
        });
        assert!(
            (source.settings().routing.keyboard_mode == oink_foundation::input::InputMode::Disabled),
            "replacement acknowledges dismissal, never Undo"
        );
        let local = lock(&host.inner.notifications).families[&Family::Keyboard].surface;
        let target = host.clone();
        tokio::task::spawn_blocking(move || target.notification_click(Click { id: local, undo: true }))
            .await
            .unwrap();
        until("visible local undo", || {
            host.settings().routing.keyboard_mode != oink_foundation::input::InputMode::Disabled
                && lock(&host.inner.notifications).keyboard_id.is_none()
        })
        .await;
        assert_eq!(
            source.settings().routing.keyboard_mode,
            oink_foundation::input::InputMode::Disabled
        );
        let remote_id = send_legacy_keyboard_notice(&source, &host);
        until("new visible remote undo", || {
            lock(&host.inner.notifications)
                .origins
                .values()
                .any(|origin| origin.id == remote_id && origin.peer == source.id())
        })
        .await;
        let surface = lock(&host.inner.notifications).families[&Family::Keyboard].surface;
        host.notification_click(Click {
            id: surface,
            undo: true,
        });
        until("visible remote undo reaches its source", || {
            source.settings().routing.keyboard_mode != oink_foundation::input::InputMode::Disabled
        })
        .await;
        source.shutdown().await;
        host.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn remote_keyboard_notice_stays_on_owner_and_owner_undo_releases() {
        // Generic CLI calls and the UI's owner-card token must converge on the
        // affected owner, including without the optional notification capability.
        for supported in [true, false] {
            let (_home_a, home_b, requester, owner) = owners(supported).await;
            assert_eq!(owner.owner_notification_screen().unwrap().display, "right");
            for operation in ["remote_cli", "remote_ui", "local"] {
                until("requester observes unlocked keyboard owner", || {
                    requester.device_cards().unwrap().cards.iter().any(|card| {
                        card.device.0 == owner.id()
                            && card
                                .status
                                .as_ref()
                                .is_some_and(|status| status.keyboard.disabled == Some(false))
                    })
                })
                .await;
                let request = if operation == "remote_ui" {
                    let cards = requester.device_cards().unwrap();
                    let owner_card = cards.cards.iter().find(|card| card.device.0 == owner.id()).unwrap();
                    let crate::device_cards::CardAction::Invoke { token, .. } =
                        &owner_card.status.as_ref().unwrap().actions.keyboard_disabled
                    else {
                        panic!("keyboard owner action unavailable")
                    };
                    api::Request::InvokeDeviceCardAction {
                        request_id: crate::device_controls::runtime::fresh_request_id(),
                        token: token.clone(),
                    }
                } else {
                    api::Request::SetKeyboardDisabled {
                        device: owner.id().into(),
                        disabled: true,
                    }
                };
                call(if operation == "local" { &owner } else { &requester }, request).await;
                until("keyboard owner notice", || {
                    lock(&owner.inner.notifications).keyboard_id.is_some()
                })
                .await;
                let id = lock(&owner.inner.notifications).keyboard_id.unwrap();
                let surface = assert_only_owner(&requester, &owner, id, Undo::KeyboardUnlock);
                assert_eq!(
                    owner.settings().routing.keyboard_mode,
                    oink_foundation::input::InputMode::Disabled
                );
                let target = owner.clone();
                tokio::task::spawn_blocking(move || {
                    target.notification_click(Click {
                        id: surface,
                        undo: true,
                    })
                })
                .await
                .unwrap();
                until(
                    &format!("owner keyboard undo persisted and retired: {supported}/{operation}"),
                    || {
                        owner.settings().routing.keyboard_mode != oink_foundation::input::InputMode::Disabled
                            && lock(&owner.inner.notifications).keyboard_id.is_none()
                    },
                )
                .await;
                assert!(
                    AgentConfig::load_from(home_b.path())
                        .unwrap()
                        .settings
                        .routing
                        .keyboard_mode
                        != oink_foundation::input::InputMode::Disabled
                );
                assert!(lock(&requester.inner.notifications).origins.is_empty());
            }
            requester.shutdown().await;
            owner.shutdown().await;
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn remote_and_local_pointer_locks_notify_only_the_affected_owner() {
        for supported in [true, false] {
            let (_home_a, _home_b, requester, owner) = owners(supported).await;
            for remote in [true, false] {
                if remote {
                    call(
                        &requester,
                        api::Request::LockFocusTo {
                            device: owner.id().into(),
                        },
                    )
                    .await;
                } else {
                    call(&owner, api::Request::SetFocusLocked { locked: true }).await;
                }
                until("affected pointer owner locked on both nodes", || {
                    [&requester, &owner]
                        .into_iter()
                        .all(|node| node.input_status().unwrap().locked_device == Some(owner.id()))
                        && lock(&owner.inner.notifications).pointer_id.is_some()
                })
                .await;
                let id = lock(&owner.inner.notifications).pointer_id.unwrap();
                let surface = assert_only_owner(&requester, &owner, id, Undo::PointerUnlock);
                owner.notification_click(Click {
                    id: surface,
                    undo: true,
                });
                until("owner pointer undo releases both observations", || {
                    [&requester, &owner]
                        .into_iter()
                        .all(|node| node.input_status().unwrap().locked_device.is_none())
                        && lock(&owner.inner.notifications).pointer_id.is_none()
                        && lock(&owner.inner.notifications)
                            .pending
                            .values()
                            .any(|p| p.action.is_none())
                })
                .await;
                assert!(lock(&requester.inner.notifications).origins.is_empty());
                let release = *lock(&owner.inner.notifications).pending.keys().next().unwrap();
                owner.dismiss_notification(release);
            }
            requester.shutdown().await;
            owner.shutdown().await;
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn ordered_owner_notices_ignore_a_stale_remote_pointer_snapshot() {
        let (_home_a, _home_b, requester, owner) = owners(true).await;
        // Ordered transition work can precede the observer's latest pointer
        // metadata. Neither a lock nor game transition may follow that snapshot.
        owner.notification_status(&StatusEvent::LocalPointerLock(true));
        let id = lock(&owner.inner.notifications).pointer_id.unwrap();
        assert_only_owner(&requester, &owner, id, Undo::PointerUnlock);
        assert_eq!(
            lock(&owner.inner.notifications).pending[&id].level,
            NotificationLevel::Notice
        );
        owner.dismiss_notification(id);
        owner.notification_status(&StatusEvent::LocalPointerLock(false));
        let release = *lock(&owner.inner.notifications).pending.keys().next().unwrap();
        assert!(lock(&owner.inner.notifications).pending[&release].action.is_none());
        owner.dismiss_notification(release);
        owner.notification_status(&StatusEvent::LocalGameMode(true));
        let id = lock(&owner.inner.notifications).game_id.unwrap();
        assert_only_owner(&requester, &owner, id, Undo::GameOff);
        assert_eq!(
            lock(&owner.inner.notifications).pending[&id].level,
            NotificationLevel::Info
        );
        owner.notification_status(&StatusEvent::LocalGameMode(false));
        {
            let state = lock(&owner.inner.notifications);
            assert_eq!(state.pending.len(), 1, "game exit replaces the entry notice");
            let leave = state.pending.values().next().unwrap();
            assert_eq!(leave.level, NotificationLevel::Info);
            assert_eq!(leave.destination, owner.id());
            assert!(leave.surface.is_some());
            assert!(leave.action.is_none());
            assert!(leave.expires.is_some());
        }
        assert!(lock(&requester.inner.notifications).origins.is_empty());
        requester.shutdown().await;
        owner.shutdown().await;
    }

    #[tokio::test]
    async fn owner_screen_keeps_negative_subpoints_and_falls_back_to_primary() {
        let home = tempfile::tempdir().unwrap();
        let input = Arc::new(FakeInput::new());
        let mut options = super::tests::options(home.path());
        options.input = input.clone();
        let mut above = options.displays[1].clone();
        above.key = "above".into();
        above.is_primary = false;
        above.bounds.y = -800.;
        options.displays.push(above);
        let node = Node::start(options).await.unwrap();
        until("local pointer owner", || node.input_status().unwrap().pointer_focus).await;
        for (point, key) in [
            (Point::new(-0.2, 40.), "left"),
            (Point::new(20., -0.2), "above"),
            (Point::new(4000., 4000.), "right"),
        ] {
            input.set_cursor(point);
            let screen = node.owner_notification_screen().unwrap();
            assert_eq!(screen.device.0, node.id());
            assert_eq!(screen.display, key);
        }
        node.shutdown().await;
    }
}
