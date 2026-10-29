//! 输入路由：全局唯一的指针与焦点、焦点移交、防卡键、修饰键合并、手感换算。
//!
//! 实现为与系统和网络无关的状态机（输入事件、对端消息、定时器 → 注入、发送、模式切换），
//! Routing decisions can be exercised with multiple simulated devices.
//!
//! - [`Router`]：每台设备一个的状态机；
//! - [`LayoutQuery`]：路由对「屏幕与布局」的需求；[`TopologyLayout`] / [`LayoutModel`] 适配
//!   `oink_display::layout`（正式实现），[`SimpleLayout`] 为测试用的简单实现；
//! - [`RouterConfig`] / [`RoutingSettings`]：本机身份与用户设置；
//! - [`hotkey`]：全局快捷键的解析与识别。

pub mod application_focus;
pub mod circle;
pub mod config;
pub mod contacts;
pub mod control;
pub mod damping;
pub mod display_layout;
pub mod dnd;
pub mod hotkey;
pub mod input;
pub mod layout;
pub mod negotiation;
pub mod router;
pub mod simple_layout;

pub use config::{ButtonHandoff, Os, RouterConfig, RoutingSettings};
pub use display_layout::{LayoutModel, TopologyLayout};
pub use hotkey::{Chord, ChordError, HotkeyBinding, Hotkeys, Mods};
pub use layout::{LayoutQuery, ScreenId, ScreenPos, StepResult};
pub use router::keyboard::{KEYBOARD_RELEASE_CHORD, KEYBOARD_RELEASE_HOLD_US};
pub use router::{
    ApplicationFocusCommand, Delivery, DndCmd, DndEvent, DndIntegrity, DndPreparation, DndSourceEffect, DndTarget,
    FocusInfo, FocusLockError, GameMode, GameModeStatus, GroupFocusStatus, Micros, Output, PointerCaptureObservation,
    Router, StatusEvent,
};
pub use simple_layout::{SimpleLayout, SimpleScreen};

/// Input-owner domain packets. These are never serialized by a generic wire codec.
#[derive(Clone, Debug, PartialEq)]
pub enum Packet {
    Input(input::InputMsg),
    Control(control::ControlMsg),
    Dnd(dnd::DndMsg),
    TimedInput {
        time: oink_foundation::input::CaptureTime,
        input: input::InputMsg,
    },
}
