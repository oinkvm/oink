//! Explicit projections between runtime preferences and their sole durable owner
//! values. Local JSON never stores a second copy of these fields.
use super::{
    Settings,
    owner::{OwnerStore, validate_value},
    preferences::*,
    store::{self, StoreError},
};
use oink_foundation::{
    input::{InputMode, KeyCode},
    input_transform as input,
};
use oink_protocol::v1::{
    RawMap, RawValue,
    dto::{self, LocalOption},
    schema::WireValue,
};

fn map(entries: impl IntoIterator<Item = (u32, RawValue)>) -> RawValue {
    RawValue::Map(entries.into_iter().collect())
}
fn list(entries: impl IntoIterator<Item = RawValue>) -> RawValue {
    RawValue::Array(entries.into_iter().collect())
}

pub(super) fn values(settings: &Settings) -> Vec<(LocalOption, RawValue)> {
    let r = &settings.routing;
    let a = &settings.audio;
    use LocalOption::*;
    vec![
        (
            MouseMode,
            (match r.mouse_mode {
                InputMode::Shared => 0u32,
                InputMode::Local => 1,
                InputMode::Disabled => 2,
            })
            .write(),
        ),
        (
            KeyboardMode,
            (match r.keyboard_mode {
                InputMode::Shared => 0u32,
                InputMode::Local => 1,
                InputMode::Disabled => 2,
            })
            .write(),
        ),
        (ClipboardEnabled, settings.clipboard_enabled.write()),
        (ForwardingEnabled, a.forwarding_enabled.write()),
        (ScreenShared, settings.screen_shared.write()),
        (
            ScreenPreferences,
            map([
                (
                    1,
                    list(settings.screen_preferences.screens.iter().map(|s| {
                        map([
                            (1, RawValue::Text(s.display.clone())),
                            (2, s.gain.write()),
                            (3, s.enabled.write()),
                            (4, s.remote_gain.write()),
                        ])
                    })),
                ),
                (2, settings.screen_preferences.device_gain.write()),
            ]),
        ),
        (
            SpeedMode,
            (match settings.speed_mode {
                super::SpeedMode::Physical => 0u32,
                super::SpeedMode::ScreenProportional => 1,
            })
            .write(),
        ),
        (CursorTrail, r.cursor_trail.write()),
        (CursorTrailDuration, settings.cursor_trail_duration_ms.write()),
        (RemoteScrollSpeed, r.remote_scroll_speed.write()),
        (InputTransform, transform(&settings.input_transform)),
        (
            SourceInputPolicy,
            map([
                (
                    1,
                    r.fixed_reference_display
                        .as_ref()
                        .map_or(RawValue::Null, |s| RawValue::Text(s.clone())),
                ),
                (
                    2,
                    (if r.button_handoff == oink_routing::ButtonHandoff::Block {
                        0u32
                    } else {
                        1
                    })
                    .write(),
                ),
                (3, r.forward_gestures.write()),
                (4, r.return_pointer_on_display_sleep.write()),
                (5, r.wake_displays_on_input.write()),
                (6, r.dnd.write()),
            ]),
        ),
        (
            ForwardingControls,
            map([
                (1, a.forwarding_volume_percent.write()),
                (2, a.forwarding_muted.write()),
            ]),
        ),
        (
            DeviceName,
            RawValue::Text(settings.device_name.clone().unwrap_or_else(super::system_device_name)),
        ),
        (
            GameModePreference,
            (match settings.game_mode {
                super::GameModePreference::Auto => 0u32,
                super::GameModePreference::On => 1,
                super::GameModePreference::Off => 2,
            })
            .write(),
        ),
        (AcceptContacts, settings.accept_contacts.write()),
        (TouchpadInjection, r.touchpad_injection.write()),
        (NativeMultifinger, r.native_multifinger.write()),
        (MacScrollFeel, r.mac_scroll_feel.write()),
        (AudioExitClaim, RawValue::Null),
        (MicrophoneShareClaim, RawValue::Null),
        (
            MicrophoneControls,
            map([
                (1, settings.microphone.volume_percent.write()),
                (2, settings.microphone.muted.write()),
            ]),
        ),
        (MicrophoneReceiveMuted, settings.microphone.receive_muted.write()),
        (
            MicrophoneInput,
            settings
                .microphone
                .input
                .as_ref()
                .map(|v| oink_protocol::v1::schema::Text::<512>(v.clone()))
                .write(),
        ),
        (SyncSystemArrangement, settings.sync_system_arrangement.write()),
    ]
}
fn keyboard(value: &input::KeyboardMapping) -> RawValue {
    let mut raw: RawMap = [
        (
            1,
            (match value.preset {
                input::KeyPreset::WinFeel => 0u32,
                input::KeyPreset::MacFeel => 1,
                input::KeyPreset::Custom => 2,
                input::KeyPreset::None => 3,
            })
            .write(),
        ),
        (
            2,
            list(
                value
                    .table
                    .iter()
                    .map(|k| map([(1, k.from.0.write()), (2, k.to.0.write())])),
            ),
        ),
    ]
    .into();
    if let Some(caps) = value.caps_tap_hold {
        raw.insert(3, map([(1, caps.tap.0.write()), (2, caps.hold.0.write())]));
    }
    if let Some(saved) = &value.saved_custom {
        let mut snapshot: RawMap = [
            (
                1,
                list(
                    saved
                        .table
                        .iter()
                        .map(|k| map([(1, k.from.0.write()), (2, k.to.0.write())])),
                ),
            ),
            (3, saved.caps_split.write()),
            (4, saved.allow_fn.write()),
        ]
        .into();
        if let Some(hold) = saved.caps_hold {
            snapshot.insert(2, hold.0.write());
        }
        raw.insert(4, RawValue::Map(snapshot));
    }
    RawValue::Map(raw)
}
fn target(value: &input::TargetTransform) -> RawValue {
    map([(1, value.reverse_wheel.write()), (2, keyboard(&value.keyboard))])
}
pub(crate) fn transform(value: &input::InputTransform) -> RawValue {
    map([
        (1, target(&value.macos)),
        (2, target(&value.windows)),
        (
            3,
            list(value.overrides.iter().map(|o| {
                let mut raw: RawMap = [(1, store::wire_device(o.target).write())].into();
                if let Some(value) = o.reverse_wheel {
                    raw.insert(2, value.write());
                }
                if let Some(value) = &o.keyboard {
                    raw.insert(3, keyboard(value));
                }
                RawValue::Map(raw)
            })),
        ),
    ])
}
fn read_keyboard(value: dto::KeyboardMapping) -> Result<input::KeyboardMapping, StoreError> {
    Ok(input::KeyboardMapping {
        preset: match value.preset()? {
            dto::KeyPreset::WinFeel => input::KeyPreset::WinFeel,
            dto::KeyPreset::MacFeel => input::KeyPreset::MacFeel,
            dto::KeyPreset::Custom => input::KeyPreset::Custom,
            dto::KeyPreset::None => input::KeyPreset::None,
            _ => return Err(StoreError::Invalid),
        },
        table: value
            .table()?
            .0
            .into_iter()
            .map(|k| {
                Ok(input::KeyMapping {
                    from: KeyCode(k.from()?),
                    to: KeyCode(k.to()?),
                })
            })
            .collect::<Result<_, StoreError>>()?,
        caps_tap_hold: value
            .caps_tap_hold()?
            .map(|c| {
                Ok::<_, StoreError>(input::CapsTapHold {
                    tap: KeyCode(c.tap()?),
                    hold: KeyCode(c.hold()?),
                })
            })
            .transpose()?,
        saved_custom: value
            .saved_custom()?
            .map(|saved| {
                Ok::<_, StoreError>(input::SavedCustomKeymap {
                    table: saved
                        .table()?
                        .0
                        .into_iter()
                        .map(|k| {
                            Ok(input::KeyMapping {
                                from: KeyCode(k.from()?),
                                to: KeyCode(k.to()?),
                            })
                        })
                        .collect::<Result<_, StoreError>>()?,
                    caps_hold: saved.caps_hold()?.map(KeyCode),
                    caps_split: saved.caps_split()?,
                    allow_fn: saved.allow_fn()?,
                })
            })
            .transpose()?,
    })
}
fn read_target(value: dto::TargetTransform) -> Result<input::TargetTransform, StoreError> {
    Ok(input::TargetTransform {
        reverse_wheel: value.reverse_wheel()?,
        keyboard: read_keyboard(value.keyboard()?)?,
    })
}
pub(crate) fn read_transform(value: dto::InputTransformValue) -> Result<input::InputTransform, StoreError> {
    validate_value(LocalOption::InputTransform, &value.write()).map_err(|_| StoreError::Invalid)?;
    Ok(input::InputTransform {
        macos: read_target(value.macos()?)?,
        windows: read_target(value.windows()?)?,
        overrides: value
            .overrides()?
            .0
            .into_iter()
            .map(|o| {
                Ok(input::TargetOverride {
                    target: store::device(&o.target()?.write())?,
                    reverse_wheel: o.reverse_wheel()?,
                    keyboard: o.keyboard()?.map(read_keyboard).transpose()?,
                })
            })
            .collect::<Result<_, StoreError>>()?,
    })
}
pub(super) fn apply(owner: &OwnerStore, settings: &mut Settings) -> Result<(), StoreError> {
    use LocalOption::*;
    for (option, _) in values(settings) {
        let value = owner.value(option).ok_or(StoreError::Invalid)?;
        validate_value(option, &value).map_err(|_| StoreError::Invalid)?;
        match option {
            MouseMode => {
                settings.routing.mouse_mode = match value.as_u64()? {
                    0 => InputMode::Shared,
                    1 => InputMode::Local,
                    _ => InputMode::Disabled,
                }
            }
            KeyboardMode => {
                settings.routing.keyboard_mode = match value.as_u64()? {
                    0 => InputMode::Shared,
                    1 => InputMode::Local,
                    _ => InputMode::Disabled,
                }
            }
            ClipboardEnabled => settings.clipboard_enabled = bool::read(&value)?,
            ForwardingEnabled => settings.audio.forwarding_enabled = bool::read(&value)?,
            ScreenShared => settings.screen_shared = bool::read(&value)?,
            ScreenPreferences => {
                let value = dto::ScreenPreferencesValue::read(&value)?;
                settings.screen_preferences = super::ScreenPreferences {
                    device_gain: value.device_gain()?,
                    screens: value
                        .screens()?
                        .0
                        .into_iter()
                        .map(|s| {
                            Ok(ScreenPreference {
                                remote_gain: s.remote_gain()?,
                                display: s.display()?.0,
                                gain: s.gain()?,
                                enabled: s.enabled()?,
                            })
                        })
                        .collect::<Result<_, StoreError>>()?,
                };
            }
            SpeedMode => {
                settings.speed_mode = if value.as_u64()? == 0 {
                    super::SpeedMode::Physical
                } else {
                    super::SpeedMode::ScreenProportional
                }
            }
            CursorTrail => settings.routing.cursor_trail = bool::read(&value)?,
            CursorTrailDuration => settings.cursor_trail_duration_ms = u32::read(&value)?,
            RemoteScrollSpeed => settings.routing.remote_scroll_speed = f64::read(&value)?,
            InputTransform => {
                settings.input_transform = super::read_transform(dto::InputTransformValue::read(&value)?)?
            }
            SourceInputPolicy => {
                let value = dto::SourceInputPolicyValue::read(&value)?;
                let r = &mut settings.routing;
                r.fixed_reference_display = value.reference_display()?.map(|s| s.0);
                r.button_handoff = match value.button_handoff()? {
                    dto::ButtonHandoff::Block => oink_routing::ButtonHandoff::Block,
                    _ => oink_routing::ButtonHandoff::Allow,
                };
                r.forward_gestures = value.forward_gestures()?;
                r.return_pointer_on_display_sleep = value.return_pointer_on_display_sleep()?;
                r.wake_displays_on_input = value.wake_displays_on_input()?;
                r.dnd = value.dnd_enabled()?;
            }
            ForwardingControls => {
                let value = dto::ForwardingControlsValue::read(&value)?;
                settings.audio.forwarding_volume_percent = value.volume_percent()?;
                settings.audio.forwarding_muted = value.muted()?;
            }
            DeviceName => {
                let RawValue::Text(name) = value else {
                    return Err(StoreError::Invalid);
                };
                settings.device_name = Some(name);
            }
            GameModePreference => {
                settings.game_mode = match value.as_u64()? {
                    0 => super::GameModePreference::Auto,
                    1 => super::GameModePreference::On,
                    _ => super::GameModePreference::Off,
                }
            }
            AcceptContacts => settings.accept_contacts = bool::read(&value)?,
            TouchpadInjection => settings.routing.touchpad_injection = bool::read(&value)?,
            NativeMultifinger => settings.routing.native_multifinger = bool::read(&value)?,
            MacScrollFeel => settings.routing.mac_scroll_feel = bool::read(&value)?,
            SyncSystemArrangement => settings.sync_system_arrangement = bool::read(&value)?,
            MicrophoneControls => {
                let value = dto::ForwardingControlsValue::read(&value)?;
                settings.microphone.volume_percent = value.volume_percent()?;
                settings.microphone.muted = value.muted()?;
            }
            MicrophoneReceiveMuted => settings.microphone.receive_muted = bool::read(&value)?,
            MicrophoneInput => {
                settings.microphone.input = Option::<oink_protocol::v1::schema::Text<512>>::read(&value)?.map(|v| v.0)
            }
            AudioExitClaim | MicrophoneShareClaim => {}
            _ => return Err(StoreError::Invalid),
        }
    }
    settings.routing.emergency_hold_ms = settings.emergency_hold_ms.get();
    settings.routing.event_frequency = settings.event_frequency;
    settings.routing.cursor_trail_duration_ms = settings.cursor_trail_duration_ms;
    settings.routing.accept_contacts = settings.accept_contacts;
    settings.routing.game_mode_preference = match settings.game_mode {
        super::GameModePreference::Auto => oink_routing::GameMode::Auto,
        super::GameModePreference::On => oink_routing::GameMode::On,
        super::GameModePreference::Off => oink_routing::GameMode::Off,
    };
    Ok(())
}

pub(super) fn local(settings: &Settings) -> serde_json::Value {
    let mut value = serde_json::to_value(settings).expect("settings serialization");
    // None means current defaults at runtime, not an explicit null preference.
    value["clipboard"] =
        serde_json::to_value(settings.clipboard.unwrap_or_else(crate::clip::default_settings)).unwrap();
    value["hotkeys"] = serde_json::to_value(crate::hotkeys::configured(settings.hotkeys.clone())).unwrap();
    let routing = value["routing"].as_object_mut().expect("routing object");
    for field in [
        "mouse_mode",
        "keyboard_mode",
        "fixed_reference_display",
        "button_handoff",
        "forward_gestures",
        "return_pointer_on_display_sleep",
        "wake_displays_on_input",
        "dnd",
        "remote_scroll_speed",
        "cursor_trail",
        "accept_contacts",
        "touchpad_injection",
        "native_multifinger",
        "mac_scroll_feel",
        "emergency_hold_ms",
    ] {
        routing.remove(field);
    }
    let audio = value["audio"].as_object_mut().expect("audio object");
    for field in [
        "enabled",
        "target",
        "forwarding_enabled",
        "forwarding_volume_percent",
        "forwarding_muted",
    ] {
        audio.remove(field);
    }
    value
}
