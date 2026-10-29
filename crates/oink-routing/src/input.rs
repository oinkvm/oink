//! Input-domain events. Wire conversion belongs to the input stream owner.
//! Ordered pointer motion and keys use reliable transport; game counts use datagrams.

use oink_foundation::input::{Gesture, KeyCode, MouseButton, RecentAudioNotifications, ScrollPhase, SourceKind};
use oink_foundation::session::Boot;
use serde::{Deserialize, Serialize};

/// The only key identities allowed in volume transition diagnostics.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VolumeKey {
    Up,
    Down,
    Mute,
}
impl VolumeKey {
    pub fn from_code(code: KeyCode) -> Option<Self> {
        match (code.page(), code.usage()) {
            (12, 0xe9) | (7, 0x80) => Some(Self::Up),
            (12, 0xea) | (7, 0x81) => Some(Self::Down),
            (12, 0xe2) | (7, 0x7f) => Some(Self::Mute),
            _ => None,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeKeyOrigin {
    pub physical: Option<bool>,
    pub source_kind: SourceKind,
    pub keep_local_playback: Option<bool>,
    pub windows_notifications: Option<RecentAudioNotifications>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum InputMsg {
    /// Terminal application delivery, carried by the existing key wire record.
    /// Always inside Forwarded so physical source Boot and business hops survive.
    ApplicationKey {
        route: crate::application_focus::keys::KeyRoute,
        code: KeyCode,
        physical_code: Option<KeyCode>,
        pressed: bool,
    },
    ApplicationRepeat {
        route: crate::application_focus::keys::KeyRoute,
        code: KeyCode,
        n: u32,
    },
    /// Release-only repair for the additional application ledger. The route's
    /// press ID is the snapshot watermark; each entry retains its press identity.
    ApplicationHeldSync {
        route: crate::application_focus::keys::KeyRoute,
        keys: Vec<(KeyCode, u64)>,
    },
    /// 指针位移，reference-plane distance（源机已加速的位移 × 参考屏 reference_per_native_unit）。
    /// Ordered；源端默认 125 Hz 额度补充（可选 Unlimited），增量合并；突发及保序刷新可超出此频率。
    /// `seq` 保留最新采样号，仅用于诊断。
    Motion {
        seq: u64,
        dx: f64,
        dy: f64,
    },
    /// 物理键按下/抬起（HID Usage）。目标路由负责修饰键合并与输入转换；自动重复见 [`InputMsg::KeyRepeat`]。
    /// 目标端对同一源重复的按下是幂等的：焦点进入时源机用它发送修饰键快照。
    Key {
        code: KeyCode,
        pressed: bool,
    },
    Button {
        button: MouseButton,
        pressed: bool,
    },
    /// 滚动，已换算为与源机屏幕无关的量（见 [`ScrollAmount`]）。方向约定同 `InputEvent::Scroll`。
    Scroll {
        source: SourceKind,
        dx: f64,
        dy: f64,
        unit: ScrollAmount,
        phase: ScrollPhase,
    },
    /// 按键状态同步：源机认为在接收方仍按着的键和按钮（随心跳发送，仅在非空或刚变空时）。
    /// 接收方释放为该源按下、但不在列表中的键和按钮；从不据此按下。
    HeldSync {
        keys: Vec<KeyCode>,
        buttons: Vec<MouseButton>,
    },
    /// 接收方已不是焦点机时，把消息转给它所知的焦点机。`origin` 为最初的源机，`hops` 为已转发次数。
    Forwarded {
        origin: Boot,
        hops: u8,
        msg: Box<InputMsg>,
    },
    /// 按键重复（reliable input event）：源机系统对按着的 `code` 产生了第 `n` 次自动重复
    /// （从 1 开始，每次按下重新计数）。重复只在源机产生、逐个转发，所以抬起晚到只会晚停，不会多出字符。
    /// 目标端只为仍按着的键注入一次重复，从不据此按下；`n` 不大于已注入的次数时忽略（重复投递）。
    KeyRepeat {
        code: KeyCode,
        n: u32,
    },
    /// 触摸板手势（reliable input event）：Mac 源机截获的系统手势，目标端重放或映射为快捷键
    /// A gesture stays on the same target from start to finish, just like a scroll gesture.
    Gesture {
        source: SourceKind,
        gesture: Gesture,
    },
    /// Cumulative unaccelerated counts for one game generation. Missing datagrams
    /// are recovered by the next total; old/reordered totals are discarded.
    RelativeMotion {
        epoch: u64,
        generation: u64,
        seq: u64,
        x: f64,
        y: f64,
    },
    /// Deliver volume directly to this device, regardless of focus. Never forward again.
    /// Only volume usages are accepted; modifiers are keyboard E0–E7 bits.
    Volume {
        route: VolumeRoute,
        press_id: u64,
        code: KeyCode,
        pressed: bool,
        repeat: bool,
        modifiers: u8,
    },
    /// Native gesture contacts; every phase uses reliable I0.
    TouchpadContacts(oink_foundation::contacts::Frame),
    /// Diagnostic context immediately before the volume press on Ordered I0.
    /// No input action; forwarding/repeat/release still use the original records.
    /// Carries source provenance to the focus that makes the final volume choice.
    VolumeKeyOrigin {
        action: VolumeKey,
        origin: VolumeKeyOrigin,
    },
    /// Source-resolved Caps tap/hold output, delivered only to this target. The
    /// physical held identity is CAPS_LOCK, not `code`; no target table or repeat.
    CapsKey {
        code: KeyCode,
        pressed: bool,
    },
}

/// 线上滚动量的单位。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScrollAmount {
    /// Source scroll pixels, without display-size or pointer-gain conversion.
    Pixels,
    /// Legacy reference-plane distance; encode/decode at the fixed canonical
    /// .25/686 per pixel, never a physical display calibration.
    Reference,
    /// 行：源机的行滚动 × 源机每格行数（macOS 源为 1）。
    Lines,
}

impl InputMsg {
    /// 是否走不可靠的低延迟通道（datagram）。
    pub fn is_datagram(&self) -> bool {
        match self {
            InputMsg::RelativeMotion { .. } => true,
            InputMsg::Forwarded { msg, .. } => msg.is_datagram(),
            _ => false,
        }
    }
}

/// The source-owned audio context captured once at a physical volume press.
#[derive(Clone, Debug, PartialEq)]
pub struct VolumeRoute {
    pub source: Boot,
    pub target: Boot,
    pub session: std::sync::Arc<oink_protocol::v1::dto::AudioSessionContext>,
}
impl VolumeRoute {
    /// Borrow retained fields without allocating a second DTO tree on capture.
    pub fn from_session(session: std::sync::Arc<oink_protocol::v1::dto::AudioSessionContext>) -> Option<Self> {
        use oink_protocol::v1::{RawMap, RawValue, schema::WireDto};
        fn map(value: &RawValue) -> Option<&RawMap> {
            if let RawValue::Map(value) = value {
                Some(value)
            } else {
                None
            }
        }
        fn boot(value: &RawValue) -> Option<Boot> {
            let value = map(value)?;
            let device = map(value.get(&1)?)?;
            if !matches!(device.get(&1),Some(RawValue::Text(algorithm)) if algorithm==oink_protocol::v1::dto::ED25519) {
                return None;
            }
            let (RawValue::Bytes(key), RawValue::Bytes(nonce)) = (device.get(&2)?, value.get(&2)?) else {
                return None;
            };
            Some(Boot {
                device_id: oink_foundation::DeviceId(key.as_slice().try_into().ok()?),
                nonce: nonce.as_slice().try_into().ok()?,
            })
        }
        let raw = session.raw();
        let source = boot(raw.get(&1)?)?;
        let target = boot(map(raw.get(&3)?)?.get(&1)?)?;
        Some(Self {
            source,
            target,
            session,
        })
    }
}
