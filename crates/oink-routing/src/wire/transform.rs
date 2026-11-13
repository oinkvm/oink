//! A member's published input transform (DeviceFacts preference
//! `LocalOption::InputTransform`) as the router's `InputTransform`. Only the
//! active mappings are read; editor memory (`saved_custom`) is not needed to
//! route keys.
use oink_foundation::input::KeyCode;
use oink_foundation::input_transform as t;
use oink_protocol::v2::{self, dto};

fn keyboard(value: dto::KeyboardMapping) -> v2::Result<t::KeyboardMapping> {
    Ok(t::KeyboardMapping {
        preset: match value.preset()? {
            dto::KeyPreset::WinFeel => t::KeyPreset::WinFeel,
            dto::KeyPreset::MacFeel => t::KeyPreset::MacFeel,
            dto::KeyPreset::Custom => t::KeyPreset::Custom,
            dto::KeyPreset::None => t::KeyPreset::None,
            _ => return Err(v2::ErrorCode::UnsupportedValue.into()),
        },
        table: value
            .table()?
            .0
            .into_iter()
            .map(|k| {
                Ok(t::KeyMapping {
                    from: KeyCode(k.from()?),
                    to: KeyCode(k.to()?),
                })
            })
            .collect::<v2::Result<_>>()?,
        caps_tap_hold: value
            .caps_tap_hold()?
            .map(|c| {
                Ok::<_, v2::Error>(t::CapsTapHold {
                    tap: KeyCode(c.tap()?),
                    hold: KeyCode(c.hold()?),
                })
            })
            .transpose()?,
        saved_custom: None,
    })
}

fn target(value: dto::TargetTransform) -> v2::Result<t::TargetTransform> {
    Ok(t::TargetTransform {
        reverse_wheel: value.reverse_wheel()?,
        keyboard: keyboard(value.keyboard()?)?,
    })
}

pub fn read(value: dto::InputTransformValue) -> v2::Result<t::InputTransform> {
    Ok(t::InputTransform {
        macos: target(value.macos()?)?,
        windows: target(value.windows()?)?,
        android: value.android()?.map(target).transpose()?,
        overrides: value
            .overrides()?
            .0
            .into_iter()
            .map(|o| {
                Ok(t::TargetOverride {
                    target: oink_protocol::v2::identity::read_device(&o.target()?)?,
                    reverse_wheel: o.reverse_wheel()?,
                    keyboard: o.keyboard()?.map(keyboard).transpose()?,
                })
            })
            .collect::<v2::Result<_>>()?,
    })
}

/// The transform a member publishes in its DeviceFacts, if any.
pub fn from_facts(facts: &dto::DeviceFacts) -> Option<t::InputTransform> {
    use oink_protocol::v2::schema::WireValue;
    for option in facts.preferences().ok()?.map_or(Vec::new(), |values| values.0) {
        if option.option().ok()? == dto::LocalOption::InputTransform {
            let raw = option.value().ok()?;
            let value = dto::InputTransformValue::read(&raw).ok()?;
            return read(value).ok();
        }
    }
    None
}
