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
#[derive(Debug, Default)]
pub(super) struct Notifications {
    stopped: bool,
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
        if kind != 0 {
            // Keyboard events are local and pointer/game events identify their
            // owner below. Never route these notices through the generic API's
            // group-pointer default, even when another member requested the edit.
            request.screen = Some(self.owner_notification_screen()?);
        } else if request.screen.is_none() {
            request.screen = self.default_notification_screen();
        }
        let group = self.inner.group.get();
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
            let raw = [
                (1, id.write()),
                (2, V::Text(screen.display.clone())),
                (3, level_number(request.level).write()),
                (4, V::Text(request.title.clone())),
                (5, V::Text(request.description.clone())),
                (6, request.persistent.write()),
                (7, action.is_some().write()),
                (8, kind.write()),
            ]
            .into();
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
        let overlay = self.notification_overlay(&mut state);
        let local = overlay.push(Toast {
            title: request.title,
            description: request.description,
            undo: action.map(|_| copy(self.notification_language(), 0).into()),
            persistent: request.persistent,
            scale: notification::scale_for(&display),
            display,
        });
        state.origins.insert(
            local,
            Origin {
                level: request.level,
                expires: (!request.persistent).then(|| Instant::now() + notification::LIFETIME),
                peer: self.id(),
                boot: None,
                id,
            },
        );
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
        self.apply_notification_preferences();
        Ok(Some(NotificationReceipt {
            id: id.to_string(),
            screen: Some(screen),
            remote: false,
        }))
    }
    fn notification_language(&self) -> crate::config::UiLanguage {
        self.settings().ui_language.resolve(&system_language())
    }
    fn notification_click(&self, click: Click) {
        let origin = { lock(&self.inner.notifications).origins.remove(&click.id) };
        let Some(origin) = origin else { return };
        if origin.peer == self.id() {
            self.perform_notification_action(origin.id, click.undo, self.id());
        } else if let Some(group) = self.inner.group.get()
            && group.peer_boot(origin.peer) == origin.boot
            && group
                .peer_agreement(origin.peer)
                .is_some_and(|a| a.contains(CAPABILITY))
        {
            let message = dto::NotifyAction::from_raw([(1, origin.id.write()), (2, click.undo.write())].into())
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
        let pending = { lock(&self.inner.notifications).pending.remove(&id) };
        let Some(p) = pending else { return };
        if let Some(local) = p.surface {
            let mut state = lock(&self.inner.notifications);
            state.origins.remove(&local);
            if let Some(overlay) = &state.overlay {
                overlay.dismiss(local);
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
                            overlay.dismiss(local);
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
                    ))
                };
                let Ok((id, key, level, mut title, description, persistent, undo, kind)) = read() else {
                    return;
                };
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
                let overlay = self.notification_overlay(&mut state);
                let local = overlay.push(Toast {
                    title,
                    description,
                    undo: undo.then(|| copy(language, 0).into()),
                    persistent,
                    scale: notification::scale_for(&display),
                    display,
                });
                state.origins.insert(
                    local,
                    Origin {
                        level,
                        expires: (!persistent).then(|| Instant::now() + notification::LIFETIME),
                        peer,
                        boot: group.peer_boot(peer),
                        id,
                    },
                );
                drop(state);
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
            StatusEvent::GameMode(game) => (1, game.holder == self.id() && game.active),
            StatusEvent::FocusLock { holder } => (3, *holder == Some(self.id())),
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
            if *current == on {
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
        if let Some(id) = dismiss {
            self.dismiss_notification(id);
        }
        if !on && kind != 1 {
            return;
        }
        let notice = if kind == 1 && !on { 2 } else { kind };
        let action = match notice {
            1 => Some(Undo::GameOff),
            3 => Some(Undo::PointerUnlock),
            4 => Some(Undo::KeyboardUnlock),
            5 => Some(Undo::MouseEnable),
            _ => None,
        };
        let request = Notification {
            level: NotificationLevel::Notice,
            title: copy(self.notification_language(), notice).into(),
            description: String::new(),
            screen: None,
            persistent: notice >= 3,
        };
        if let Ok(Some(receipt)) = self.raise_notification(request, action, notice)
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
fn copy(language: crate::config::UiLanguage, kind: u8) -> &'static str {
    use crate::config::UiLanguage::*;
    let index = usize::from(kind.min(5));
    match language {
        SimplifiedChinese => [
            "撤销",
            "已进入游戏模式",
            "已退出游戏模式",
            "已锁定指针",
            "已禁用键盘",
            "已禁用鼠标",
        ][index],
        Japanese => [
            "元に戻す",
            "ゲームモードが有効になりました",
            "ゲームモードが無効になりました",
            "ポインターをロックしました",
            "キーボードを無効にしました",
            "マウスを無効にしました",
        ][index],
        Korean => [
            "실행 취소",
            "게임 모드가 켜졌습니다",
            "게임 모드가 꺼졌습니다",
            "포인터가 잠겼습니다",
            "키보드가 비활성화되었습니다",
            "마우스가 비활성화되었습니다",
        ][index],
        _ => [
            "Undo",
            "Entered game mode",
            "Left game mode",
            "Pointer locked",
            "Keyboard disabled",
            "Mouse disabled",
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
        let home = tempfile::tempdir().unwrap();
        let node = Node::start(options(home.path())).await.unwrap();
        let n = node.clone();
        tokio::task::spawn_blocking(move || {
            n.edit_device_option(n.id().into(), dto::LocalOption::GameModePreference, 1u32.write())
        })
        .await
        .unwrap()
        .unwrap();
        let receipt = node
            .raise_notification(request(), Some(Undo::GameOff), 1)
            .unwrap()
            .unwrap();
        let id = receipt.id.parse().unwrap();
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
    async fn persistent_lock_body_dismiss_and_undo_have_distinct_effects() {
        use crate::snapshot_tests::until;
        let home = tempfile::tempdir().unwrap();
        let node = Node::start(options(home.path())).await.unwrap();
        node.set_focus_locked(true).unwrap();
        until("pointer lock notice", || {
            lock(&node.inner.notifications).pointer_id.is_some()
        })
        .await;
        let id = lock(&node.inner.notifications).pointer_id.unwrap();
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
        assert!(!node.settings().keyboard_disabled);
        node.shutdown().await;
    }
    #[tokio::test]
    async fn ordered_cause_release_dismisses_only_its_banner_and_stale_undo() {
        let home = tempfile::tempdir().unwrap();
        let node = Node::prepare(options(home.path())).unwrap();
        let keyboard = StatusEvent::KeyboardDisabled(true);
        let pointer = StatusEvent::FocusLock {
            holder: Some(node.id()),
        };
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
        node.notification_status(&StatusEvent::FocusLock { holder: None });
        assert!(!lock(&node.inner.notifications).pending.contains_key(&pointer_id));
        node.notification_status(&StatusEvent::KeyboardDisabled(false));
        assert!(lock(&node.inner.notifications).pending.is_empty());
        assert!(lock(&node.inner.notifications).origins.is_empty());
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
                until("remote keyboard undo", || !a.settings().keyboard_disabled).await;
                assert!(!AgentConfig::load_from(a.data_dir()).unwrap().settings.keyboard_disabled);
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
                assert!(owner.settings().keyboard_disabled);
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
                    || !owner.settings().keyboard_disabled && lock(&owner.inner.notifications).keyboard_id.is_none(),
                )
                .await;
                assert!(
                    !AgentConfig::load_from(home_b.path())
                        .unwrap()
                        .settings
                        .keyboard_disabled
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
                })
                .await;
                assert!(lock(&requester.inner.notifications).origins.is_empty());
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
        owner.notification_status(&StatusEvent::FocusLock {
            holder: Some(owner.id()),
        });
        let id = lock(&owner.inner.notifications).pointer_id.unwrap();
        assert_only_owner(&requester, &owner, id, Undo::PointerUnlock);
        owner.dismiss_notification(id);
        owner.notification_status(&StatusEvent::FocusLock { holder: None });
        let mut game = owner.game_status();
        game.holder = owner.id();
        game.active = true;
        owner.notification_status(&StatusEvent::GameMode(game));
        let id = lock(&owner.inner.notifications).game_id.unwrap();
        assert_only_owner(&requester, &owner, id, Undo::GameOff);
        let mut game = owner.game_status();
        game.holder = owner.id();
        game.active = false;
        owner.notification_status(&StatusEvent::GameMode(game));
        {
            let state = lock(&owner.inner.notifications);
            assert_eq!(state.pending.len(), 1, "game exit replaces the entry notice");
            let leave = state.pending.values().next().unwrap();
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
