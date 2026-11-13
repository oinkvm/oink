//! Read and synthesize system media/brightness keys and touchpad gesture events.
//!
//! **系统定义事件**（`NX_SYSDEFINED`，CGEvent 类型 14，subtype 8 = `NX_SUBTYPE_AUX_CONTROL_BUTTONS`）：键类型、
//! 按下 / 抬起、重复标志在 `data1` 里（`keymap::AuxKey`）。CGEvent 没有公开的字段读 subtype / data1，
//! 直接读写 CGEvent 私有字段 83 / 149 / 150（subtype / data1 / data2），不进入 AppKit。
//! `+[NSEvent eventWithCGEvent:]` 会处理 Caps Lock 输入源切换，在 tap 线程触发 HIToolbox 主队列断言。
//! 这些字段不能回退到 NSEvent 转换。
//! 原 NSEvent 合成路径于 2026-09-29 实测音量、亮度生效；这里保留相同的 subtype / data1 / data2、
//! flags 与 HID 投递位置。新旧字段于 macOS 26.6.2 / 14.7.8 对照一致；实体媒体键转发另行验收。
//!
//! **手势**：WindowServer 把触摸板识别好的手势作为私有类型的 CGEvent 分发，字段号是私有的
//! （`kCGSEventTypeField` = 55、`kCGEventGestureHIDType` = 110 …；开源的 Mac Mouse Fix 用同样的字段模拟
//! 调度中心与切换 Space，这里只参考它的机制，代码独立编写）：
//! - Dock 轻扫：类型 30（`kCGSEventDockControl`），HID 类型 23（`kIOHIDEventTypeDockSwipe`），
//!   轻扫种类（123、165：1 左右、2 上下、3 捏合）、阶段（132、134：`IOHIDEventPhaseBits`）、累计进度（124，
//!   135 是它的 float32 位）、结束速度（129、130）。本机实录的原生触摸板（2026-11-12，1290 帧，
//!   local validation records）：每帧前面紧挨一个
//!   **同一时间戳**的类型 29 伴随事件（先 29 后 30）；136 恒为 1；129 = 130 = 结束速度（Began/Changed 为 0）；
//!   终帧 115 = 0。Dock 只读 129 作速度（三个轴都是），136 第 0 位为 0 时把手势反向
//!   （`DOCKGestureEvent`：reversed = 136 & 1 == 0，按 123、124 符号和 136 选手势）。
//!   线上方向：左右为正 = 手指向左，竖直为负 = 向上（调度中心），捏合为负 = 捏合（Apps）、正 = 张开
//!   （显示桌面）。重放一律 136=1：竖直原始进度 = 线上进度，左右与捏合取反（进度和速度一起）；这样
//!   手机的方向选中的 Dock 手势与以前（左右 136=0、原样）相同。截获做逆运算，136=0 的来源先整体取反
//!   成 136=1 形式。所以 Mac → Mac 的重放与原生帧逐字段一致（115 的离散方向脉冲除外：未生成，恒为 0）。
//! - 捏合、旋转、智能缩放：类型 29（`kCGSEventGesture`），HID 类型 8（缩放，113 = 放大量）、5（旋转，
//!   114 = 角度）、22（智能缩放）。
//!
//! 合成的事件都带本项目的注入标记（`kCGEventSourceUserData` = `MAGIC`），截获回调据此跳过。

use objc2_core_foundation::CFRetained;
use objc2_core_graphics::{CGEvent, CGEventField, CGEventFlags, CGEventTapLocation, CGEventType};
use oink_foundation::input::{Gesture, GesturePhase, SwipeAxis};

use super::inject::MAGIC;
use super::keymap::{AuxKey, NX_SUBTYPE_AUX_CONTROL_BUTTONS};

/// CGEvent 类型：系统定义事件。
pub(crate) const TYPE_SYSDEFINED: u32 = 14;
/// CGEvent 类型：手势（`kCGSEventGesture`，= `NSEventTypeGesture`）。
pub(crate) const TYPE_GESTURE: u32 = 29;
/// CGEvent 类型：Dock 控制（`kCGSEventDockControl`）。
pub(crate) const TYPE_DOCK_CONTROL: u32 = 30;

/// 私有字段号（见模块说明）。
mod field {
    pub const EVENT_TYPE: u32 = 55;
    pub const SUBTYPE: u32 = 83;
    pub const DATA1: u32 = 149;
    pub const DATA2: u32 = 150;
    pub const HID_TYPE: u32 = 110;
    pub const ZOOM: u32 = 113;
    pub const ROTATION: u32 = 114;
    pub const SWIPE_MOTION: u32 = 123;
    pub const SWIPE_PROGRESS: u32 = 124;
    pub const SWIPE_EXIT_VX: u32 = 129;
    pub const SWIPE_EXIT_VY: u32 = 130;
    pub const PHASE: u32 = 132;
    pub const SWIPE_PHASE: u32 = 134;
    pub const SWIPE_PROGRESS_F32: u32 = 135;
    pub const SWIPE_INVERTED_FROM_DEVICE: u32 = 136;
    /// Dock 轻扫的离散方向脉冲（Dock 读作 movedEnough；缩放 / 旋转事件里同号字段是别的载荷）。
    pub const SWIPE_DISCRETE_MOTION: u32 = 115;
    pub const SWIPE_MOTION_2: u32 = 165;
}

/// `IOHIDEventType`。
mod hid_type {
    pub const ROTATION: i64 = 5;
    pub const ZOOM: i64 = 8;
    pub const ZOOM_TOGGLE: i64 = 22;
    pub const DOCK_SWIPE: i64 = 23;
}

/// `IOHIDEventPhaseBits`。
mod phase_bits {
    pub const BEGAN: i64 = 1;
    pub const CHANGED: i64 = 2;
    pub const ENDED: i64 = 4;
    pub const CANCELLED: i64 = 8;
}

/// 读取系统定义事件里的辅助键（媒体键、亮度键等）。不是 `NX_SUBTYPE_AUX_CONTROL_BUTTONS` 时为 `None`。
pub(crate) fn read_aux_key(ev: &CGEvent) -> Option<AuxKey> {
    // DATA1/DATA2 access asserts for some other system subtypes; always gate first.
    if CGEvent::r#type(Some(ev)).0 != TYPE_SYSDEFINED
        || CGEvent::integer_value_field(Some(ev), CGEventField(field::SUBTYPE))
            != i64::from(NX_SUBTYPE_AUX_CONTROL_BUTTONS)
    {
        return None;
    }
    AuxKey::parse(CGEvent::integer_value_field(Some(ev), CGEventField(field::DATA1)))
}

/// Create without AppKit, including on the injection worker. Keep the same flags/data2 as NSEvent.
fn create_aux_key(key: AuxKey) -> Option<CFRetained<CGEvent>> {
    let ev = CGEvent::new(None)?;
    CGEvent::set_type(Some(&ev), CGEventType(TYPE_SYSDEFINED));
    CGEvent::set_flags(Some(&ev), CGEventFlags(if key.pressed { 0xA00 } else { 0xB00 }));
    for (field, value) in [
        (field::SUBTYPE, i64::from(NX_SUBTYPE_AUX_CONTROL_BUTTONS)),
        (field::DATA1, key.data1()),
        (field::DATA2, -1),
    ] {
        CGEvent::set_integer_value_field(Some(&ev), CGEventField(field), value);
    }
    CGEvent::set_integer_value_field(Some(&ev), CGEventField::EventSourceUserData, MAGIC);
    Some(ev)
}

/// 投递一个辅助键事件（系统像实体键一样处理：调节音量 / 亮度并弹出提示、控制播放）。
pub(crate) fn post_aux_key(key: AuxKey) -> bool {
    post_aux_key_with_flags(key, 0)
}

/// Carry the source modifiers on the system-defined event itself. Injecting modifier
/// keystrokes would disturb the exit device's foreground application. The existing
/// NX subtype/data encoding is unchanged; Option+Shift uses normal CGEvent flags.
pub(crate) fn post_aux_key_with_flags(key: AuxKey, modifiers: u64) -> bool {
    let Some(ev) = create_aux_key(key) else {
        return false;
    };
    CGEvent::set_flags(Some(&ev), CGEventFlags(CGEvent::flags(Some(&ev)).0 | modifiers));
    CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&ev));
    true
}

/// Preserve the native Caps gesture, including its hold interval. HIToolbox owns
/// the long-press transition to ABC + lock and the return to the previous input
/// source when unlocking. Merely setting the IOKit lock leaves Pinyin selected:
/// on macOS 14.7.8 it turns even AlphaShift-tagged A/B/C into lowercase text.
/// A short pair posted only on release also loses this long-press state. Native
/// DOWN/UP restores it without TIS calls, synthetic waits or foreground changes.
/// Ordinary lock mode and source capture remain separate. Verified against the
/// native NX path on macOS 26.6.2; the physical-keyboard reference also enters ABC.
pub(crate) fn post_caps_transition(pressed: bool) -> bool {
    post_aux_key(AuxKey {
        key: 4, // NX_KEYTYPE_CAPS_LOCK, IOKit/hidsystem/ev_keymap.h
        pressed,
        repeat: false,
    })
}

fn phase_from_bits(bits: i64) -> Option<GesturePhase> {
    Some(match bits {
        phase_bits::BEGAN => GesturePhase::Began,
        phase_bits::CHANGED => GesturePhase::Changed,
        phase_bits::ENDED => GesturePhase::Ended,
        phase_bits::CANCELLED => GesturePhase::Cancelled,
        // MayBegin（128，手指放上还没动）与未知的值不上报。
        _ => return None,
    })
}

fn phase_to_bits(p: GesturePhase) -> i64 {
    match p {
        GesturePhase::Began => phase_bits::BEGAN,
        GesturePhase::Changed => phase_bits::CHANGED,
        GesturePhase::Ended => phase_bits::ENDED,
        GesturePhase::Cancelled => phase_bits::CANCELLED,
    }
}

/// 从手势类事件的字段翻译出手势（纯函数：`int` / `dbl` 读字段）。不是能转发的手势时为 `None`
/// （例如只携带触点的手势事件、Dock 控制事件后面的伴随事件、`MayBegin`）。
pub(crate) fn parse_gesture(ty: u32, int: impl Fn(u32) -> i64, dbl: impl Fn(u32) -> f64) -> Option<Gesture> {
    let hid = int(field::HID_TYPE);
    match (ty, hid) {
        (TYPE_DOCK_CONTROL, hid_type::DOCK_SWIPE) => {
            let axis = match int(field::SWIPE_MOTION) {
                1 => SwipeAxis::Horizontal,
                2 => SwipeAxis::Vertical,
                3 => SwipeAxis::Pinch,
                _ => return None,
            };
            let phase = phase_from_bits(int(field::PHASE))?;
            // Dock reads the velocity from 129 for every axis.
            let velocity = if phase.is_end() { dbl(field::SWIPE_EXIT_VX) } else { 0.0 };
            // 136 bit 0 clear makes Dock reverse the gesture: take it to the 136 = 1 form first,
            // then to the wire's direction.
            let natural = if int(field::SWIPE_INVERTED_FROM_DEVICE) & 1 == 1 {
                1.0
            } else {
                -1.0
            };
            let direction = natural * wire_direction(axis);
            Some(Gesture::DockSwipe {
                axis,
                phase,
                progress: dbl(field::SWIPE_PROGRESS) * direction,
                velocity: velocity * direction,
            })
        }
        (TYPE_GESTURE, hid_type::ZOOM) => Some(Gesture::Magnify {
            phase: phase_from_bits(int(field::PHASE))?,
            amount: dbl(field::ZOOM),
        }),
        (TYPE_GESTURE, hid_type::ROTATION) => Some(Gesture::Rotate {
            phase: phase_from_bits(int(field::PHASE))?,
            degrees: dbl(field::ROTATION),
        }),
        (TYPE_GESTURE, hid_type::ZOOM_TOGGLE) => Some(Gesture::SmartMagnify),
        _ => None,
    }
}

/// The wire's direction against the raw progress in the 136 = 1 form (the native trackpad's):
/// vertical keeps it; horizontal and pinch are the opposite. Derived from Dock's gesture choice
/// (`DOCKGestureEvent` / the direction table in REPORT §3) so that the phone's directions keep
/// the Dock gestures they had when horizontal was replayed with 136 = 0 and the wire's sign.
fn wire_direction(axis: SwipeAxis) -> f64 {
    match axis {
        SwipeAxis::Vertical => 1.0,
        SwipeAxis::Horizontal | SwipeAxis::Pinch => -1.0,
    }
}

/// 一个待投递的事件：类型与字段（`true` = 按 double 写）。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Synth {
    pub ty: u32,
    pub fields: Vec<(u32, f64, bool)>,
}

/// 手势 → 要投递的事件序列（纯函数，单元测试核对字段）。最后一帧是手势本身；Dock 轻扫前面是
/// 同一时间戳的类型 29 伴随事件（原生顺序）。
pub(crate) fn synth_gesture(g: &Gesture) -> Vec<Synth> {
    let i = |f: u32, v: i64| (f, v as f64, false);
    let d = |f: u32, v: f64| (f, v, true);
    match *g {
        Gesture::DockSwipe {
            axis,
            phase,
            progress,
            velocity,
        } => {
            let motion = match axis {
                SwipeAxis::Horizontal => 1,
                SwipeAxis::Vertical => 2,
                SwipeAxis::Pinch => 3,
            };
            // The native trackpad's form: 136 = 1 on every axis, the raw progress and velocity in
            // its direction (see `wire_direction`).
            let direction = wire_direction(axis);
            let progress = progress * direction;
            // Began / Changed carry no velocity; the end carries the same value in 129 and 130.
            let velocity = if phase.is_end() { velocity * direction } else { 0.0 };
            let bits = phase_to_bits(phase);
            let fields = vec![
                i(field::EVENT_TYPE, i64::from(TYPE_DOCK_CONTROL)),
                i(field::HID_TYPE, hid_type::DOCK_SWIPE),
                i(field::PHASE, bits),
                i(field::SWIPE_PHASE, bits),
                d(field::SWIPE_PROGRESS, progress),
                i(field::SWIPE_PROGRESS_F32, i64::from((progress as f32).to_bits())),
                i(field::SWIPE_MOTION, motion),
                i(field::SWIPE_MOTION_2, motion),
                i(field::SWIPE_INVERTED_FROM_DEVICE, 1),
                d(field::SWIPE_EXIT_VX, velocity),
                d(field::SWIPE_EXIT_VY, velocity),
                // The driver's discrete motion pulse (Dock's movedEnough). It is 0 on every end
                // frame; the pulses of some Changed frames are not generated (their integrator's
                // input is not recovered), so it is 0 throughout.
                i(field::SWIPE_DISCRETE_MOTION, 0),
            ];
            // The native trackpad sends each Dock frame right after a type 29 event with the same
            // timestamp (`post_gesture` gives them one).
            vec![
                Synth {
                    ty: TYPE_GESTURE,
                    fields: vec![i(field::EVENT_TYPE, i64::from(TYPE_GESTURE))],
                },
                Synth {
                    ty: TYPE_DOCK_CONTROL,
                    fields,
                },
            ]
        }
        Gesture::Magnify { phase, amount } => vec![Synth {
            ty: TYPE_GESTURE,
            fields: vec![
                i(field::EVENT_TYPE, i64::from(TYPE_GESTURE)),
                i(field::HID_TYPE, hid_type::ZOOM),
                i(field::PHASE, phase_to_bits(phase)),
                d(field::ZOOM, amount),
            ],
        }],
        Gesture::Rotate { phase, degrees } => vec![Synth {
            ty: TYPE_GESTURE,
            fields: vec![
                i(field::EVENT_TYPE, i64::from(TYPE_GESTURE)),
                i(field::HID_TYPE, hid_type::ROTATION),
                i(field::PHASE, phase_to_bits(phase)),
                d(field::ROTATION, degrees),
            ],
        }],
        Gesture::SmartMagnify => vec![Synth {
            ty: TYPE_GESTURE,
            fields: vec![
                i(field::EVENT_TYPE, i64::from(TYPE_GESTURE)),
                i(field::HID_TYPE, hid_type::ZOOM_TOGGLE),
            ],
        }],
    }
}

/// 投递合成的手势事件（位置取当前光标，应用内手势发给光标下的窗口）。一次手势的各帧先全部建好，
/// 用同一个时间戳按顺序投递（原生 Dock 帧与它前面的类型 29 事件同一时间戳）；建不出来就一帧也不投。
pub(crate) fn post_gesture(g: &Gesture) -> bool {
    let mut events = Vec::new();
    for (frame, s) in synth_gesture(g).into_iter().enumerate() {
        let Some(ev): Option<CFRetained<CGEvent>> = CGEvent::new(None) else {
            tracing::warn!(?g, frame, "gesture injection: event allocation failed; nothing posted");
            return false;
        };
        CGEvent::set_type(Some(&ev), CGEventType(s.ty));
        for (f, v, dbl) in s.fields {
            if dbl {
                CGEvent::set_double_value_field(Some(&ev), CGEventField(f), v);
            } else {
                CGEvent::set_integer_value_field(Some(&ev), CGEventField(f), v as i64);
            }
        }
        CGEvent::set_integer_value_field(Some(&ev), CGEventField::EventSourceUserData, MAGIC);
        events.push((s.ty, ev));
    }
    let Some(timestamp) = events.first().map(|(_, ev)| CGEvent::timestamp(Some(ev))) else {
        return true;
    };
    for (frame, (event_type, ev)) in events.iter().enumerate() {
        let ev: &CGEvent = ev;
        CGEvent::set_timestamp(Some(ev), timestamp);
        tracing::debug!(
            ?g,
            frame,
            event_type,
            timestamp_ns = timestamp,
            "gesture injection: posting native frame (delivery unconfirmed)"
        );
        CGEvent::post(CGEventTapLocation::SessionEventTap, Some(ev));
    }
    true
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    use std::collections::HashMap;

    // Runs on libtest's worker, with no AppKit main loop and no event posting.
    #[test]
    fn aux_fields_roundtrip_on_background_thread() {
        assert!(!super::super::main_queue::is_main_thread());
        for key in [0, 1, 2, 3, 7, 16, 17, 18, 19, 20, 21, 22, 23, 4, 0xffff] {
            for pressed in [false, true] {
                for repeat in [false, true] {
                    let key = AuxKey { key, pressed, repeat };
                    let ev = create_aux_key(key).unwrap();
                    assert_eq!(read_aux_key(&ev), Some(key));
                    // Even when Command is held in the injector/system state,
                    // brightness must carry only the native NX direction flags.
                    assert_eq!(CGEvent::flags(Some(&ev)).0, if pressed { 0xa00 } else { 0xb00 });
                    assert_eq!(
                        CGEvent::integer_value_field(Some(&ev), CGEventField::EventSourceUserData),
                        MAGIC
                    );
                    assert_eq!(CGEvent::integer_value_field(Some(&ev), CGEventField(field::DATA2)), -1);
                    CGEvent::set_integer_value_field(Some(&ev), CGEventField(field::SUBTYPE), 7);
                    assert_eq!(read_aux_key(&ev), None);
                    CGEvent::set_integer_value_field(Some(&ev), CGEventField(field::SUBTYPE), 8);
                    CGEvent::set_type(Some(&ev), CGEventType::KeyDown);
                    assert_eq!(read_aux_key(&ev), None);
                }
            }
        }
    }

    fn fields_of(s: &Synth) -> (HashMap<u32, i64>, HashMap<u32, f64>) {
        let mut ints = HashMap::new();
        let mut dbls = HashMap::new();
        for &(f, v, d) in &s.fields {
            if d {
                dbls.insert(f, v);
            } else {
                ints.insert(f, v as i64);
            }
        }
        (ints, dbls)
    }

    /// 合成的事件再按截获的规则解析，得到原来的手势（Mac → Mac 重放不失真）。
    #[test]
    fn synthesized_gestures_parse_back() {
        let cases = [
            Gesture::DockSwipe {
                axis: SwipeAxis::Vertical,
                phase: GesturePhase::Changed,
                progress: -0.35,
                velocity: 0.0,
            },
            Gesture::DockSwipe {
                axis: SwipeAxis::Horizontal,
                phase: GesturePhase::Ended,
                progress: 0.8,
                velocity: 1.5,
            },
            Gesture::DockSwipe {
                axis: SwipeAxis::Pinch,
                phase: GesturePhase::Began,
                progress: 0.0,
                velocity: 0.0,
            },
            Gesture::Magnify {
                phase: GesturePhase::Changed,
                amount: 0.05,
            },
            Gesture::Rotate {
                phase: GesturePhase::Ended,
                degrees: -3.5,
            },
            Gesture::SmartMagnify,
        ];
        for g in cases {
            let synth = synth_gesture(&g);
            let main = synth.last().unwrap();
            let (ints, dbls) = fields_of(main);
            let parsed = parse_gesture(
                main.ty,
                |f| ints.get(&f).copied().unwrap_or(0),
                |f| dbls.get(&f).copied().unwrap_or(0.0),
            );
            assert_eq!(parsed, Some(g), "{synth:?}");
            // Dock 轻扫前面的伴随事件不是手势（不会重复上报）。
            for extra in &synth[..synth.len() - 1] {
                let (ints, dbls) = fields_of(extra);
                assert_eq!(
                    parse_gesture(
                        extra.ty,
                        |f| ints.get(&f).copied().unwrap_or(0),
                        |f| dbls.get(&f).copied().unwrap_or(0.0)
                    ),
                    None
                );
            }
        }
    }

    #[test]
    fn pinch_capture_normalizes_both_device_directions() {
        for inverted in [0, 1] {
            for canonical in [-0.7, 0.7] {
                for phase in [
                    GesturePhase::Began,
                    GesturePhase::Changed,
                    GesturePhase::Ended,
                    GesturePhase::Cancelled,
                ] {
                    let direction = if inverted == 1 { -1.0 } else { 1.0 };
                    let ints = HashMap::from([
                        (field::HID_TYPE, hid_type::DOCK_SWIPE),
                        (field::SWIPE_MOTION, 3),
                        (field::PHASE, phase_to_bits(phase)),
                        (field::SWIPE_INVERTED_FROM_DEVICE, inverted),
                    ]);
                    let raw_velocity = canonical * 6.0 * direction;
                    let g = parse_gesture(
                        TYPE_DOCK_CONTROL,
                        |f| ints.get(&f).copied().unwrap_or(0),
                        |f| match f {
                            field::SWIPE_PROGRESS => canonical * direction,
                            field::SWIPE_EXIT_VX => raw_velocity,
                            _ => 0.0,
                        },
                    );
                    assert_eq!(
                        g,
                        Some(Gesture::DockSwipe {
                            axis: SwipeAxis::Pinch,
                            phase,
                            progress: canonical,
                            velocity: if phase.is_end() { canonical * 6.0 } else { 0.0 }
                        })
                    );
                    use crate::win_actions::{GestureMapper, WinAction};
                    let actions = GestureMapper::default().map(&g.unwrap());
                    let expected = if phase == GesturePhase::Ended && canonical < 0.0 {
                        vec![WinAction::Shortcut(vec![oink_foundation::input::KeyCode::LEFT_GUI])]
                    } else {
                        // With no foreground Start/search surface, opening the
                        // pinch is a no-op. Non-ended phases never commit.
                        Vec::new()
                    };
                    assert_eq!(actions, expected);
                }
            }
        }
    }

    #[test]
    fn pinch_replay_uses_natural_direction_with_consistent_progress_and_velocity() {
        for progress in [-0.7, 0.7] {
            for phase in [
                GesturePhase::Began,
                GesturePhase::Changed,
                GesturePhase::Ended,
                GesturePhase::Cancelled,
            ] {
                let velocity = if phase.is_end() { progress * 6.0 } else { 0.0 };
                let g = Gesture::DockSwipe {
                    axis: SwipeAxis::Pinch,
                    phase,
                    progress,
                    velocity,
                };
                let synth = synth_gesture(&g);
                let (ints, dbls) = fields_of(synth.last().unwrap());
                assert_eq!(ints[&field::SWIPE_INVERTED_FROM_DEVICE], 1);
                assert_eq!(dbls[&field::SWIPE_PROGRESS], -progress);
                assert_eq!(
                    ints[&field::SWIPE_PROGRESS_F32],
                    i64::from((-progress as f32).to_bits())
                );
                if phase.is_end() {
                    assert_eq!(dbls[&field::SWIPE_EXIT_VX], -velocity);
                    assert_eq!(dbls[&field::SWIPE_EXIT_VY], -velocity);
                }
                assert_eq!(
                    parse_gesture(
                        TYPE_DOCK_CONTROL,
                        |f| ints.get(&f).copied().unwrap_or(0),
                        |f| dbls.get(&f).copied().unwrap_or(0.0)
                    ),
                    Some(g)
                );
            }
        }
    }

    #[test]
    fn dock_swipe_fields() {
        let s = synth_gesture(&Gesture::DockSwipe {
            axis: SwipeAxis::Vertical,
            phase: GesturePhase::Ended,
            progress: -1.0,
            velocity: -2.0,
        });
        assert_eq!(s.len(), 2);
        assert_eq!(s[1].ty, TYPE_DOCK_CONTROL);
        let (ints, dbls) = fields_of(&s[1]);
        assert_eq!(ints[&field::HID_TYPE], hid_type::DOCK_SWIPE);
        assert_eq!(ints[&field::PHASE], phase_bits::ENDED);
        assert_eq!(ints[&field::SWIPE_MOTION], 2);
        assert_eq!(ints[&field::SWIPE_PROGRESS_F32], i64::from((-1.0f32).to_bits()));
        assert_eq!(dbls[&field::SWIPE_EXIT_VX], -2.0);
        assert_eq!(dbls[&field::SWIPE_EXIT_VY], -2.0);
        assert_eq!(s[0].ty, TYPE_GESTURE);
    }

    /// The recorded native trackpad (macOS 26.6.2, 1290 Dock frames): every Dock frame comes
    /// right after a type 29 event whose gesture fields are all 0; 123 = 165, 132 = 134,
    /// 129 = 130 (0 before the end), 136 = 1 and 135 = float32(124) on every axis; 115 = 0 at
    /// the end.
    #[test]
    fn dock_frames_have_the_native_order_and_fields() {
        for axis in [SwipeAxis::Horizontal, SwipeAxis::Vertical, SwipeAxis::Pinch] {
            for phase in [
                GesturePhase::Began,
                GesturePhase::Changed,
                GesturePhase::Ended,
                GesturePhase::Cancelled,
            ] {
                let s = synth_gesture(&Gesture::DockSwipe {
                    axis,
                    phase,
                    progress: 0.6,
                    velocity: 3.0,
                });
                assert_eq!(
                    s.iter().map(|f| f.ty).collect::<Vec<_>>(),
                    [TYPE_GESTURE, TYPE_DOCK_CONTROL]
                );
                assert_eq!(s[0].fields, vec![(field::EVENT_TYPE, f64::from(TYPE_GESTURE), false)]);
                let (ints, dbls) = fields_of(&s[1]);
                assert_eq!(ints[&field::SWIPE_MOTION], ints[&field::SWIPE_MOTION_2]);
                assert_eq!(ints[&field::PHASE], ints[&field::SWIPE_PHASE]);
                assert_eq!(ints[&field::SWIPE_INVERTED_FROM_DEVICE], 1);
                assert_eq!(ints[&field::SWIPE_DISCRETE_MOTION], 0);
                assert_eq!(
                    ints[&field::SWIPE_PROGRESS_F32],
                    i64::from((dbls[&field::SWIPE_PROGRESS] as f32).to_bits())
                );
                let v = dbls[&field::SWIPE_EXIT_VX];
                assert_eq!(dbls[&field::SWIPE_EXIT_VY], v);
                assert_eq!(v == 0.0, !phase.is_end(), "{axis:?} {phase:?}");
                // Cancelled keeps its velocity (the driver sends it on a cancel too).
                if phase.is_end() {
                    assert_eq!(v.abs(), 3.0);
                }
            }
        }
    }

    /// Dock's gesture choice (`DOCKGestureEvent` and the direction table of the static analysis):
    /// axis 123, the progress's sign and 136 bit 0 pick the gesture id; 136 bit 0 clear swaps the
    /// pair and reverses the progress Dock works with.
    fn dock_choice(axis: i64, progress: f64, inverted: i64) -> (u8, f64) {
        let (positive, other) = match axis {
            1 => (3, 4),
            2 => (1, 2),
            _ => (6, 5),
        };
        let natural = inverted & 1 == 1;
        let id = if (progress > 0.0) == natural { positive } else { other };
        (id, if natural { progress } else { -progress })
    }

    /// Replaying with 136 = 1 on every axis keeps the Dock gesture each phone direction chose
    /// before, when horizontal went out with 136 = 0 and the wire's sign (vertical and pinch
    /// already used 136 = 1).
    #[test]
    fn replay_with_136_1_keeps_the_dock_gestures_of_the_phone_directions() {
        for (axis, motion) in [
            (SwipeAxis::Horizontal, 1),
            (SwipeAxis::Vertical, 2),
            (SwipeAxis::Pinch, 3),
        ] {
            for wire in [-0.7, 0.7] {
                let before = match axis {
                    SwipeAxis::Horizontal => dock_choice(motion, wire, 0),
                    SwipeAxis::Vertical => dock_choice(motion, wire, 1),
                    SwipeAxis::Pinch => dock_choice(motion, -wire, 1),
                };
                let s = synth_gesture(&Gesture::DockSwipe {
                    axis,
                    phase: GesturePhase::Changed,
                    progress: wire,
                    velocity: 0.0,
                });
                let (ints, dbls) = fields_of(&s[1]);
                let now = dock_choice(
                    ints[&field::SWIPE_MOTION],
                    dbls[&field::SWIPE_PROGRESS],
                    ints[&field::SWIPE_INVERTED_FROM_DEVICE],
                );
                assert_eq!(now, before, "{axis:?} {wire}");
            }
        }
    }

    /// A native Dock frame (recorded segment D16, a space switch) is read into the wire's
    /// direction and replayed field for field; a source with 136 = 0 reads the same gesture.
    #[test]
    fn native_dock_frames_replay_unchanged() {
        let ints = HashMap::from([
            (field::HID_TYPE, hid_type::DOCK_SWIPE),
            (field::SWIPE_MOTION, 1),
            (field::PHASE, phase_bits::ENDED),
            (field::SWIPE_INVERTED_FROM_DEVICE, 1),
        ]);
        let raw = |f: u32| match f {
            field::SWIPE_PROGRESS => 1.00579833984375,
            field::SWIPE_EXIT_VX | field::SWIPE_EXIT_VY => 1.09320068359375,
            _ => 0.0,
        };
        let g = parse_gesture(TYPE_DOCK_CONTROL, |f| ints.get(&f).copied().unwrap_or(0), raw).unwrap();
        let Gesture::DockSwipe { progress, velocity, .. } = g else {
            panic!("{g:?}")
        };
        assert_eq!((progress, velocity), (-1.00579833984375, -1.09320068359375));
        let (replayed_ints, replayed) = fields_of(&synth_gesture(&g)[1]);
        assert_eq!(replayed[&field::SWIPE_PROGRESS], raw(field::SWIPE_PROGRESS));
        assert_eq!(replayed[&field::SWIPE_EXIT_VX], raw(field::SWIPE_EXIT_VX));
        assert_eq!(replayed_ints[&field::SWIPE_INVERTED_FROM_DEVICE], 1);

        // The same gesture from a source whose frames carry 136 = 0 (both values reversed).
        let mut reversed = ints.clone();
        reversed.insert(field::SWIPE_INVERTED_FROM_DEVICE, 0);
        assert_eq!(
            parse_gesture(
                TYPE_DOCK_CONTROL,
                |f| reversed.get(&f).copied().unwrap_or(0),
                |f| -raw(f)
            ),
            Some(g)
        );
    }

    #[test]
    fn may_begin_and_touch_only_events_are_not_gestures() {
        let int = |ints: HashMap<u32, i64>| move |f: u32| ints.get(&f).copied().unwrap_or(0);
        // MayBegin（128）不上报。
        let ints = HashMap::from([(field::HID_TYPE, hid_type::ZOOM), (field::PHASE, 128)]);
        assert_eq!(parse_gesture(TYPE_GESTURE, int(ints), |_| 0.0), None);
        // 只携带触点的手势事件（HID 类型 0）。
        assert_eq!(parse_gesture(TYPE_GESTURE, |_| 0, |_| 0.0), None);
        // 类型 30 但不是 Dock 轻扫。
        let ints = HashMap::from([(field::HID_TYPE, hid_type::ZOOM), (field::PHASE, 2)]);
        assert_eq!(parse_gesture(TYPE_DOCK_CONTROL, int(ints), |_| 0.0), None);
    }
}
